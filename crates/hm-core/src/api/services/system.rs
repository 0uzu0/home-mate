use crate::api::*;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub(in crate::api) async fn config_history(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({ "history": state.config.history(50).unwrap_or_default() }))
}

/// Restore one config-history row (M3). Restore is a CONFIG operation:
/// values re-enter through the same validated save paths with actor
/// "restore"; it never replays device actions (plays/volumes/stops are task
/// history, not configuration). Keys are never stored, hence never restored.
pub(in crate::api) async fn config_history_restore(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let entry = state
        .config
        .history_entry(id)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("no config history row {id}") })),
            )
        })?;
    let scope = entry["scope"].as_str().unwrap_or_default().to_string();
    let payload = entry["payload"].clone();
    match scope.as_str() {
        "llm" | "asr" | "tts" => {
            if payload.get("provider").is_none() {
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({ "error": "该历史行没有可恢复的配置快照（M3 之前的旧格式行）" })),
                ));
            }
            let cfg = crate::config::ServiceConfig {
                provider: payload["provider"].as_str().unwrap_or_default().into(),
                base_url: payload["base_url"].as_str().unwrap_or_default().into(),
                model: payload["model"].as_str().unwrap_or_default().into(),
                api_key_env: payload["api_key_env"].as_str().unwrap_or_default().into(),
                voice: payload["voice"].as_str().unwrap_or_default().into(),
                // Pre-thinking-field snapshots restore as "disabled" (the
                // voice default), not the old high-effort behavior.
                thinking: payload["thinking"].as_str().unwrap_or("disabled").into(),
            };
            match state.config.save_service(&scope, cfg, None, "restore") {
                Ok(st) => Ok(Json(json!({
                    "state": "applied", "scope": scope, "restored": st,
                    "note": "api_key 未随恢复变更（密钥不进历史；如需更换请在服务页设置）",
                }))),
                Err(e) => Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": e.to_string() })),
                )),
            }
        }
        "device" => {
            let device = payload["device"].as_str().ok_or_else(|| {
                (
                    StatusCode::CONFLICT,
                    Json(json!({ "error": "device 快照缺少 device 字段" })),
                )
            })?;
            let name = payload["name"].as_str().unwrap_or_default();
            let room = payload["room"].as_str().unwrap_or_default();
            match state.config.save_device_meta(device, name, room, "restore") {
                Ok(meta) => Ok(Json(json!({
                    "state": "applied", "scope": "device", "restored": meta,
                    "note": "仅恢复显示名与房间；不改设备连接与会话",
                }))),
                Err(e) => Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": e.to_string() })),
                )),
            }
        }
        "voice" => {
            let settings: crate::voice::VoiceSettings = serde_json::from_value(payload.clone())
                .map_err(|e| {
                    (
                        StatusCode::CONFLICT,
                        Json(json!({ "error": format!("voice 快照无法解析：{e}") })),
                    )
                })?;
            match state
                .voice
                .restore_settings_document(settings, &payload, "restore")
            {
                Ok(snap) => {
                    // Same reason as the import path: a restored snapshot can
                    // carry different retreat/window-close/wake acks, and those
                    // play from pre-synthesized WAVs (2026-11-15).
                    state.voice.spawn_ack_synthesis();
                    Ok(Json(json!({
                        "state": "applied", "scope": "voice", "restored": snap,
                        "note": "恢复仅重放配置：重新校验并重发 mode_set；不重放播放/音量/停止",
                    })))
                }
                Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
            }
        }
        other => Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("scope '{other}' 不支持恢复") })),
        )),
    }
}

// --- configuration backup (R1f): one restorable file --------------------------

pub const CONFIRM_SECRETS_HEADER: &str = "x-hm-confirm-secrets";
pub const CONFIRM_SECRETS_VALUE: &str = "EXPORT_SECRETS";

#[derive(Deserialize)]
pub(in crate::api) struct ExportQuery {
    /// Include secret VALUES (api keys, music secrets). Off by default: an
    /// export must not hand out credentials unless asked, and the backup
    /// tool/console opts in explicitly with confirmation. Never logged, never
    /// in history.
    #[serde(default)]
    secrets: Option<String>,
    #[serde(default)]
    with_secrets: Option<String>,
    #[serde(default)]
    include_secrets: Option<String>,
}

fn wants_secrets(q: Option<&str>) -> bool {
    matches!(q, Some("1" | "true" | "yes"))
}

fn service_snapshot(cfg: &ServiceConfig, key_set: bool) -> Value {
    let mut v = serde_json::to_value(cfg).unwrap_or_else(|_| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("api_key_set".into(), json!(key_set));
    }
    v
}

