mod diag;
mod persona;
mod personal;
mod sockets;
pub(crate) mod system;
mod tools;

pub(super) use diag::*;
pub(super) use persona::*;
pub(super) use personal::*;
pub(super) use sockets::*;
pub use system::{
    apply_config_import, build_config_document, ConfigImportBody, ImportOutcome,
    FLEET_SOURCE_HEADER,
};
pub(super) use system::{
    config_export, config_history, config_history_restore, config_import, fleet_cue, fleet_get,
    fleet_node_delete, fleet_node_put, fleet_owner, fleet_ping, fleet_playback, fleet_push,
    fleet_self_put, fleet_wake,
};
pub(super) use tools::*;

use super::*;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[cfg(test)]
mod catalog_repair_tests;

/// 2026-11-13: what the model can do and SEE right now. Used only to notice a
/// configuration change that invalidates the assistant's own older statements.
/// The ready-tool set alone missed an entity-directory change (adding a light
/// keeps `home.set` ready), so the home whitelist is part of the fingerprint.
pub(super) fn service_fingerprint(state: &AppState) -> Vec<String> {
    let mut out: Vec<String> = state
        .registry
        .ready_capabilities()
        .iter()
        .flat_map(|s| s.tools.iter().map(|t| t.name.clone()))
        .collect();
    let home = state.home.config();
    let mut names: Vec<String> = home.entities.iter().map(|e| e.name.clone()).collect();
    names.sort();
    out.push(format!("home_entities={}", names.join(",")));
    out
}

/// A service becoming usable (or unusable) invalidates the assistant's own
/// older statements about it — the real-HA failure was the model replaying
/// 「家庭灯光后端还没有接入」 from a transcript written before the household
/// configured it. The conversation is where those statements live, so the
/// DEVICE sessions are cleared when the ready-tool set actually changes.
/// Console/text sessions are left alone (they are the user's own workspace).
pub(super) fn reset_device_history_if_capabilities_changed(state: &AppState, before: Vec<String>) {
    let after = service_fingerprint(state);
    if before == after {
        return;
    }
    let cleared = state.sessions.clear_device_sessions().unwrap_or(0);
    tracing::info!(
        tools_before = ?before,
        tools_after = ?after,
        cleared,
        "service availability changed: device conversations cleared so stale claims cannot be replayed"
    );
    let _ = crate::config::append_history(
        &state.store,
        "system",
        "voice",
        &format!("service availability changed; cleared {cleared} device message(s)"),
        &serde_json::json!({ "tools_before": before, "tools_after": after, "messages_cleared": cleared }),
    );
}

// --- status -------------------------------------------------------------------

#[derive(Default, Deserialize)]
pub(super) struct StatusQuery {
    #[serde(default)]
    pub(super) device: Option<String>,
}

