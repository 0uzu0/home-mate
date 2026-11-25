//! The Core configuration publisher owns only local SQLite work. The supervisor awaits
//! blocking jobs outside the store lock and retries infrastructure failures.
use super::*;
use crate::{
    gateway::DeviceHub,
    store::Store,
    voice::{VoicePipeline, VoiceSettings, VOICE_CFG_KEY},
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

pub struct ConfigApplyCoordinator(tokio::task::JoinHandle<()>);

impl ConfigApplyCoordinator {
    /// Keep this handle for the Core lifetime. Dropping it stops supervision;
    /// any already-running local transaction still completes atomically.
    pub fn start(store: Arc<Store>) -> Self {
        Self::supervise(store, None, None, None)
    }

    pub fn start_with_voice(store: Arc<Store>, voice: Arc<VoicePipeline>) -> Self {
        Self::supervise(store, Some(voice), None, None)
    }

    pub fn start_for_app(state: &crate::api::AppState) -> Self {
        Self::supervise(
            state.store.clone(),
            Some(state.voice.clone()),
            Some(super::open_sockets::Provider::from_app(state)),
            Some(state.hub.clone()),
        )
    }

    fn supervise(
        store: Arc<Store>,
        voice: Option<Arc<VoicePipeline>>,
        open_sockets: Option<super::open_sockets::Provider>,
        hub: Option<Arc<DeviceHub>>,
    ) -> Self {
        Self(tokio::spawn(async move {
            loop {
                let store_c = store.clone();
                let voice_c = voice.clone();
                let open_sockets_c = open_sockets.clone();
                let hub_c = hub.clone();
                match tokio::task::spawn_blocking(move || {
                    run_with_runtime(
                        &store_c,
                        voice_c.as_ref(),
                        open_sockets_c.as_ref(),
                        hub_c.as_deref(),
                    )
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => {
                        tracing::warn!("configuration publication store unavailable; retrying")
                    }
                    Err(_) => {
                        tracing::warn!("configuration publication worker interrupted; restarting")
                    }
                }
                if let Some(hub) = hub.as_ref() {
                    if let Err(e) = run_gateway_jobs(&store, voice.as_ref(), hub).await {
                        tracing::warn!(error = %e.detail, "gateway config dispatch error");
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }))
    }
}
impl Drop for ConfigApplyCoordinator {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) const PERSONA: &str = "persona.agent_md.content";

/// A pending desired value must not be mistaken for an external legacy write.
/// Revision fencing happens at publication, not by deleting the desired value.
pub(super) fn expected_projection(
    c: &Connection,
    epoch: u64,
    block: &str,
) -> Result<Option<Value>> {
    let row: Option<(String,String)> = c.query_row(
        "SELECT o.payload_json,b.state FROM config_outbox o JOIN config_applications a USING(principal,application_id)
         JOIN config_application_blocks b USING(principal,application_id)
         WHERE o.data_epoch=?1 AND b.block=?2 AND json_type(o.payload_json,'$.' || b.block)='object'
         ORDER BY a.revision DESC,a.created_ms DESC LIMIT 1",
        params![epoch as i64,block], |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?;
    let Some((payload, state)) = row else {
        return Ok(None);
    };
    // Inspect the latest job before filtering state: falling back to an older
    // pending job could hide a legacy write after a newer successful publish.
    if !["saved_pending", "failed", "superseded"].contains(&state.as_str()) {
        return Ok(None);
    }
    let payload: Value = serde_json::from_str(&payload)?;
    if payload[block]["legacy"] == true {
        return Ok(None);
    }
    let same = if block == "persona" {
        payload[block]["expected_revision_id"] == revision_id(c)?
    } else {
        // Snapshot adoption compares each field with its captured projection.
        // Unrelated native metadata edits must not erase pending desired.
        // Publication still checks the complete raw JSON baseline.
        true
    };
    if !same {
        return Ok(None);
    }
    Ok(payload[block].get("expected").cloned())
}

#[derive(Clone)]
pub(super) struct Job {
    principal: String,
    id: String,
    epoch: u64,
    revision: u64,
    block: String,
    payload: Value,
}

pub(super) fn prepare(store: &Store) -> Result<Vec<Job>> {
    store
        .with(|c| {
            let mut stmt = c.prepare(
            "SELECT o.principal,o.application_id,o.data_epoch,a.revision,o.payload_json,a.after_json,b.block
             FROM config_outbox o JOIN config_applications a USING(principal,application_id)
             JOIN config_application_blocks b USING(principal,application_id)
             WHERE b.block IN ('persona','services','weather','home','music','news','search','openapi','mode','open_sockets','reflex','node','audio_eq','device_led_configs','unit_acoustic_profiles') AND b.state='saved_pending'
             ORDER BY a.revision LIMIT 64",
        )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)? as u64,
                        r.get::<_, i64>(3)? as u64,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .map_err(DeliveryError::internal)?
        .into_iter()
        .map(|(principal, id, epoch, revision, text, after, block)| {
            let mut payload: Value = serde_json::from_str(&text)?;
            if payload.get(&block).is_none() {
                if !matches!(block.as_str(), "persona" | "services") {
                    return Err(DeliveryError::internal("missing native block publication payload"));
                }
                // Earlier receipts projected synchronously. Verify only their
                // exact persisted persona, never infer a new installed default.
                let after: Value = serde_json::from_str(&after)?;
                let desired = if block=="persona" {after["fields"].get(PERSONA).cloned().unwrap_or(Value::Null)}
                    else {super::service::value_tree(&serde_json::from_value::<Fields>(after["fields"].clone())?,"services")};
                payload[&block] = json!({"expected":desired,"desired":desired,"legacy":true});
            }
            Ok(Job {
                principal,
                id,
                epoch,
                revision,
                block,
                payload,
            })
        })
        .collect()
}

pub(super) fn revision_id(c: &Connection) -> Result<i64> {
    Ok(c.query_row("SELECT COALESCE((SELECT id FROM agent_md_revisions ORDER BY version DESC,id DESC LIMIT 1),0)",[],|r|r.get(0))?)
}

fn content(c: &Connection) -> Result<String> {
    Ok(c.query_row(
        "SELECT content FROM agent_md_revisions ORDER BY version DESC,id DESC LIMIT 1",
        [],
        |r| r.get(0),
    )
    .optional()?
    .unwrap_or_default())
}

fn require_services_snapshot(value: &Value) -> Result<()> {
    for slot in ["llm", "llm_fallback", "asr", "tts"] {
        for field in [
            "provider",
            "base_url",
            "model",
            "api_key_env",
            "thinking",
            "voice",
        ] {
            if !value[slot][field].is_string() {
                return Err(DeliveryError::internal(
                    "incomplete schema-1 services snapshot",
                ));
            }
        }
    }
    Ok(())
}
fn baseline_matches(c: &Connection, job: &Job) -> Result<bool> {
    let block = &job.payload[&job.block];
    if job.block == "persona" {
        Ok(content(c)? == block["expected"]
            && (block["legacy"] == true || block["expected_revision_id"] == revision_id(c)?))
    } else if job.block == "services" {
        require_services_snapshot(&block["desired"])?;
        if block["legacy"] == true {
            let Some(raw) = super::service::raw(c, "services")? else {
                return Ok(false);
            };
            let native: crate::config::ServicesConfig = serde_json::from_str(&raw)?;
            Ok(serde_json::to_value(native)? == block["expected"])
        } else {
            Ok(serde_json::to_value(super::service::raw(c, "services")?)? == block["expected_raw"])
        }
    } else {
        let storage_key = if job.block == "mode" {
            VOICE_CFG_KEY
        } else {
            &job.block
        };
        Ok(serde_json::to_value(super::service::raw(c, storage_key)?)? == block["expected_raw"])
    }
}

fn database<T>(store: &Store, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    store.with(|c| Ok(f(c))).map_err(DeliveryError::internal)?
}

#[cfg(test)]
pub(super) fn publish(store: &Store, job: Job) -> Result<()> {
    publish_transaction(store, &job, None, None).map(|_| ())
}

fn publish_transaction(
    store: &Store,
    job: &Job,
    current_mode: Option<&VoiceSettings>,
    hub: Option<&DeviceHub>,
) -> Result<(Option<VoiceSettings>, bool)> {
    let _openapi_gate = (job.block == "openapi")
        .then(|| crate::lock_or_recover(&store.openapi_gate, "openapi.publication"));
    let attempt = database(store, |c| {
        let tx = rusqlite::Transaction::new_unchecked(c, rusqlite::TransactionBehavior::Immediate)?;
        let (revision, epoch, node): (i64, i64, String) = tx.query_row(
            "SELECT revision,data_epoch,node_id FROM config_head WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let state: Option<String> = tx.query_row("SELECT state FROM config_application_blocks WHERE principal=?1 AND application_id=?2 AND block=?3",params![job.principal,job.id,job.block],|r|r.get(0)).optional()?;
        if state.as_deref() != Some("saved_pending") {
            return Ok((None, false));
        }
        let mut next_mode = None;
        let mut applied = false;
        if revision as u64 != job.revision || epoch as u64 != job.epoch {
            finish(&tx, job, "superseded", None)?;
        } else if job.block == "open_sockets"
            && !super::open_sockets::live_node_matches(&tx, &node)?
        {
            finish(
                &tx,
                job,
                "failed",
                Some("OPEN_SOCKETS_ENROLLED_NODE_MISMATCH"),
            )?;
        } else if !baseline_matches(&tx, job)? {
            finish(
                &tx,
                &job,
                "failed",
                Some(match job.block.as_str() {
                    "persona" => "PERSONA_BASELINE_CHANGED",
                    "services" => "SERVICES_BASELINE_CHANGED",
                    "weather" => "WEATHER_BASELINE_CHANGED",
                    "news" => "NEWS_BASELINE_CHANGED",
                    "home" => "HOME_BASELINE_CHANGED",
                    "music" => "MUSIC_BASELINE_CHANGED",
                    "openapi" => "OPENAPI_BASELINE_CHANGED",
                    "mode" => "VOICE_BASELINE_CHANGED",
                    "open_sockets" => "OPEN_SOCKETS_BASELINE_CHANGED",
                    "reflex" => "REFLEX_BASELINE_CHANGED",
                    "node" => "NODE_BASELINE_CHANGED",
                    "audio_eq" => "AUDIO_EQ_BASELINE_CHANGED",
                    "device_led_configs" => "LED_BASELINE_CHANGED",
                    "unit_acoustic_profiles" => "ACOUSTIC_PROFILE_BASELINE_CHANGED",
                    _ => "SEARCH_BASELINE_CHANGED",
                }),
            )?;
        } else {
            tx.execute("UPDATE config_head SET applying=1 WHERE id=1", [])?;
            if job.block == "persona" {
                let desired = job.payload["persona"]["desired"]
                    .as_str()
                    .ok_or_else(|| DeliveryError::internal("persona desired missing"))?;
                if desired.chars().count() > crate::agent_md::MAX_AGENT_MD_CHARS {
                    return Err(DeliveryError::internal("persona exceeds limit"));
                }
                if content(&tx)? != desired {
                    tx.execute("INSERT INTO agent_md_revisions(version,content,author,note,created_ms) SELECT COALESCE(MAX(version),0)+1,?1,?2,?3,?4 FROM agent_md_revisions",params![desired,job.principal,format!("Publish configuration {}",job.id),crate::id::now_ms() as i64])?;
                }
            } else if job.block == "mode" {
                super::mode::require_readable(&tx)?;
                let block = &job.payload["mode"];
                let current = current_mode
                    .ok_or_else(|| DeliveryError::internal("voice runtime is unavailable"))?;
                if super::mode::projection(current)
                    != super::mode::projection(&super::mode::native(&tx)?)
                {
                    return Err(DeliveryError::internal(
                        "voice runtime differs from its saved settings",
                    ));
                }
                let next = super::mode::resolve_projection(current, &block["desired"])?;
                if super::mode::projection(current) != super::mode::projection(&next) {
                    super::mode::write_projection_on(
                        &tx,
                        current,
                        &block["desired"],
                        &job.principal,
                        "Published reviewed voice policy",
                    )?;
                    next_mode = Some(next);
                }
            } else {
                let block = &job.payload[&job.block];
                let desired = &block["desired"];
                if job.block == "services" {
                    require_services_snapshot(desired)?;
                } else if let Some(block) = super::discovery::Block::named(&job.block) {
                    block.require_readable(&tx, store)?;
                    block.validate(desired.clone(), store.allows_loopback_public())?;
                } else if job.block == "weather" {
                    super::weather::require_readable_baseline(&tx)?;
                    super::weather::validate(desired.clone(), store.allows_loopback_public())?;
                } else if job.block == "audio_eq" {
                    let eq: crate::config::AudioEqConfig = serde_json::from_value(desired.clone())?;
                    eq.validate()
                        .map_err(|e| DeliveryError::new("INVALID_AUDIO_EQ", e))?;
                } else if job.block == "reflex" {
                    let rfx: crate::config::ReflexConfig = serde_json::from_value(desired.clone())?;
                    crate::config::compile_keywords(&rfx)
                        .map_err(|e| DeliveryError::new("INVALID_REFLEX", e))?;
                } else if job.block == "device_led_configs" {
                    if let Some(map) = desired.as_object() {
                        for v in map.values() {
                            let cfg: crate::config::DeviceLedConfig =
                                serde_json::from_value(v.clone())?;
                            cfg.validate().map_err(|e| {
                                DeliveryError::new("INVALID_LED_CONFIG", e.to_string())
                            })?;
                        }
                    }
                } else if job.block == "unit_acoustic_profiles" {
                    if let Some(map) = desired.as_object() {
                        let expected_map = block["expected"].as_object();
                        for (dev, v) in map {
                            let changed = expected_map.and_then(|m| m.get(dev)) != Some(v);
                            let dev_allowed =
                                dev == &node || hub.is_some_and(|h| h.device_exists(dev));
                            if changed && !dev_allowed {
                                return Err(DeliveryError::new(
                                    "FORBIDDEN_SCOPE",
                                    "unit acoustic profile device mismatch",
                                ));
                            }
                            let prof: crate::config::UnitAcousticProfile =
                                serde_json::from_value(v.clone())?;
                            prof.validate().map_err(|e| {
                                DeliveryError::new("INVALID_ACOUSTIC_PROFILE", e.to_string())
                            })?;
                        }
                    }
                }
                if block["legacy"] != true && desired != &block["expected"] {
                    if job.block == "openapi" {
                        let config = serde_json::from_value(desired.clone())?;
                        super::openapi::check_source_binding(&tx, &config)?;
                        let token = store
                            .secret_get_on(&tx, crate::config::OPENAPI_TOKEN_SECRET)
                            .map_err(DeliveryError::internal)?;
                        super::openapi::reject_exposed_token(&config, token.as_deref())?;
                        Store::publish_openapi_on(&tx, &config, &job.principal)
                            .map_err(DeliveryError::internal)?;
                    } else if job.block == "open_sockets" {
                        let desired = super::open_sockets::canonical(desired.clone())?;
                        super::open_sockets::check_bindings(&tx, store, &desired)?;
                        super::open_sockets::publish_on(&tx, &desired, &job.principal)?;
                    } else {
                        let mut native = super::service::raw(&tx, &job.block)?
                            .map(|s| serde_json::from_str::<Value>(&s))
                            .transpose()?
                            .unwrap_or_else(|| json!({}));
                        super::service::merge_known(&mut native, desired);
                        tx.execute("INSERT INTO config_kv(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![job.block,native.to_string()])?;
                    }
                }
            }
            finish(&tx, job, "applied", None)?;
            applied = true;
            tx.execute("UPDATE config_head SET applying=0 WHERE id=1", [])?;
        }
        tx.commit()?;
        Ok((next_mode, applied))
    });
    match attempt {
        Ok(published) => Ok(published),
        Err(_) => {
            record_failure(
                store,
                job,
                match job.block.as_str() {
                    "persona" => "PERSONA_PUBLICATION_FAILED",
                    "services" => "SERVICES_PUBLICATION_FAILED",
                    "weather" => "WEATHER_PUBLICATION_FAILED",
                    "news" => "NEWS_PUBLICATION_FAILED",
                    "home" => "HOME_PUBLICATION_FAILED",
                    "music" => "MUSIC_PUBLICATION_FAILED",
                    "openapi" => "OPENAPI_PUBLICATION_FAILED",
                    "mode" => "VOICE_PUBLICATION_FAILED",
                    "open_sockets" => "OPEN_SOCKETS_PUBLICATION_FAILED",
                    "reflex" => "REFLEX_PUBLICATION_FAILED",
                    "node" => "NODE_PUBLICATION_FAILED",
                    "audio_eq" => "AUDIO_EQ_PUBLICATION_FAILED",
                    "device_led_configs" => "LED_PUBLICATION_FAILED",
                    "unit_acoustic_profiles" => "ACOUSTIC_PROFILE_PUBLICATION_FAILED",
                    _ => "SEARCH_PUBLICATION_FAILED",
                },
            )?;
            Ok((None, false))
        }
    }
}

fn record_failure(store: &Store, job: &Job, code: &str) -> Result<()> {
    database(store, |c| {
        let tx = rusqlite::Transaction::new_unchecked(c, rusqlite::TransactionBehavior::Immediate)?;
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM config_application_blocks WHERE principal=?1 AND application_id=?2 AND block=?3 AND state='saved_pending')",
            params![job.principal, job.id, job.block], |r| r.get(0),
        )?;
        if pending {
            let current: bool = tx.query_row(
                "SELECT revision=?1 AND data_epoch=?2 FROM config_head WHERE id=1",
                params![job.revision as i64, job.epoch as i64],
                |r| r.get(0),
            )?;
            finish(
                &tx,
                job,
                if current { "failed" } else { "superseded" },
                Some(code),
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

fn finish(c: &Connection, job: &Job, state: &str, error: Option<&str>) -> Result<()> {
    let text: String = c.query_row(
        "SELECT payload_json FROM config_outbox WHERE principal=?1 AND application_id=?2",
        params![job.principal, job.id],
        |r| r.get(0),
    )?;
    let mut payload: Value = serde_json::from_str(&text)?;
    if payload.get(&job.block).is_none() {
        payload[&job.block] = job.payload[&job.block].clone();
    }
    payload[&job.block]["state"] = json!(state);
    payload[&job.block]["error"] = json!(error);
    payload[&job.block]["checked_ms"] = json!(crate::id::now_ms());
    c.execute("UPDATE config_application_blocks SET state=?1 WHERE principal=?2 AND application_id=?3 AND block=?4 AND state='saved_pending'",params![state,job.principal,job.id,job.block])?;
    let pending:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM config_application_blocks WHERE principal=?1 AND application_id=?2 AND state='saved_pending')",params![job.principal,job.id],|r|r.get(0))?;
    let failed:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM config_application_blocks WHERE principal=?1 AND application_id=?2 AND state='failed')",params![job.principal,job.id],|r|r.get(0))?;
    let aggregate = if failed {
        "failed"
    } else if pending {
        "partial_pending"
    } else {
        state
    };
    c.execute("UPDATE config_outbox SET state=?1,payload_json=?2 WHERE principal=?3 AND application_id=?4 AND data_epoch=?5",params![aggregate,payload.to_string(),job.principal,job.id,job.epoch as i64])?;
    Ok(())
}

#[cfg(test)]
pub(super) fn run_once(store: &Store) -> Result<()> {
    run_with_runtime(store, None, None, None)
}

fn mode_has_gateway_changes(job: &Job) -> bool {
    let block = &job.payload["mode"];
    let expected = &block["expected"];
    let desired = &block["desired"];
    [
        "wake_words",
        "retreat_words",
        "kws_threshold",
        "kws_score",
        "duplex_requested",
    ]
    .iter()
    .any(|k| desired.get(k) != expected.get(k))
}

fn is_gateway_block(job: &Job) -> bool {
    matches!(
        job.block.as_str(),
        "reflex" | "node" | "audio_eq" | "device_led_configs" | "unit_acoustic_profiles"
    ) || (job.block == "mode" && mode_has_gateway_changes(job))
}

fn run_with_runtime(
    store: &Store,
    voice: Option<&Arc<VoicePipeline>>,
    open_sockets: Option<&super::open_sockets::Provider>,
    hub: Option<&DeviceHub>,
) -> Result<()> {
    for job in prepare(store)? {
        if is_gateway_block(&job) && (hub.is_some() || job.block != "mode") {
            continue;
        }
        match job.block.as_str() {
            "mode" => {
                if let Some(voice) = voice {
                    let _ = voice.publish_reviewed_settings(|current| {
                        publish_transaction(store, &job, Some(current), hub)
                    })?;
                }
            }
            "open_sockets" => {
                if let Some(provider) = open_sockets {
                    let attempt = super::open_sockets::canonical(
                        job.payload["open_sockets"]["desired"].clone(),
                    )
                    .and_then(|desired| {
                        provider
                            .runtime
                            .publish_reviewed(&provider.registry, desired, || {
                                publish_transaction(store, &job, None, hub)
                                    .map(|(_, applied)| applied)
                            })
                    });
                    if let Err(error) = attempt {
                        record_failure(store, &job, &error.code)?;
                    }
                }
            }
            _ => {
                publish_transaction(store, &job, None, hub)?;
            }
        }
    }
    Ok(())
}

async fn blocking_db<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| DeliveryError::internal(format!("blocking sqlite task failed: {e}")))?
}

async fn run_gateway_jobs(
    store: &Arc<Store>,
    voice: Option<&Arc<VoicePipeline>>,
    hub: &Arc<DeviceHub>,
) -> Result<()> {
    let store_prep = store.clone();
    let (jobs, node_id) = blocking_db(move || {
        let jobs = prepare(&store_prep)?;
        if jobs.is_empty() {
            return Ok((Vec::new(), String::new()));
        }
        let node_id: String = database(&store_prep, |c| {
            Ok(
                c.query_row("SELECT node_id FROM config_head WHERE id=1", [], |r| {
                    r.get(0)
                })?,
            )
        })?;
        Ok((jobs, node_id))
    })
    .await?;
    if jobs.is_empty() || node_id.is_empty() {
        return Ok(());
    }

    for job in jobs {
        if !is_gateway_block(&job) {
            continue;
        }
        let block_data = &job.payload[&job.block];
        let desired = &block_data["desired"];
        let expected = block_data.get("expected");
        let current_voice = voice.map(|v| v.settings_snapshot());
        let store_snap = store.clone();
        let (current_reflex, current_node, current_led_map) = blocking_db(move || {
            let current_reflex = store_snap
                .config_get(crate::config::REFLEX_KEY)
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str::<crate::config::ReflexConfig>(&s).ok());
            let current_node = store_snap
                .config_get("node")
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok());
            let current_led_map = store_snap
                .config_get(crate::config::DEVICE_LED_CONFIGS_KEY)
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok());
            Ok((current_reflex, current_node, current_led_map))
        })
        .await?;

        // Resolve target gateway devices for this config job (`MG-P1-02`):
        // - `unit_acoustic_profiles` and `device_led_configs` target ONLY per-unit
        //   keys that actually changed in `desired` vs `expected`. If a changed
        //   target device is offline, NEVER fall back to pushing to `node_id`
        //   (`dot2-d`); record `skipped_offline` and persist for reconnect reconciliation.
        // - Shared gateway blocks (`reflex`, `audio_eq`, `mode`, `node`) target every
        //   online `config_apply_v2` device, falling back to `[node_id]` only when
        //   zero v2 gateways are online so offline failures remain honest.
        let mut receipts: Vec<Value> = Vec::new();
        let target_devices: Vec<String> = match job.block.as_str() {
            "unit_acoustic_profiles" | "device_led_configs" => {
                let online_v2 = hub.devices_with_cap("config_apply_v2");
                let expected_map = expected.and_then(|v| v.as_object());
                let cfg_block = if job.block == "unit_acoustic_profiles" {
                    crate::gateway::ConfigBlock::Acoustics
                } else {
                    crate::gateway::ConfigBlock::Led
                };
                let mut changed: Vec<String> = desired
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .filter(|(k, v)| {
                                expected_map.is_none_or(|em| em.get(k.as_str()) != Some(*v))
                            })
                            .map(|(k, _)| k.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                changed.sort();
                let mut matched = Vec::new();
                for dev in changed {
                    if online_v2.iter().any(|d| d == &dev) {
                        matched.push(dev);
                    } else {
                        receipts.push(json!({
                            "device": dev,
                            "block": cfg_block,
                            "state": "skipped_offline",
                            "reason": "target device is offline or lacks config_apply_v2; saved for reconnect reconciliation",
                        }));
                    }
                }
                matched
            }
            _ => {
                let mut online_v2 = hub.devices_with_cap("config_apply_v2");
                online_v2.sort();
                if online_v2.is_empty() {
                    vec![node_id.clone()]
                } else {
                    online_v2
                }
            }
        };

        // Plan blocks per device and ALWAYS check per-device capabilities
        // (`supports_config_apply_v2`), even when only 1 gateway (e.g. Android)
        // is online (`MG-P1-02` #4).
        let mut planned_by_device: std::collections::BTreeMap<
            String,
            Vec<(crate::gateway::ConfigBlock, Value)>,
        > = std::collections::BTreeMap::new();
        let mut build_err: Option<DeliveryError> = None;
        for target_dev in &target_devices {
            let current_led = current_led_map
                .as_ref()
                .and_then(|v| v.get(target_dev).cloned())
                .and_then(|v| serde_json::from_value::<crate::config::DeviceLedConfig>(v).ok());
            match super::gateway_dispatch::build_gateway_payload(
                &job.block,
                target_dev,
                hub.kws_device_class(target_dev),
                desired,
                expected,
                current_voice.as_ref(),
                current_reflex.as_ref(),
                current_node.as_ref(),
                current_led.as_ref(),
            ) {
                Ok(items) => {
                    let dev_is_online_v2 =
                        hub.device_online_with_cap(target_dev, "config_apply_v2");
                    for (cfg_block, values) in items {
                        if let Err(reason) = hub.supports_config_apply_v2(target_dev, cfg_block) {
                            if dev_is_online_v2 {
                                receipts.push(json!({
                                    "device": target_dev,
                                    "block": cfg_block,
                                    "state": "skipped_unsupported",
                                    "reason": reason,
                                }));
                                continue;
                            }
                        }
                        planned_by_device
                            .entry(target_dev.clone())
                            .or_default()
                            .push((cfg_block, values));
                    }
                }
                Err(e) => {
                    build_err = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = build_err {
            let store_err = store.clone();
            let job_err = job.clone();
            blocking_db(move || {
                record_failure_with_receipt(
                    &store_err,
                    &job_err,
                    &e.code,
                    json!({ "error": e.detail }),
                )
            })
            .await?;
            continue;
        }

        let mut all_applied_blocks: Vec<(String, crate::gateway::ConfigBlock)> = Vec::new();
        let mut failures: Vec<(String, String, &'static str)> = Vec::new();

        // `block_generation` is tracked per `(device, block)` monotonically.
        // Using `revision * 10 + 1` for forward apply and `revision * 10 + 2` for
        // rollback compensation guarantees strict monotonicity across any number
        // of devices and blocks without ever colliding into `revision + 1`.
        let forward_gen = job.revision.saturating_mul(10).saturating_add(1);
        let rollback_gen = job.revision.saturating_mul(10).saturating_add(2);

        // Dispatch per device independently (`MG-P1-02` #1): a failure on Device B
        // rolls back ONLY Device B's own intra-device partial blocks and NEVER rolls
        // back healthy sibling Device A.
        for (target_dev, blocks) in &planned_by_device {
            let mut dev_applied: Vec<(String, crate::gateway::ConfigBlock)> = Vec::new();
            let mut dev_failure: Option<(String, &'static str)> = None;
            for (cfg_block, values) in blocks {
                let mut outcome = super::gateway_dispatch::dispatch_block_v2(
                    hub,
                    target_dev,
                    &node_id,
                    job.epoch,
                    &job.id,
                    job.revision,
                    forward_gen,
                    *cfg_block,
                    values.clone(),
                )
                .await;
                if let Some(obj) = outcome.receipt.as_object_mut() {
                    obj.entry("device".to_string())
                        .or_insert_with(|| json!(target_dev));
                }
                receipts.push(outcome.receipt.clone());
                if outcome.state == "applied" {
                    dev_applied.push((target_dev.clone(), *cfg_block));
                    all_applied_blocks.push((target_dev.clone(), *cfg_block));
                } else {
                    dev_failure = Some((
                        outcome
                            .error
                            .unwrap_or_else(|| "GATEWAY_DISPATCH_FAILED".into()),
                        outcome.state,
                    ));
                    break;
                }
            }
            if let Some((err_msg, state)) = dev_failure {
                if !dev_applied.is_empty() {
                    // Roll back ONLY this failing device's intra-device partial blocks.
                    let single = vec![target_dev.clone()];
                    rollback_applied_gateway_blocks(
                        hub,
                        &job,
                        &node_id,
                        rollback_gen,
                        &single,
                        &dev_applied,
                        expected,
                        current_voice.as_ref(),
                        current_reflex.as_ref(),
                        current_node.as_ref(),
                        current_led_map.as_ref(),
                    )
                    .await;
                }
                failures.push((target_dev.clone(), err_msg, state));
            }
        }

        if !failures.is_empty() {
            let receipts_json = json!(receipts);
            let all_superseded = failures.iter().all(|(_, _, st)| *st == "superseded");
            let err_summary = if failures.len() == 1 {
                failures[0].1.clone()
            } else {
                failures
                    .iter()
                    .map(|(d, e, _)| format!("{d}: {e}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            let store_fail = store.clone();
            let job_fail = job.clone();
            blocking_db(move || {
                if all_superseded {
                    record_superseded_with_receipt(
                        &store_fail,
                        &job_fail,
                        &err_summary,
                        receipts_json,
                    )
                } else {
                    record_failure_with_receipt(&store_fail, &job_fail, &err_summary, receipts_json)
                }
            })
            .await?;
            continue;
        }

        let receipts_json = json!(receipts);
        let store_pub = store.clone();
        let voice_pub = voice.cloned();
        let hub_pub = hub.clone();
        let job_pub = job.clone();
        let pub_res = blocking_db(move || {
            if job_pub.block == "mode" {
                if let Some(voice) = voice_pub.as_ref() {
                    voice.publish_reviewed_settings(|current| {
                        publish_transaction(&store_pub, &job_pub, Some(current), Some(&hub_pub))
                    })
                } else {
                    Err(DeliveryError::internal("voice runtime is unavailable"))
                }
            } else {
                publish_transaction(&store_pub, &job_pub, None, Some(&hub_pub))
                    .map(|(_, applied)| applied)
            }
        })
        .await;
        match pub_res {
            Ok(true) => {
                let store_rcpt = store.clone();
                let job_rcpt = job.clone();
                blocking_db(move || store_gateway_receipt(&store_rcpt, &job_rcpt, receipts_json))
                    .await?;
            }
            Ok(false) => {
                // SQLite rejected or superseded publication after gateways applied
                // the forward config: compensate all applied gateways immediately
                // so live gateway state cannot diverge from Core SQLite.
                rollback_applied_gateway_blocks(
                    hub,
                    &job,
                    &node_id,
                    rollback_gen,
                    &target_devices,
                    &all_applied_blocks,
                    expected,
                    current_voice.as_ref(),
                    current_reflex.as_ref(),
                    current_node.as_ref(),
                    current_led_map.as_ref(),
                )
                .await;
            }
            Err(err) => {
                rollback_applied_gateway_blocks(
                    hub,
                    &job,
                    &node_id,
                    rollback_gen,
                    &target_devices,
                    &all_applied_blocks,
                    expected,
                    current_voice.as_ref(),
                    current_reflex.as_ref(),
                    current_node.as_ref(),
                    current_led_map.as_ref(),
                )
                .await;
                return Err(err);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn rollback_applied_gateway_blocks(
    hub: &Arc<DeviceHub>,
    job: &Job,
    core_node_id: &str,
    rollback_gen: u64,
    target_devices: &[String],
    applied_blocks: &[(String, crate::gateway::ConfigBlock)],
    expected: Option<&Value>,
    current_voice: Option<&crate::voice::VoiceSettings>,
    current_reflex: Option<&crate::config::ReflexConfig>,
    current_node: Option<&Value>,
    current_led_map: Option<&Value>,
) {
    if applied_blocks.is_empty() {
        return;
    }
    for target_dev in target_devices {
        let dev_applied: Vec<crate::gateway::ConfigBlock> = applied_blocks
            .iter()
            .filter(|(d, _)| d == target_dev)
            .map(|(_, b)| *b)
            .collect();
        if dev_applied.is_empty() {
            continue;
        }
        let current_led = current_led_map
            .and_then(|v| v.get(target_dev).cloned())
            .and_then(|v| serde_json::from_value::<crate::config::DeviceLedConfig>(v).ok());
        let mut rollback_targets = if let Some(exp) = expected {
            super::gateway_dispatch::build_gateway_payload(
                &job.block,
                target_dev,
                hub.kws_device_class(target_dev),
                exp,
                None,
                current_voice,
                current_reflex,
                current_node,
                current_led.as_ref(),
            )
            .unwrap_or_default()
        } else {
            Vec::new()
        };
        // If `mode` applied `ConfigBlock::Duplex` on a device whose prior
        // `expected.duplex_requested` had no explicit entry, its effective
        // baseline was the everyday default `"half"`: synthesize an explicit
        // rollback frame so the gateway never remains stuck in `"full"` duplex.
        if job.block == "mode"
            && dev_applied.contains(&crate::gateway::ConfigBlock::Duplex)
            && !rollback_targets
                .iter()
                .any(|(b, _)| *b == crate::gateway::ConfigBlock::Duplex)
        {
            rollback_targets.push((
                crate::gateway::ConfigBlock::Duplex,
                json!({ "duplex_requested": "half" }),
            ));
        }
        for (rb_block, rb_values) in rollback_targets {
            if dev_applied.contains(&rb_block) {
                let _ = super::gateway_dispatch::dispatch_block_v2(
                    hub,
                    target_dev,
                    core_node_id,
                    job.epoch,
                    &job.id,
                    job.revision,
                    rollback_gen,
                    rb_block,
                    rb_values,
                )
                .await;
            }
        }
    }
}

fn store_gateway_receipt(store: &Store, job: &Job, receipts: Value) -> Result<()> {
    database(store, |c| {
        let tx = rusqlite::Transaction::new_unchecked(c, rusqlite::TransactionBehavior::Immediate)?;
        let text: Option<String> = tx
            .query_row(
                "SELECT payload_json FROM config_outbox WHERE principal=?1 AND application_id=?2",
                params![job.principal, job.id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(text) = text {
            let mut payload: Value = serde_json::from_str(&text)?;
            payload[&job.block]["gateway_receipt"] = receipts;
            tx.execute(
                "UPDATE config_outbox SET payload_json=?1 WHERE principal=?2 AND application_id=?3",
                params![payload.to_string(), job.principal, job.id],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

fn record_failure_with_receipt(
    store: &Store,
    job: &Job,
    code: &str,
    receipts: Value,
) -> Result<()> {
    record_failure(store, job, code)?;
    store_gateway_receipt(store, job, receipts)
}

fn record_superseded_with_receipt(
    store: &Store,
    job: &Job,
    code: &str,
    receipts: Value,
) -> Result<()> {
    database(store, |c| {
        let tx = rusqlite::Transaction::new_unchecked(c, rusqlite::TransactionBehavior::Immediate)?;
        finish(&tx, job, "superseded", Some(code))?;
        tx.commit()?;
        Ok(())
    })?;
    store_gateway_receipt(store, job, receipts)
}

pub(super) fn evidence(c: &Connection, principal: &str, id: &str, block: &str) -> Result<Value> {
    let row: Option<(String,String,i64,i64)> = c.query_row("SELECT b.state,o.payload_json,a.revision,o.data_epoch FROM config_application_blocks b JOIN config_outbox o USING(principal,application_id) JOIN config_applications a USING(principal,application_id) WHERE b.principal=?1 AND b.application_id=?2 AND b.block=?3",params![principal,id,block],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    match row {
        Some((state, payload, revision, epoch)) => {
            let payload: Value = serde_json::from_str(&payload)?;
            let mut out = json!({
                "block": block,
                "application_id": id,
                "revision": revision,
                "data_epoch": epoch,
                "state": state,
                "error": payload[block]["error"],
                "isolation": if payload.get(block).is_some() && payload[block]["legacy"] != true { "desired_until_worker" } else { "legacy_projection" }
            });
            if let Some(gw) = payload[block]
                .get("gateway_receipt")
                .filter(|v| !v.is_null())
            {
                out["gateway_receipt"] = gw.clone();
            }
            Ok(out)
        }
        None => Ok(Value::Null),
    }
}