/// The export document, built in one place so `GET /config/export` and a fleet
/// push cannot drift apart: whatever a household backs up is exactly what it
/// can send to another node.
///
/// `include_secrets` puts the ten household credentials in the document in
/// plaintext. Both callers are explicit about it (the backup tool's `?secrets=1`
/// and the console's "include keys" checkbox, which is off by default).
pub fn build_config_document(state: &AppState, include_secrets: bool) -> Result<Value, String> {
    let services = state.config.load().map_err(|e| e.to_string())?;
    let music = state
        .config
        .music_snapshot(true)
        .map_err(|e| e.to_string())?;
    let mut music_secrets = if include_secrets {
        [music.library.clone(), music.online.clone()]
    } else {
        [None, None]
    };
    let music = music.status();
    let home = state.config.home_snapshot().unwrap_or_default();
    let home_cfg = home.config;
    let mut home_token = home.token;
    let home_url = if crate::config::HomeConfig::endpoint_is_portable(&home_cfg.base_url) {
        home_cfg.base_url.as_str()
    } else {
        ""
    };
    let weather = state.config.weather_status().map_err(|e| e.to_string())?;
    let openapi = state.config.openapi_snapshot().map_err(|e| e.to_string())?;
    let mut openapi_token = if include_secrets {
        openapi.token.clone()
    } else {
        None
    };
    let openapi = openapi.status();
    let key_set = |svc: &str| {
        state
            .config
            .status(svc)
            .map(|s| s.api_key_set)
            .unwrap_or(false)
    };
    let reflex = state.config.reflex().unwrap_or_default();
    let audio_eq = state.config.audio_eq().unwrap_or_default();
    let mut doc = json!({
        "format": "home-mate-config",
        "version": 1,
        "exported_ms": crate::id::now_ms(),
        "services": {
            "llm": service_snapshot(&services.llm, key_set("llm")),
            "llm_fallback": service_snapshot(&services.llm_fallback, key_set("llm_fallback")),
            "asr": service_snapshot(&services.asr, key_set("asr")),
            "tts": service_snapshot(&services.tts, key_set("tts")),
        },
        "music": music,
        "weather": weather,
        "home": {
            "backend": home_cfg.backend,
            // Audit 2026-11-14: the source (API directory vs Assist exposure)
            // was missing, so an Assist setup did not survive a restore.
            "entity_source": home_cfg.entity_source,
            "base_url": home_url,
            "entities": home_cfg.entities,
            "timeout_ms": home_cfg.timeout_ms,
            "retries": home_cfg.retries,
            "token_set": home_token.is_some(),
        },
        "news": state.config.news_status().map_err(|e| e.to_string())?,
        "search": crate::tool::search::status_json(&state.search),
        "openapi": {
            "enabled": openapi.enabled,
            "spec_url": openapi.spec_url,
            "base_url": openapi.base_url,
            "auth_header": openapi.auth_header,
            "operations": openapi.operations,
            "timeout_ms": openapi.timeout_ms,
            "retries": openapi.retries,
            "token_set": openapi.token_set,
        },
        // M12a-1 (v0.26): the reflex table and the output tone travel too —
        // a second node that gets the household's vocabulary minus its wake
        // word is a working node, and the console's audio curve is part of the
        // sound a household chose.
        "reflex": { "words": reflex.words },
        "audio_eq": { "curve": audio_eq.curve, "bands": audio_eq.bands },
        "devices": state.config.device_meta().unwrap_or_default(),
        "mode": state.voice.settings_snapshot(),
    });
    let (node, source) = state.fleet.self_id(None);
    if source != "default" {
        if let Some(mut remote) = state.remote_socket.export().map_err(|error| error.detail)? {
            remote["source_node"] = json!(node);
            doc["remote_socket"] = remote;
        }
        if let Some(sockets) =
            crate::config_delivery::ConfigDelivery::open_sockets_export(state, &node)
        {
            doc["open_sockets"] = json!(sockets);
            doc["open_sockets_source_node"] = json!(node);
        }
    }
    if include_secrets {
        let mut map = serde_json::Map::new();
        for name in crate::config::CONFIG_SECRETS {
            let value = match name {
                crate::config::HOME_TOKEN_SECRET => home_token.take(),
                crate::config::MUSIC_LIBRARY_SECRET => music_secrets[0].take(),
                crate::config::MUSIC_ONLINE_SECRET => music_secrets[1].take(),
                crate::config::OPENAPI_TOKEN_SECRET => openapi_token.take(),
                _ => state.store.secret_get(name).ok().flatten(),
            };
            if let Some(value) = value {
                map.insert(name.to_string(), Value::String(value));
            }
        }
        doc["secrets"] = Value::Object(map);
    }
    Ok(doc)
}

pub(in crate::api) async fn config_export(
    method: axum::http::Method,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<ExportQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let include_secrets = wants_secrets(q.secrets.as_deref())
        || wants_secrets(q.with_secrets.as_deref())
        || wants_secrets(q.include_secrets.as_deref());
    if include_secrets {
        if method != axum::http::Method::POST {
            return Err((
                StatusCode::METHOD_NOT_ALLOWED,
                Json(json!({
                    "error": "method_not_allowed",
                    "code": "SECRET_EXPORT_POST_REQUIRED",
                    "detail": "exporting plaintext secrets requires POST /api/v0/config/export with X-HM-Confirm-Secrets: EXPORT_SECRETS"
                })),
            ));
        }
        let header_ok = headers
            .get(CONFIRM_SECRETS_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim() == CONFIRM_SECRETS_VALUE)
            .unwrap_or(false);
        if !header_ok {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "secret_export_confirmation_required",
                    "code": "SECRET_EXPORT_CONFIRMATION_REQUIRED",
                    "detail": "exporting plaintext secrets requires header X-HM-Confirm-Secrets: EXPORT_SECRETS"
                })),
            ));
        }
    }
    build_config_document(&state, include_secrets)
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
        })
}

#[derive(Deserialize)]
pub struct ConfigImportBody {
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub version: Option<u32>,
    #[serde(default)]
    pub services: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    pub music: Option<Value>,
    #[serde(default)]
    pub weather: Option<Value>,
    /// Home automation (R1j/P0-b): { backend: "none"|"home_assistant" }.
    #[serde(default)]
    pub home: Option<Value>,
    /// News feeds (R2-BM6): { enabled, sources, default_alias, timeout_ms }.
    #[serde(default)]
    pub news: Option<Value>,
    /// General web search (M2.5): { enabled, base_url, count, timeout_ms }.
    /// The key travels in `secrets` only (`?secrets=1` backups).
    #[serde(default)]
    pub search: Option<Value>,
    /// OpenAPI adapter (R2-BM6). Import validates the stored catalogue
    /// STRUCTURALLY (GET only, local paths); it never makes an HTTP request.
    #[serde(default)]
    pub openapi: Option<Value>,
    /// Node-local remote socket settings. Its credential is always kept on the target.
    #[serde(default)]
    pub remote_socket: Option<Value>,
    #[serde(default)]
    pub open_sockets: Option<Value>,
    #[serde(default)]
    pub open_sockets_source_node: Option<String>,
    /// The reflex vocabulary (M12a-1): { words: [{text,tokens,intent,enabled}] }.
    /// A fleet push never carries wake-intent words — see `apply_config_import`.
    #[serde(default)]
    pub reflex: Option<Value>,
    /// The output tone (M12a-1): { curve, bands }. Unknown curves are refused
    /// by `save_audio_eq`, so a push cannot install a curve the device would
    /// silently clamp.
    #[serde(default)]
    pub audio_eq: Option<Value>,
    /// Household labels: { stable_id: { name, room } }.
    #[serde(default)]
    pub devices: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    pub mode: Option<Value>,
    #[serde(default)]
    pub mode_policy: Option<Value>,
    #[serde(default)]
    pub secrets: Option<serde_json::Map<String, Value>>,
    /// Per-unit acoustic calibration parameters. Explicitly refused/ignored on
    /// import and fleet push so one unit's calibration never overwrites another.
    #[serde(default)]
    pub unit_acoustic_profiles: Option<Value>,
}