pub(super) async fn status(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StatusQuery>,
) -> Json<Value> {
    let services = state.config.statuses().unwrap_or(json!({}));
    // R2 maintenance (2026-11-13): the last retention/checkpoint run as the
    // store recorded it (window, deleted rows, WAL frames). Null until the
    // first run — nothing is fabricated.
    let db = state
        .store
        .config_get("db.maintenance")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    let mut db = db;
    let schema = state.store.schema_version().ok();
    match db.as_object_mut() {
        Some(obj) => {
            obj.insert("schema".into(), json!(schema));
        }
        None => db = json!({ "schema": schema }),
    }
    // Per-device `wifi_provisioning` tracking (`MG-P2-04`): preserve every
    // device's active provisioning request without letting sibling devices
    // overwrite or hide each other.
    let recent_voice = state.voice.recent(100);
    let mut resolved_wifi_devices = std::collections::BTreeSet::new();
    let mut wifi_provisioning_by_device: std::collections::BTreeMap<String, Value> =
        std::collections::BTreeMap::new();
    for e in &recent_voice {
        let dev = e["device"].as_str().unwrap_or_default();
        if dev.is_empty() {
            continue;
        }
        match e["kind"].as_str() {
            Some("wifi_state")
                if matches!(
                    e["state"].as_str(),
                    Some("applied" | "connected" | "joined" | "associated")
                ) =>
            {
                resolved_wifi_devices.insert(dev.to_string());
            }
            Some("wifi_provisioning_requested")
                if !resolved_wifi_devices.contains(dev)
                    && !wifi_provisioning_by_device.contains_key(dev) =>
            {
                wifi_provisioning_by_device.insert(
                    dev.to_string(),
                    json!({
                        "requested": true,
                        "device": dev,
                        "action": e["action"],
                        "trigger": e.get("trigger").cloned().unwrap_or_else(|| json!("mute_hold")),
                        "ts_ms": e["ts_ms"],
                    }),
                );
            }
            _ => {}
        }
    }
    for (dev, ts_ms) in state.voice.wifi_provisioning_map() {
        wifi_provisioning_by_device
            .entry(dev.clone())
            .or_insert_with(|| {
                json!({
                    "requested": true,
                    "device": dev,
                    "action": "mute_hold_wifi_setup",
                    "trigger": "mute_hold",
                    "ts_ms": ts_ms,
                })
            });
    }
    let requested_dev = q.device.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let empty_wifi_prov = |dev: Option<&str>| {
        json!({
            "requested": false,
            "device": dev,
            "action": Value::Null,
            "trigger": Value::Null,
            "ts_ms": Value::Null,
        })
    };
    let wifi_provisioning = match requested_dev {
        Some(dev) => wifi_provisioning_by_device
            .get(dev)
            .cloned()
            .unwrap_or_else(|| empty_wifi_prov(Some(dev))),
        None => wifi_provisioning_by_device
            .values()
            .next()
            .cloned()
            .unwrap_or_else(|| empty_wifi_prov(None)),
    };
    let mut gateway = state.hub.snapshot();
    let voice_settings = state.voice.settings_snapshot();
    let tail_blind_ms = match requested_dev {
        Some(dev) => voice_settings.effective_for(dev).tail_blind_ms,
        None => voice_settings.tail_blind_ms,
    };
    let tail_mismatch = enrich_gateway_tail_mismatch(&mut gateway, &voice_settings);
    Json(json!({
        "product": "Home Mate",
        "core_id": state.auth.core_id(),
        "assistant_name": state.effective_assistant_name(),
        "now_ms": crate::id::now_ms(),
        "version": env!("CARGO_PKG_VERSION"),
        "build": { "version": crate::CORE_VERSION, "id": crate::BUILD_ID },
        "data_dir": state.data_dir.display().to_string(),
        "services": services,
        "gateway": gateway,
        "tools": state.registry.names(),
        "tool_catalog_error": state.registry.catalog().err().map(|e| e.to_string()),
        "db": db,
        // M9: never a token or hash — just whether the household plane is
        // paired, so the console and an operator can tell the open state.
        "auth": {
            "configured": state.auth.configured(),
            "tokens": state.auth.summaries().len(),
            "loopback_trusted": true,
            "lan_gateway_exempt": state.auth.lan_gateway_exempt(),
        },
        // M12a-1: how many stored credentials are sealed, how many could not
        // be opened, and whether the key is usable. Counts only, never a value.
        "vault": state.store.vault_status(),
        "voice": {
            "pump_metrics": state.voice.pump_metrics_json(),
            "tail_blind_ms": tail_blind_ms,
            "tail_mismatch": tail_mismatch,
        },
        "wifi_provisioning": wifi_provisioning,
        "wifi_provisioning_by_device": wifi_provisioning_by_device,
    }))
}

pub(super) fn enrich_gateway_tail_mismatch(
    gateway: &mut Value,
    settings: &crate::voice::VoiceSettings,
) -> bool {
    let mut any_mismatch = false;
    if let Some(devices) = gateway.get_mut("devices").and_then(|v| v.as_array_mut()) {
        for dev in devices {
            let dev_id = dev.get("stable_id").and_then(|v| v.as_str()).unwrap_or("");
            let dev_tail = if dev_id.is_empty() {
                settings.tail_blind_ms
            } else {
                settings.effective_for(dev_id).tail_blind_ms
            };
            let aec_tail = dev.get("aec_tail_ms").and_then(|v| v.as_u64());
            let mismatch = aec_tail.is_some_and(|aec| dev_tail < aec);
            if mismatch {
                any_mismatch = true;
            }
            if let Some(obj) = dev.as_object_mut() {
                obj.insert("tail_blind_ms".into(), json!(dev_tail));
                obj.insert("tail_mismatch".into(), json!(mismatch));
            }
        }
    }
    any_mismatch
}

// --- config -------------------------------------------------------------------

pub(super) async fn config_services(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.config.statuses().unwrap_or(json!({})))
}

// --- device labels (household metadata, console-owned) -----------------------

pub(super) async fn device_meta_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let meta = state.config.device_meta().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    Ok(Json(json!({ "meta": meta })))
}

#[derive(Deserialize)]
pub(super) struct DeviceMetaPut {
    #[serde(default)]
    name: String,
    #[serde(default)]
    room: String,
}

pub(super) async fn device_meta_put(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<DeviceMetaPut>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let meta = state
        .config
        .save_device_meta(&id, &body.name, &body.room, "console")
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": e.to_string() })),
            )
        })?;
    Ok(Json(
        json!({ "state": "applied", "device": id, "meta": meta }),
    ))
}

/// Restart the core process; the supervisor respawns it from the active slot.
/// The response is sent first, then the process exits — the console shows the
/// reconnect through its normal status poll.
pub(super) async fn system_restart() -> Json<Value> {
    tracing::warn!("core restart requested from the console");
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        std::process::exit(0);
    });
    Json(json!({ "state": "restarting" }))
}

#[derive(Deserialize)]
pub(super) struct FactoryResetReq {
    #[serde(default)]
    confirm: String,
    #[serde(default)]
    include_secrets: bool,
}

