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
                match tokio::task::spawn_blocking(move || {
                    run_with_runtime(&store_c, voice_c.as_ref(), open_sockets_c.as_ref())
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
             WHERE b.block IN ('persona','services','weather','home','music','news','search','openapi','mode','remote_socket','open_sockets','reflex','node','audio_eq','device_led_configs','unit_acoustic_profiles') AND b.state='saved_pending'
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
    publish_transaction(store, &job, None).map(|_| ())
}

fn publish_transaction(
    store: &Store,
    job: &Job,
    current_mode: Option<&VoiceSettings>,
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
                    "remote_socket" => "REMOTE_SOCKET_BASELINE_CHANGED",
                    "open_sockets" => "OPEN_SOCKETS_BASELINE_CHANGED",
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
                    if matches!(block, super::discovery::Block::RemoteSocket) {
                        super::remote_socket::check_publication(
                            &tx,
                            store,
                            &serde_json::from_value(desired.clone())?,
                        )?;
                    }
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
                        for (dev, v) in map {
                            if dev != &node {
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
                    "remote_socket" => "REMOTE_SOCKET_PUBLICATION_FAILED",
                    "open_sockets" => "OPEN_SOCKETS_PUBLICATION_FAILED",
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
    run_with_runtime(store, None, None)
}

fn mode_has_gateway_changes(job: &Job) -> bool {
    let block = &job.payload["mode"];
    let expected = &block["expected"];
    let desired = &block["desired"];
    [
        "wake_words",
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
) -> Result<()> {
    for job in prepare(store)? {
        if is_gateway_block(&job) {
            continue;
        }
        match job.block.as_str() {
            "mode" => {
                if let Some(voice) = voice {
                    voice.publish_reviewed_settings(|current| {
                        publish_transaction(store, &job, Some(current)).map(|(next, _)| next)
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
                                publish_transaction(store, &job, None).map(|(_, applied)| applied)
                            })
                    });
                    if let Err(error) = attempt {
                        record_failure(store, &job, error.code)?;
                    }
                }
            }
            _ => {
                publish_transaction(store, &job, None)?;
            }
        }
    }
    Ok(())
}

async fn run_gateway_jobs(
    store: &Store,
    voice: Option<&Arc<VoicePipeline>>,
    hub: &Arc<DeviceHub>,
) -> Result<()> {
    let jobs = prepare(store)?;
    if jobs.is_empty() {
        return Ok(());
    }
    let node_id: String = database(store, |c| {
        Ok(
            c.query_row("SELECT node_id FROM config_head WHERE id=1", [], |r| {
                r.get(0)
            })?,
        )
    })?;
    if node_id.is_empty() {
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
        let current_reflex = store
            .config_get(crate::config::REFLEX_KEY)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<crate::config::ReflexConfig>(&s).ok());
        let current_node = store
            .config_get("node")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        let current_led = store
            .config_get(crate::config::DEVICE_LED_CONFIGS_KEY)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get(&node_id).cloned())
            .and_then(|v| serde_json::from_value::<crate::config::DeviceLedConfig>(v).ok());

        let targets = match super::gateway_dispatch::build_gateway_payload(
            &job.block,
            &node_id,
            desired,
            expected,
            current_voice.as_ref(),
            current_reflex.as_ref(),
            current_node.as_ref(),
            current_led.as_ref(),
        ) {
            Ok(t) => t,
            Err(e) => {
                record_failure_with_receipt(store, &job, e.code, json!({ "error": e.detail }))?;
                continue;
            }
        };

        let mut receipts = Vec::new();
        let mut applied_blocks = Vec::new();
        let mut failure: Option<(String, &'static str, Value)> = None;

        for (idx, (cfg_block, values)) in targets.iter().enumerate() {
            let outcome = super::gateway_dispatch::dispatch_block_v2(
                hub,
                &node_id,
                job.epoch,
                &job.id,
                job.revision,
                job.revision.saturating_mul(10) + (idx as u64) + 1,
                *cfg_block,
                values.clone(),
            )
            .await;
            receipts.push(outcome.receipt.clone());
            if outcome.state == "applied" {
                applied_blocks.push(*cfg_block);
            } else {
                failure = Some((
                    outcome
                        .error
                        .unwrap_or_else(|| "GATEWAY_DISPATCH_FAILED".into()),
                    outcome.state,
                    json!(receipts),
                ));
                break;
            }
        }

        if let Some((err_msg, state, receipts_json)) = failure {
            // Rollback compensation for any sub-blocks that already succeeded in this job.
            if !applied_blocks.is_empty() {
                if let Some(exp) = expected {
                    if let Ok(rollback_targets) = super::gateway_dispatch::build_gateway_payload(
                        &job.block,
                        &node_id,
                        exp,
                        None,
                        current_voice.as_ref(),
                        current_reflex.as_ref(),
                        current_node.as_ref(),
                        current_led.as_ref(),
                    ) {
                        for (idx, (rb_block, rb_values)) in rollback_targets.into_iter().enumerate()
                        {
                            if applied_blocks.contains(&rb_block) {
                                let _ = super::gateway_dispatch::dispatch_block_v2(
                                    hub,
                                    &node_id,
                                    job.epoch,
                                    &job.id,
                                    job.revision,
                                    job.revision.saturating_mul(10) + 5 + (idx as u64),
                                    rb_block,
                                    rb_values,
                                )
                                .await;
                            }
                        }
                    }
                }
            }
            if state == "superseded" {
                record_superseded_with_receipt(store, &job, &err_msg, receipts_json)?;
            } else {
                record_failure_with_receipt(store, &job, &err_msg, receipts_json)?;
            }
            continue;
        }

        let receipts_json = json!(receipts);
        let pub_res = if job.block == "mode" {
            if let Some(voice) = voice {
                voice.publish_reviewed_settings(|current| {
                    publish_transaction(store, &job, Some(current)).map(|(next, _)| next)
                })
            } else {
                Err(DeliveryError::internal("voice runtime is unavailable"))
            }
        } else {
            publish_transaction(store, &job, None).map(|_| ())
        };
        if pub_res.is_ok() {
            store_gateway_receipt(store, &job, receipts_json)?;
        }
    }
    Ok(())
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