/// `X-Home-Mate-Source`: who sent a config import. Present only on a fleet
/// push; every write it causes is recorded as `actor=fleet:<source>` so the
/// receiving device's history says who changed it, not just "import".
pub const FLEET_SOURCE_HEADER: &str = "x-home-mate-source";

/// Stable ids are dot2-d style. Anything else is refused rather than
/// sanitized: an actor string appears in history and must be recognisable.
fn valid_source_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn fleet_actor(source: Option<&str>) -> Result<String, (StatusCode, Json<Value>)> {
    match source {
        None => Ok("import".into()),
        Some(s) => {
            let s = s.trim();
            if valid_source_id(s) {
                Ok(format!("fleet:{s}"))
            } else {
                Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!("bad {FLEET_SOURCE_HEADER} '{s}': use [A-Za-z0-9._-]{{1,64}}")
                    })),
                ))
            }
        }
    }
}

pub(in crate::api) async fn config_import(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ConfigImportBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let source = headers
        .get(FLEET_SOURCE_HEADER)
        .and_then(|v| v.to_str().ok());
    let actor = fleet_actor(source)?;
    let out = apply_config_import(&state, body, &actor).await?;
    Ok(Json(json!({
        "state": "applied",
        "actor": actor,
        "applied": out.applied,
        "warnings": out.warnings,
    })))
}

/// What one import changed (per block) and what it refused to guess about.
pub struct ImportOutcome {
    pub applied: Vec<String>,
    pub warnings: Vec<String>,
}

struct ImportRollbackCheckpoint {
    config_kv: Vec<(String, String)>,
    secrets: Vec<(String, String)>,
    config_head: Option<(i64, i64, String, String, String, String)>,
    max_history_id: i64,
    voice_settings: crate::voice::VoiceSettings,
}

fn capture_import_checkpoint(state: &AppState) -> Option<ImportRollbackCheckpoint> {
    use rusqlite::OptionalExtension;
    let voice_settings = state.voice.settings_snapshot();
    state
        .store
        .with(|c| {
            let mut kv_stmt = c.prepare("SELECT key, value FROM config_kv")?;
            let config_kv = kv_stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut sec_stmt = c.prepare("SELECT name, value FROM secrets")?;
            let secrets = sec_stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let config_head = c
                .query_row(
                    "SELECT revision, data_epoch, household_id, node_id, layers_json, resolved_json FROM config_head WHERE id=1",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, String>(5)?,
                        ))
                    },
                )
                .optional()?;
            let max_history_id: i64 = c.query_row(
                "SELECT COALESCE(MAX(id), 0) FROM config_history",
                [],
                |r| r.get(0),
            )?;
            Ok(ImportRollbackCheckpoint {
                config_kv,
                secrets,
                config_head,
                max_history_id,
                voice_settings,
            })
        })
        .ok()
}

fn restore_import_checkpoint(state: &AppState, cp: ImportRollbackCheckpoint) {
    let _ = state.store.with(|c| {
        let tx = rusqlite::Transaction::new_unchecked(c, rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM config_kv", [])?;
        for (k, v) in &cp.config_kv {
            tx.execute(
                "INSERT INTO config_kv(key, value) VALUES (?1, ?2)",
                rusqlite::params![k, v],
            )?;
        }
        tx.execute("DELETE FROM secrets", [])?;
        for (k, v) in &cp.secrets {
            tx.execute(
                "INSERT INTO secrets(name, value) VALUES (?1, ?2)",
                rusqlite::params![k, v],
            )?;
        }
        if let Some((rev, epoch, hh, node, layers, resolved)) = &cp.config_head {
            tx.execute(
                "UPDATE config_head SET revision=?1, data_epoch=?2, household_id=?3, node_id=?4, layers_json=?5, resolved_json=?6 WHERE id=1",
                rusqlite::params![rev, epoch, hh, node, layers, resolved],
            )?;
        }
        tx.execute("DELETE FROM config_history WHERE id > ?1", [cp.max_history_id])?;
        tx.commit()?;
        Ok(())
    });
    state.voice.overwrite_in_memory_settings(cp.voice_settings);
}

fn bad_section(sec: &str, e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": format!("{sec}: {e}") })),
    )
}