fn remove_dir_if_exists(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_dir_all(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %path.display(), error = %e, "failed to clear directory during factory reset");
        }
    }
}

/// Clear user config, device metadata, and non-secret runtime state.
/// Requires explicit `{"confirm": "FACTORY_RESET"}`; never bound to a physical button.
pub(super) async fn system_factory_reset(
    State(state): State<Arc<AppState>>,
    Json(body): Json<FactoryResetReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if body.confirm != "FACTORY_RESET" {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "confirmation required: confirm must equal \"FACTORY_RESET\""
            })),
        ));
    }
    let cleared = state
        .open_sockets
        .reset_configuration(&state.registry, || {
            state
                .voice
                .factory_reset_configuration(body.include_secrets)
        })
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?;
    state.auth.reset_after_factory_reset().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    let (data_epoch, revision) = state
        .store
        .with(|c| {
            Ok(c.query_row(
                "SELECT data_epoch, revision FROM config_head WHERE id=1",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )?)
        })
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?;
    state.tasks.reset();
    state.working_memory.reset();
    remove_dir_if_exists(&state.data_dir.join("assets").join("wake-acks"));
    remove_dir_if_exists(&state.data_dir.join("voice-events"));
    tracing::warn!(
        include_secrets = body.include_secrets,
        data_epoch,
        revision,
        "core factory reset executed from console"
    );
    Ok(Json(json!({
        "state": "reset",
        "cleared": cleared,
        "secrets_preserved": !body.include_secrets,
        "data_epoch": data_epoch,
        "revision": revision,
    })))
}

#[derive(Deserialize)]
pub(super) struct ServicePut {
    provider: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    voice: String,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
}

pub(super) async fn config_service_put(
    State(state): State<Arc<AppState>>,
    Path(svc): Path<String>,
    Json(body): Json<ServicePut>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if svc == "llm" && body.provider == "deepseek" && body.base_url.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "base_url required" })),
        ));
    }
    let tools_before = service_fingerprint(&state);
    let stored_env = match svc.as_str() {
        "llm" => state
            .config
            .load()
            .map(|c| c.llm.api_key_env)
            .unwrap_or_default(),
        "llm_fallback" => state
            .config
            .load()
            .map(|c| c.llm_fallback.api_key_env)
            .unwrap_or_default(),
        "asr" => state
            .config
            .load()
            .map(|c| c.asr.api_key_env)
            .unwrap_or_default(),
        "tts" => state
            .config
            .load()
            .map(|c| c.tts.api_key_env)
            .unwrap_or_default(),
        _ => String::new(),
    };
    let cfg = ServiceConfig {
        provider: body.provider,
        base_url: body.base_url,
        model: body.model,
        api_key_env: body.api_key_env.unwrap_or(stored_env),
        voice: body.voice,
        thinking: body.thinking.unwrap_or_else(|| "disabled".into()),
        language: body.language.unwrap_or_default().trim().to_string(),
    };
    match state
        .config
        .save_service(&svc, cfg, body.api_key.as_deref(), "console")
    {
        Ok(st) => {
            reset_device_history_if_capabilities_changed(&state, tools_before);
            Ok(Json(json!({ "state": "applied", "service": st })))
        }
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )),
    }
}

pub(super) async fn config_service_test(
    State(state): State<Arc<AppState>>,
    Path(svc): Path<String>,
) -> Json<Value> {
    let runtime = match state.config.runtime_snapshot() {
        Ok(runtime) => runtime,
        Err(error) => return Json(json!({"ok":false,"error":error})),
    };
    match svc.as_str() {
        "llm" | "llm_fallback" => {
            let cfg = if svc == "llm" {
                &runtime.config.llm
            } else {
                &runtime.config.llm_fallback
            };
            match runtime.llm(&svc) {
                Ok(client) => match client.ping(&cfg.model).await {
                    Ok((text, ms)) => Json(json!({ "ok": true, "reply": text, "latency_ms": ms })),
                    Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
                },
                Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
            }
        }
        "tts" => {
            let started = std::time::Instant::now();
            match runtime.tts() {
                Ok(client) => match client.synthesize("连接测试").await {
                    Ok(audio) => Json(json!({
                        "ok": true,
                        "latency_ms": started.elapsed().as_millis() as u64,
                        "audio_ms": audio.duration_ms,
                        "bytes": audio.wav.len(),
                        "provider": audio.provider,
                        "voice": audio.voice,
                    })),
                    Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
                },
                Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
            }
        }
        "asr" => {
            let started = std::time::Instant::now();
            match runtime.asr() {
                Ok(client) => {
                    const SAMPLE: &[u8] = include_bytes!("../../../assets/asr-sample-zh.wav");
                    match client.transcribe(SAMPLE).await {
                        Ok(text) => Json(json!({
                            "ok": true,
                            "text": text,
                            "latency_ms": started.elapsed().as_millis() as u64,
                            "provider": client.provider(),
                        })),
                        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
                    }
                }
                Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
            }
        }
        _ => Json(json!({ "ok": false, "error": "unknown service" })),
    }
}