fn prevalidate_config_import(
    state: &AppState,
    body: &ConfigImportBody,
    fleet: bool,
    remote_restore_node: Option<&str>,
) -> Result<(), (StatusCode, Json<Value>)> {
    let allow_loopback = state.store.allows_loopback_public();
    let secret = |name: &str| {
        body.secrets
            .as_ref()
            .and_then(|m| m.get(name))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    if let Some(policy) = &body.mode_policy {
        state
            .voice
            .prevalidate_settings_patch(policy)
            .map_err(|e| bad_section("mode", e))?;
    } else if let Some(mode) = &body.mode {
        let mut mode = mode.clone();
        if fleet {
            if let Some(object) = mode.as_object_mut() {
                for &key in crate::fleet::NODE_LOCAL_MODE_KEYS {
                    object.remove(key);
                }
            }
        }
        let settings: crate::voice::VoiceSettings =
            serde_json::from_value(mode).map_err(|e| bad_section("mode", e))?;
        state
            .voice
            .prevalidate_settings_restore(settings, fleet)
            .map_err(|e| bad_section("mode", e))?;
    }
    if let Some(devices) = &body.devices {
        for (id, value) in devices {
            let sec = format!("devices.{id}");
            let meta: crate::config::DeviceMeta =
                serde_json::from_value(value.clone()).map_err(|e| bad_section(&sec, e))?;
            crate::config::ConfigService::validate_device_meta_entry(id, &meta.name, &meta.room)
                .map_err(|e| bad_section(&sec, e))?;
        }
    }
    if let Some(services) = &body.services {
        for (name, value) in services {
            if !matches!(name.as_str(), "llm" | "llm_fallback" | "asr" | "tts") {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("unknown service '{name}'") })),
                ));
            }
            let _cfg: ServiceConfig =
                serde_json::from_value(value.clone()).map_err(|e| bad_section(name, e))?;
        }
    }
    if let Some(music) = &body.music {
        let cfg: crate::config::MusicConfig =
            serde_json::from_value(music.clone()).map_err(|e| bad_section("music", e))?;
        cfg.normalized_with(allow_loopback)
            .map_err(|e| bad_section("music", e))?;
    }
    if let Some(home) = &body.home {
        let cfg: crate::config::HomeConfig =
            serde_json::from_value(home.clone()).map_err(|e| bad_section("home", e))?;
        cfg.normalized_with(allow_loopback)
            .map_err(|e| bad_section("home", e))?;
    }
    if let Some(weather) = &body.weather {
        let cfg: crate::config::WeatherConfig =
            serde_json::from_value(weather.clone()).map_err(|e| bad_section("weather", e))?;
        cfg.normalized(allow_loopback)
            .map_err(|e| bad_section("weather", e))?;
    }
    if let Some(news) = &body.news {
        let cfg: crate::config::NewsConfig =
            serde_json::from_value(news.clone()).map_err(|e| bad_section("news", e))?;
        cfg.normalized(allow_loopback)
            .map_err(|e| bad_section("news", e))?;
    }
    if let Some(search) = &body.search {
        let cfg: crate::config::SearchConfig =
            serde_json::from_value(search.clone()).map_err(|e| bad_section("search", e))?;
        cfg.normalized(allow_loopback)
            .map_err(|e| bad_section("search", e))?;
    }
    if let Some(openapi) = &body.openapi {
        let mut cfg: crate::config::OpenApiConfig =
            serde_json::from_value(openapi.clone()).map_err(|e| bad_section("openapi", e))?;
        let token = secret(crate::config::OPENAPI_TOKEN_SECRET);
        let captured = state
            .config
            .openapi_snapshot()
            .map_err(|e| bad_section("openapi", e))?;
        if openapi.get("auth_header").is_none() {
            cfg.auth_header = captured.config.auth_header;
            let credentialed = token
                .as_deref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(captured.token.is_some() || openapi["token_set"] == true);
            if cfg.enabled && credentialed && cfg.auth_header.is_empty() {
                return Err(bad_section("openapi", "OPENAPI_AUTH_HEADER_CHOICE: add auth_header or explicitly use an empty string for Bearer only"));
            }
        }
        if captured
            .token
            .as_deref()
            .into_iter()
            .chain(token.as_deref())
            .any(|tok| cfg.contains_token(tok))
        {
            return Err(bad_section("openapi", "OPENAPI_SECRET_IN_PUBLIC_CONFIG"));
        }
        cfg.normalized(allow_loopback)
            .map_err(|e| bad_section("openapi", e))?;
    }
    if let (Some(remote), Some(_node)) = (&body.remote_socket, remote_restore_node) {
        let fail = |error: crate::config_delivery::DeliveryError| {
            (
                StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(json!({"error": error.detail, "code": error.code, "pointer": error.pointer})),
            )
        };
        let mut config: crate::tool::remote_socket::RemoteSocketConfig =
            serde_json::from_value(remote.clone()).map_err(|error| fail(error.into()))?;
        crate::tool::remote_socket::validate_remote_config(&mut config).map_err(|detail| {
            fail(
                crate::config_delivery::DeliveryError::new("INVALID_REMOTE_SOCKET_CONFIG", detail)
                    .at("remote_socket"),
            )
        })?;
        let _ = state.remote_socket.snapshot().map_err(fail)?;
    }
    if let Some(reflex) = &body.reflex {
        let incoming: crate::config::ReflexConfig =
            serde_json::from_value(reflex.clone()).map_err(|e| bad_section("reflex", e))?;
        let current = state.config.reflex().unwrap_or_default();
        let words = if fleet {
            let mut non_wake: Vec<crate::config::ReflexWord> = incoming
                .words
                .into_iter()
                .filter(|w| w.intent != crate::config::ReflexIntent::Wake)
                .collect();
            if non_wake.is_empty() {
                current.words.clone()
            } else {
                let mut merged: Vec<crate::config::ReflexWord> = current
                    .words
                    .iter()
                    .filter(|w| w.intent == crate::config::ReflexIntent::Wake)
                    .cloned()
                    .collect();
                merged.append(&mut non_wake);
                merged
            }
        } else {
            incoming.words
        };
        if words != current.words {
            crate::config::ConfigService::validate_reflex_config(crate::config::ReflexConfig {
                words,
            })
            .map_err(|e| bad_section("reflex", e))?;
        }
    }
    if let Some(audio_eq) = &body.audio_eq {
        let cfg: crate::config::AudioEqConfig =
            serde_json::from_value(audio_eq.clone()).map_err(|e| bad_section("audio_eq", e))?;
        cfg.validate().map_err(|e| bad_section("audio_eq", e))?;
    }
    Ok(())
}

/// Apply a backup/push document. Split out of the handler so a fleet push on
/// the sending side and an import on the receiving side share one
/// implementation (and the acceptance tests can drive either).
pub async fn apply_config_import(
    state: &AppState,
    body: ConfigImportBody,
    actor: &str,
) -> Result<ImportOutcome, (StatusCode, Json<Value>)> {
    let fleet = actor.starts_with("fleet:");
    if let Some(f) = &body.format {
        if f != "home-mate-config" {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("unsupported backup format '{f}'") })),
            ));
        }
    }
    if let Some(v) = body.version {
        if v != 1 {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("unsupported backup version {v}") })),
            ));
        }
    }
    let remote_restore_node = if let Some(remote) = body.remote_socket.as_ref().filter(|_| !fleet) {
        let (node, source) = state.fleet.self_id(None);
        if source == "default" || remote["source_node"].as_str() != Some(&node) {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({
                    "error":"Remote socket backups can only be restored on their source node.",
                    "code":"REMOTE_SOCKET_RESTORE_TARGET_MISMATCH"
                })),
            ));
        }
        Some(node)
    } else {
        None
    };
    let open_sockets_error = |error: crate::config_delivery::DeliveryError| {
        (
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({"error": error.detail, "code": error.code, "pointer": error.pointer})),
        )
    };
    let open_sockets_restore = if let Some(value) = body.open_sockets.as_ref().filter(|_| !fleet) {
        let (node, source) = state.fleet.self_id(None);
        if source == "default" || body.open_sockets_source_node.as_deref() != Some(&node) {
            return Err(open_sockets_error(
                crate::config_delivery::ConfigDelivery::open_sockets_restore_mismatch(),
            ));
        }
        let desired = crate::config_delivery::ConfigDelivery::prepare_open_sockets_restore(
            state,
            value.clone(),
            &node,
        )
        .map_err(open_sockets_error)?;
        Some((node, desired))
    } else {
        None
    };
    let secret = |name: &str| {
        body.secrets
            .as_ref()
            .and_then(|m| m.get(name))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let mut applied: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    if body.unit_acoustic_profiles.is_some() {
        warnings.push(
            "unit_acoustic_profiles: ignored (acoustic parameters are measured per unit and never copied across units)".into(),
        );
    }

    if body.mode.is_some() && body.mode_policy.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"choose either a legacy mode snapshot or a reviewed mode policy"})),
        ));
    }

    // Mode validation takes place before any other import writes.
    let mode_error = |error: String| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("mode: {error}")})),
        )
    };
    if fleet {
        if let Some(policy_obj) = body.mode_policy.as_ref().and_then(Value::as_object) {
            let bad_key = policy_obj
                .contains_key("system_prompt_override")
                .then_some("system_prompt_override")
                .or_else(|| {
                    crate::fleet::NODE_LOCAL_MODE_KEYS
                        .iter()
                        .copied()
                        .find(|k| policy_obj.contains_key(*k))
                });
            if let Some(bad_key) = bad_key {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!("a fleet mode_policy cannot write node-local field '{bad_key}'"),
                        "code": "FORBIDDEN_SCOPE"
                    })),
                ));
            }
        }
    }

    // Pre-validate every present block before the first write so any invalid
    // section fails with zero partial modifications.
    prevalidate_config_import(state, &body, fleet, remote_restore_node.as_deref())?;
    let checkpoint = capture_import_checkpoint(state);

    let write_res: Result<(), (StatusCode, Json<Value>)> = async {
        if let Some(policy) = &body.mode_policy {
            state.voice.restore_settings_patch(policy, actor).map_err(mode_error)?;
            applied.push("mode".into());
        } else if let Some(mode) = &body.mode {
            let mut mode = mode.clone();
            if fleet {
                if let Some(object) = mode.as_object_mut() {
                    let current_json = serde_json::to_value(state.voice.settings_snapshot())
                        .unwrap_or(Value::Null);
                    let mut ignored_other: Vec<&'static str> = Vec::new();
                    for &key in crate::fleet::NODE_LOCAL_MODE_KEYS {
                        if let Some(sent) = object.remove(key) {
                            let current_val = current_json.get(key).unwrap_or(&Value::Null);
                            if key == "wake_words" {
                                if &sent != current_val {
                                    warnings.push(
                                        "mode: the push carried wake_words; this node's own list was kept"
                                            .into(),
                                    );
                                }
                            } else if &sent != current_val {
                                ignored_other.push(key);
                            }
                        }
                    }
                    if !ignored_other.is_empty() {
                        warnings.push(format!(
                            "mode: ignored node-local field(s) [{}]; this node's own values were kept",
                            ignored_other.join(", ")
                        ));
                    }
                }
            }
            let settings: crate::voice::VoiceSettings =
                serde_json::from_value(mode).map_err(|error| mode_error(error.to_string()))?;
            if fleet {
                state.voice.restore_settings_from_peer(settings, actor).map_err(mode_error)?;
            } else {
                state.voice.restore_settings(settings, actor).map_err(mode_error)?;
            }
            applied.push("mode".into());
        }

        if let Some(devices) = &body.devices {
            for (id, value) in devices {
                let meta: crate::config::DeviceMeta =
                    serde_json::from_value(value.clone()).map_err(|e| {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": format!("devices.{id}: {e}") })),
                        )
                    })?;
                state
                    .config
                    .save_device_meta(id, &meta.name, &meta.room, actor)
                    .map_err(|e| {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": format!("devices.{id}: {e}") })),
                        )
                    })?;
                applied.push(format!("devices.{id}"));
            }
        }

        if let Some(services) = &body.services {
            for (name, value) in services {
                if !matches!(name.as_str(), "llm" | "llm_fallback" | "asr" | "tts") {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("unknown service '{name}'") })),
                    ));
                }
                let cfg: ServiceConfig = serde_json::from_value(value.clone()).map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("{name}: {e}") })),
                    )
                })?;
                let key = secret(&format!("{name}.api_key"));
                if key.is_none() && value["api_key_set"].as_bool().unwrap_or(false) {
                    warnings.push(format!(
                        "{name}: backup had a stored key but no secret value; the current stored key was kept"
                    ));
                }
                state
                    .config
                    .save_service(name, cfg, key.as_deref(), actor)
                    .map_err(|e| {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": format!("{name}: {e}") })),
                        )
                    })?;
                applied.push(format!("services.{name}"));
            }
        }

        if let Some(music) = &body.music {
            let cfg: crate::config::MusicConfig =
                serde_json::from_value(music.clone()).map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("music: {e}") })),
                    )
                })?;
            let lib = secret("music.library.secret");
            let online = secret("music.online.secret");
            state
                .config
                .save_music(cfg, lib.as_deref(), online.as_deref(), actor)
                .map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("music: {e}") })),
                    )
                })?;
            applied.push("music".into());
        }

        if let Some(home) = &body.home {
            let cfg: crate::config::HomeConfig = serde_json::from_value(home.clone()).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("home: {e}") })),
                )
            })?;
            let token = secret(crate::config::HOME_TOKEN_SECRET);
            state.config.edit_home(token.as_deref(), actor, |current| *current = cfg)
                .map_err(|e| (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("home: {e}") }))))?;
            applied.push("home".into());
        }

        if let Some(weather) = &body.weather {
            let cfg: crate::config::WeatherConfig =
                serde_json::from_value(weather.clone()).map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("weather: {e}") })),
                    )
                })?;
            let key = secret("weather.api_key");
            if key.is_none() && weather["api_key_set"].as_bool().unwrap_or(false) {
                warnings.push(
                    "weather: backup had a stored key but no secret value; the current stored key was kept"
                        .into(),
                );
            }
            state
                .config
                .save_weather(cfg, key.as_deref(), actor)
                .map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("weather: {e}") })),
                    )
                })?;
            applied.push("weather".into());
        }

        if let Some(news) = &body.news {
            let cfg: crate::config::NewsConfig = serde_json::from_value(news.clone()).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("news: {e}") })),
                )
            })?;
            state.config.save_news(cfg, actor).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("news: {e}") })),
                )
            })?;
            applied.push("news".into());
        }

        if let Some(search) = &body.search {
            let cfg: crate::config::SearchConfig =
                serde_json::from_value(search.clone()).map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("search: {e}") })),
                    )
                })?;
            let key = secret(crate::config::SEARCH_SECRET);
            if key.is_none() && search["api_key_set"].as_bool().unwrap_or(false) {
                warnings.push(
                    "search: backup had a stored key but no secret value; the current stored key was kept"
                        .into(),
                );
            }
            state
                .config
                .save_search(cfg, key.as_deref(), actor)
                .map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("search: {e}") })),
                    )
                })?;
            applied.push("search".into());
        }

        if let Some(openapi) = &body.openapi {
            let bad = |error: String| (StatusCode::BAD_REQUEST, Json(json!({"error":format!("openapi: {error}")})));
            let mut cfg: crate::config::OpenApiConfig =
                serde_json::from_value(openapi.clone()).map_err(|error| bad(error.to_string()))?;
            let token = secret(crate::config::OPENAPI_TOKEN_SECRET);
            let captured = state.config.openapi_snapshot().map_err(|error| bad(error.to_string()))?;
            if openapi.get("auth_header").is_none() {
                cfg.auth_header = captured.config.auth_header;
                let credentialed = token.as_deref().map(|value| !value.trim().is_empty())
                    .unwrap_or(captured.token.is_some() || openapi["token_set"] == true);
                if cfg.enabled && credentialed && cfg.auth_header.is_empty() {
                    return Err(bad("OPENAPI_AUTH_HEADER_CHOICE: add auth_header or explicitly use an empty string for Bearer only".into()));
                }
                warnings.push("openapi: legacy backup omitted auth_header; current header was kept".into());
            }
            if token.is_none() && openapi["token_set"] == true {
                warnings.push("openapi: backup had a stored token but no secret value; the current stored token was kept".into());
            }
            state.config.save_openapi_expected(cfg, token.as_deref(), actor, Some(&captured.revision))
                .map_err(|error| bad(error.to_string()))?;
            applied.push("openapi".into());
        }

        if let Some(remote) = &body.remote_socket {
            if let Some(node) = remote_restore_node.as_deref() {
                let fail = |error: crate::config_delivery::DeliveryError| {
                    (StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                        Json(json!({"error": error.detail, "code": error.code, "pointer": error.pointer})))
                };
                let config = serde_json::from_value(remote.clone())
                    .map_err(|error| fail(error.into()))?;
                let baseline = state.remote_socket.snapshot().map_err(fail)?;
                state.remote_socket.restore(config, actor, &baseline.revision, node).map_err(fail)?;
                if remote["token_set"] == true && baseline.token.is_none() {
                    warnings.push("remote_socket: configure the target's local credential".into());
                }
                applied.push("remote_socket".into());
            } else {
                warnings.push("remote_socket: node-local settings were kept".into());
            }
        }

        if let Some(reflex) = &body.reflex {
            let incoming: crate::config::ReflexConfig = serde_json::from_value(reflex.clone())
                .map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("reflex: {e}") })),
                    )
                })?;
            let current = state.config.reflex().unwrap_or_default();
            let words = if fleet {
                let mut non_wake: Vec<crate::config::ReflexWord> = incoming
                    .words
                    .into_iter()
                    .filter(|w| w.intent != crate::config::ReflexIntent::Wake)
                    .collect();
                if non_wake.is_empty() {
                    warnings.push(
                        "reflex: the push carried no non-wake words; the current table was kept".into(),
                    );
                    current.words.clone()
                } else {
                    let mut merged: Vec<crate::config::ReflexWord> = current
                        .words
                        .iter()
                        .filter(|w| w.intent == crate::config::ReflexIntent::Wake)
                        .cloned()
                        .collect();
                    merged.append(&mut non_wake);
                    merged
                }
            } else {
                incoming.words
            };
            if words != current.words {
                let cfg = crate::config::ReflexConfig { words };
                state.config.save_reflex(cfg, actor).map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("reflex: {e}") })),
                    )
                })?;
                applied.push("reflex".into());
                match state.hub.primary_audio_out() {
                    Some(device) => {
                        if let Err(e) = state.voice.deploy_reflex_auto(&device.stable_id).await {
                            warnings.push(format!(
                                "reflex: saved, but the spotter on {} was not restarted: {e}",
                                device.stable_id
                            ));
                        }
                    }
                    None => warnings.push(
                        "reflex: saved, but no audio device is online to load it (deploys on next start)"
                            .into(),
                    ),
                }
            }
        }

        if let Some(audio_eq) = &body.audio_eq {
            let cfg: crate::config::AudioEqConfig =
                serde_json::from_value(audio_eq.clone()).map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("audio_eq: {e}") })),
                    )
                })?;
            let current = state.config.audio_eq().unwrap_or_default();
            if cfg != current {
                state.config.save_audio_eq(cfg, actor).map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("audio_eq: {e}") })),
                    )
                })?;
                applied.push("audio_eq".into());
                for device in state.hub.audio_out_devices() {
                    if let Err(e) = state.voice.push_audio_eq(&device) {
                        warnings.push(format!("audio_eq: {device}: {e}"));
                    }
                }
            }
        }

        if body.mode.is_some() {
            state.voice.spawn_ack_synthesis();
        }

        if let Some((source, desired)) = open_sockets_restore {
            crate::config_delivery::ConfigDelivery::import_open_sockets(state, desired, &source, actor)
                .map_err(open_sockets_error)?;
            applied.push("open_sockets".into());
        } else if body.open_sockets.is_some() {
            warnings.push("open_sockets: node-local settings were kept".into());
        }
        Ok(())
    }
    .await;

    if let Err(err) = write_res {
        if let Some(cp) = checkpoint {
            restore_import_checkpoint(state, cp);
        }
        return Err(err);
    }

    Ok(ImportOutcome { applied, warnings })
}

// --- fleet: peers and configuration push (M12a-1, v0.26) ----------------------

fn node_self_id(state: &AppState) -> (String, &'static str) {
    let hint = state.hub.primary_audio_out().map(|d| d.stable_id);
    state.fleet.self_id(hint.as_deref())
}

pub(in crate::api) async fn fleet_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    let (id, source) = node_self_id(&state);
    let mut out = state.fleet.snapshot(&id, source);
    out["push_actor"] = json!(format!("fleet:{id}"));
    Json(out)
}

#[derive(Deserialize)]
pub(in crate::api) struct FleetSelfBody {
    #[serde(default)]
    self_id: Option<String>,
}

pub(in crate::api) async fn fleet_self_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<FleetSelfBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if let Some(id) = &body.self_id {
        state.fleet.set_self_id(id).map_err(bad_request)?;
    }
    let (id, source) = node_self_id(&state);
    let mut out = state.fleet.snapshot(&id, source);
    out["push_actor"] = json!(format!("fleet:{id}"));
    Ok(Json(out))
}

#[derive(Deserialize)]
pub(in crate::api) struct FleetNodeBody {
    #[serde(default)]
    base_url: String,
    /// Write-only, like every other secret: absent = keep the stored token,
    /// present and non-empty = replace, `token_clear` = remove.
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    token_clear: bool,
}

pub(in crate::api) async fn fleet_node_put(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<FleetNodeBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let node = state
        .fleet
        .upsert(&id, &body.base_url, body.token.as_deref(), body.token_clear)
        .map_err(bad_request)?;
    Ok(Json(json!({ "state": "saved", "node": node })))
}

pub(in crate::api) async fn fleet_node_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let removed = state.fleet.remove(&id).map_err(bad_request)?;
    if !removed {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no such peer '{id}'") })),
        ));
    }
    Ok(Json(json!({ "state": "removed", "id": id })))
}

#[derive(Deserialize, Default)]
pub(in crate::api) struct FleetPingBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    ids: Option<Vec<String>>,
}

/// Read-only on every peer (`GET /api/v0/status`). No body = ping every
/// configured peer, which is what the console's "probe all" button does.
pub(in crate::api) async fn fleet_ping(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let body: FleetPingBody = if body.is_empty() {
        FleetPingBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| bad_request(format!("body: {e}")))?
    };
    let ids: Vec<String> = match (body.ids, body.id) {
        (Some(ids), _) if !ids.is_empty() => ids,
        (_, Some(id)) => vec![id],
        _ => state.fleet.doc().nodes.into_iter().map(|n| n.id).collect(),
    };
    if ids.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": "no peers configured; add one first" })),
        ));
    }
    let results = state.fleet.ping(&ids).await;
    let ok = results.iter().filter(|r| r["ok"] == json!(true)).count();
    Ok(Json(json!({
        "state": "pinged",
        "results": results,
        "ok": ok,
        "failed": ids.len().saturating_sub(ok),
    })))
}

/// `POST /api/v0/fleet/wake` (v0.30, wake arbitration).
pub(in crate::api) async fn fleet_wake(
    State(state): State<Arc<AppState>>,
    Json(claim): Json<crate::arbitration::Claim>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if claim.device.trim().is_empty() {
        return Err(bad_request("claim has no device"));
    }
    Ok(Json(state.voice.arbiter.inbound(claim)))
}

#[derive(Deserialize)]
pub(in crate::api) struct FleetCueBody {
    device: String,
    phrase: String,
    #[serde(default)]
    at_ms: u64,
}

/// Contract v2 #2 (2026-11-16): a peer tells us it just spoke a cue.
pub(in crate::api) async fn fleet_cue(
    State(state): State<Arc<AppState>>,
    Json(body): Json<FleetCueBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if body.device.trim().is_empty() {
        return Err(bad_request("cue has no device"));
    }
    if body.phrase.trim().is_empty() {
        return Err(bad_request("cue has no phrase"));
    }
    let at_ms = if body.at_ms == 0 {
        crate::id::now_ms()
    } else {
        body.at_ms
    };
    state.voice.note_peer_cue(&body.device, &body.phrase, at_ms);
    Ok(Json(json!({"recorded": true})))
}

/// Wave 2: a peer notifies us of active TTS/media playback (`FleetPlaybackNotice`).
pub(in crate::api) async fn fleet_playback(
    State(state): State<Arc<AppState>>,
    Json(mut notice): Json<crate::sovereignty::FleetPlaybackNotice>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if notice.device.trim().is_empty() {
        return Err(bad_request("playback notice has no device"));
    }
    notice.at_ms = crate::id::now_ms();
    notice.ttl_ms = if notice.ttl_ms == 0 {
        crate::sovereignty::DEFAULT_PLAYBACK_NOTICE_TTL_MS
    } else {
        notice
            .ttl_ms
            .clamp(1_000, crate::sovereignty::DEFAULT_PLAYBACK_NOTICE_TTL_MS)
    };
    state.voice.sovereignty.record_playback_notice(notice);
    Ok(Json(json!({"recorded": true})))
}

/// L4 (`#116` / `ARB-04`): a peer replicates room session ownership commit,
/// renewal, or release (`FleetOwnerNotice`) with monotonic `epoch` fencing.
pub(in crate::api) async fn fleet_owner(
    State(state): State<Arc<AppState>>,
    Json(mut notice): Json<crate::sovereignty::FleetOwnerNotice>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if notice.owner_device.trim().is_empty() {
        return Err(bad_request("owner notice has no owner_device"));
    }
    if notice.epoch == 0 {
        return Err(bad_request("owner notice epoch must be >= 1"));
    }
    notice.lease_ttl_ms = notice.lease_ttl_ms.min(60_000);
    notice.voice_ttl_ms = notice.voice_ttl_ms.min(60_000);
    notice.context_ttl_ms = notice.context_ttl_ms.min(600_000);
    let accepted = state
        .voice
        .sovereignty
        .apply_remote_owner(notice, crate::id::now_ms());
    Ok(Json(json!({ "accepted": accepted })))
}

#[derive(Deserialize)]
pub(in crate::api) struct FleetPushBody {
    #[serde(default)]
    targets: Vec<String>,
    #[serde(default)]
    include_secrets: bool,
    #[serde(default)]
    dry_run: bool,
}

pub(in crate::api) async fn fleet_push(
    State(state): State<Arc<AppState>>,
    Json(body): Json<FleetPushBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if body.targets.is_empty() {
        return Err(bad_request("targets is empty: name at least one peer"));
    }
    let (source, _) = node_self_id(&state);
    let mut doc = build_config_document(&state, body.include_secrets).map_err(bad_request)?;
    // The household's rule, applied at the sender as well as at the receiver:
    // a speaker's own wake word and unit acoustic profile never travel.
    crate::fleet::strip_node_local(&mut doc);
    let results = state
        .fleet
        .push(
            &source,
            &doc,
            &body.targets,
            body.include_secrets,
            body.dry_run,
        )
        .await;
    let ok = results.iter().filter(|r| r["ok"] == json!(true)).count();
    Ok(Json(json!({
        "state": if body.dry_run { "planned" } else { "pushed" },
        "self_id": source,
        "actor": format!("fleet:{source}"),
        "include_secrets": body.include_secrets,
        "dry_run": body.dry_run,
        "targets": body.targets,
        "results": results,
        "ok": ok,
        "failed": body.targets.len().saturating_sub(ok),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::tool::mocks::MockInject;

    #[tokio::test]
    async fn apply_config_import_is_atomic_on_preflight_and_mid_write_failures() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let dir = std::env::temp_dir().join(format!(
            "hm-import-atomic-{}",
            crate::config_delivery::uuid().unwrap()
        ));
        let state = AppState::new(store.clone(), dir.clone(), MockInject::default(), None);

        let before_mode = state.voice.settings_snapshot();
        let before_devices = state.config.device_meta().unwrap();
        let before_services = state.config.load().unwrap();
        let before_history = state.config.history(50).unwrap();

        // 1. Pre-flight failure: valid early blocks (mode, devices, services) +
        //    invalid late block (audio_eq with an unknown curve).
        let bad_late: ConfigImportBody = serde_json::from_value(json!({
            "format": "home-mate-config",
            "version": 1,
            "mode": {
                "assistant_name": "已改名",
                "persona_prompt": "不应该被写入"
            },
            "devices": {
                "dot2-a": { "name": "客厅音箱", "room": "客厅" }
            },
            "services": {
                "llm": {
                    "provider": "deepseek",
                    "base_url": "https://api.deepseek.com",
                    "model": "mutated-model"
                }
            },
            "audio_eq": {
                "curve": "nonexistent_curve",
                "bands": []
            }
        }))
        .unwrap();
        let err = apply_config_import(&state, bad_late, "import")
            .await
            .err()
            .unwrap();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1["error"].as_str().unwrap().contains("audio_eq"));
        assert_eq!(
            state.voice.settings_snapshot().assistant_name,
            before_mode.assistant_name
        );
        assert_eq!(state.config.device_meta().unwrap(), before_devices);
        assert_eq!(
            state.config.load().unwrap().llm.model,
            before_services.llm.model
        );
        assert_eq!(
            state.config.history(50).unwrap().len(),
            before_history.len()
        );

        // 2. Mid-write failure: valid pre-flight across mode, devices, services, and
        //    audio_eq, but an injected SQLite trigger aborts when writing audio_eq.
        //    The checkpoint must roll back mode, devices, services, and history rows.
        store
            .with(|c| {
                c.execute_batch(
                    "CREATE TRIGGER fail_audio_eq BEFORE INSERT ON config_kv WHEN NEW.key='audio_eq' BEGIN SELECT RAISE(ABORT,'synthetic mid-write failure'); END;",
                )?;
                Ok(())
            })
            .unwrap();
        let mid_fail: ConfigImportBody = serde_json::from_value(json!({
            "format": "home-mate-config",
            "version": 1,
            "mode": {
                "assistant_name": "中途失败名",
                "persona_prompt": "中途失败应回滚"
            },
            "devices": {
                "dot2-a": { "name": "卧室音箱", "room": "卧室" }
            },
            "services": {
                "llm": {
                    "provider": "deepseek",
                    "base_url": "https://api.deepseek.com",
                    "model": "mid-fail-model"
                }
            },
            "audio_eq": {
                "curve": "flat",
                "bands": []
            }
        }))
        .unwrap();
        let err = apply_config_import(&state, mid_fail, "import")
            .await
            .err()
            .unwrap();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            state.voice.settings_snapshot().assistant_name,
            before_mode.assistant_name,
            "in-memory voice settings must roll back on mid-write failure"
        );
        assert_eq!(
            state.config.device_meta().unwrap(),
            before_devices,
            "devices must roll back on mid-write failure"
        );
        assert_eq!(
            state.config.load().unwrap().llm.model,
            before_services.llm.model,
            "services must roll back on mid-write failure"
        );
        assert_eq!(
            state.config.history(50).unwrap().len(),
            before_history.len(),
            "config_history must roll back on mid-write failure"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
