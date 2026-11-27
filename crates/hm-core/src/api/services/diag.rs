use crate::api::*;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

// --- diagnostics (M3): passive observation / active test / repair kept separate --

/// PASSIVE snapshot only — everything here is already observed state; the
/// endpoint itself triggers no external call and no device action. Active
/// probes live under /diagnostics/tests/*, repairs under /diagnostics/repair/*.
pub(in crate::api) async fn diagnostics_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    let services = state.config.statuses().unwrap_or(json!({}));
    // Voice-log derived counters (last 100 events, bounded feed).
    let voice_events = state.voice.recent(100);
    let mut voice_counts: std::collections::BTreeMap<String, u64> = Default::default();
    for e in &voice_events {
        if let Some(k) = e["kind"].as_str() {
            *voice_counts.entry(k.to_string()).or_default() += 1;
        }
    }
    let error_kinds = [
        "turn_error",
        "tts_failed",
        "play_submit_failed",
        "tts_unconfigured",
    ];
    let recent_errors: Vec<Value> = voice_events
        .iter()
        .filter(|e| {
            e["kind"]
                .as_str()
                .map(|k| error_kinds.contains(&k))
                .unwrap_or(false)
        })
        .take(10)
        .cloned()
        .collect();
    // Gateway-side anomalies from the hub ring (stale events, teardowns).
    let ring = state.hub.recent(100);
    let mut gateway_counts: std::collections::BTreeMap<String, u64> = Default::default();
    for e in &ring {
        if let Some(k) = e["kind"].as_str() {
            *gateway_counts.entry(k.to_string()).or_default() += 1;
        }
    }
    // Last failed traces (why did recent requests fail).
    let failed_traces: Vec<Value> = state
        .traces
        .list(30)
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t["status"] == json!("failed"))
        .take(3)
        .collect();

    let mode = state.voice.mode_snapshot();
    let mut gateway = state.hub.snapshot();
    let voice_settings = state.voice.settings_snapshot();
    super::enrich_gateway_tail_mismatch(&mut gateway, &voice_settings);
    Json(json!({
        "services": services,
        "gateway": gateway,
        "devices": mode["devices"],
        "voice_counts": voice_counts,
        "gateway_counts": gateway_counts,
        "recent_errors": recent_errors,
        "failed_traces": failed_traces,
        "note": "被动观测：不发起任何外部调用或设备动作；主动测试与修复在下方独立入口",
    }))
}

#[derive(Deserialize, Default)]
pub(in crate::api) struct SelfCheckBody {
    /// `true` = also run the EXISTING real probes (LLM ping, ASR sample, TTS
    /// synthesis, service probes). Still never an audible play.
    #[serde(default)]
    deep: bool,
    #[serde(default)]
    device: Option<String>,
}

/// R2-BM6: one-click self check. Passive by default (no outbound request, no
/// device action); the stage words are the same ones the service cards use.
pub(in crate::api) async fn diagnostics_self_check(
    State(state): State<Arc<AppState>>,
    body: Option<Json<SelfCheckBody>>,
) -> Json<Value> {
    let (deep, device) = body
        .map(|b| (b.0.deep, b.0.device))
        .unwrap_or((false, None));
    let input = crate::diagnostics::SelfCheckInput {
        config: &state.config,
        voice: &state.voice,
        hub: &state.hub,
        traces: &state.traces,
        music: &state.music,
        weather: &state.weather,
        home: &state.home,
        news: &state.news,
        openapi: &state.openapi,
        search: &state.search,
    };
    Json(crate::diagnostics::run_for_device(&input, deep, device.as_deref()).await)
}

#[derive(Deserialize, Default)]
pub(in crate::api) struct VoicePlayBody {
    #[serde(default)]
    device: Option<String>,
}

fn require_explicit_device_if_multi(
    state: &AppState,
    device: Option<&str>,
) -> Result<(), (StatusCode, Json<Value>)> {
    let explicit = device.map(str::trim).filter(|s| !s.is_empty());
    if explicit.is_none() && state.hub.online_device_count() > 1 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "code": "DEVICE_REQUIRED",
                "error": "DEVICE_REQUIRED: multiple devices are online; specify target device explicitly",
            })),
        ));
    }
    Ok(())
}

/// ACTIVE test: full voice-chain probe (audible on the device).
pub(in crate::api) async fn diagnostics_voice_play(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let device = if body.is_empty() {
        None
    } else {
        serde_json::from_slice::<VoicePlayBody>(&body)
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("body: {e}") })),
                )
            })?
            .device
    };
    require_explicit_device_if_multi(&state, device.as_deref())?;
    Ok(Json(
        state.voice.diagnostic_voice_play(device.as_deref()).await,
    ))
}

/// ACTIVE test: play a fixed audio asset from `<data_dir>/assets/`
/// (G6 ERLE stimulus) through the real Play path — audible, receipted.
#[derive(Deserialize)]
pub(in crate::api) struct PlayAssetBody {
    asset: String,
    device: Option<String>,
    channel: Option<String>,
    volume: Option<f64>,
}

pub(in crate::api) async fn diagnostics_play_asset(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PlayAssetBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_explicit_device_if_multi(&state, body.device.as_deref())?;
    let channel = match body.channel.as_deref() {
        Some("media") => "media",
        _ => "voice",
    };
    Ok(Json(
        state
            .voice
            .diagnostic_play_asset(
                &state.data_dir.join("assets"),
                &body.asset,
                body.device.as_deref(),
                channel,
                body.volume,
            )
            .await,
    ))
}

/// Task B: ch8 playback-loopback self-check (DAC-side track vs ch8).
#[derive(Deserialize, Default)]
pub(in crate::api) struct LoopbackBody {
    device: Option<String>,
    /// Any broadband asset works; default = the dedicated chirp probe.
    asset: Option<String>,
}

pub(in crate::api) async fn diagnostics_playback_loopback(
    State(state): State<Arc<AppState>>,
    body: Option<Json<LoopbackBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (device, asset) = body.map(|b| (b.0.device, b.0.asset)).unwrap_or_default();
    require_explicit_device_if_multi(&state, device.as_deref())?;
    Ok(Json(
        state
            .voice
            .diagnostic_playback_loopback(
                &state.data_dir.join("assets"),
                device.as_deref(),
                asset.as_deref(),
            )
            .await,
    ))
}

/// Task C: 7-mic array measurement (per-mic level/SNR + arrival lags).
pub(in crate::api) async fn diagnostics_mic_array(
    State(state): State<Arc<AppState>>,
    body: Option<Json<LoopbackBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (device, asset) = body.map(|b| (b.0.device, b.0.asset)).unwrap_or_default();
    require_explicit_device_if_multi(&state, device.as_deref())?;
    Ok(Json(
        state
            .voice
            .diagnostic_mic_array(
                &state.data_dir.join("assets"),
                device.as_deref(),
                asset.as_deref(),
            )
            .await,
    ))
}

/// REPAIR: re-send the persisted duplex request to one device.
#[derive(Deserialize)]
pub(in crate::api) struct ReapplyModeBody {
    device: String,
}

pub(in crate::api) async fn diagnostics_repair_mode(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ReapplyModeBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.hub.device_exists(&body.device) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown device '{}'", body.device) })),
        ));
    }
    match state.voice.repair_reapply_mode(&body.device) {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err((StatusCode::CONFLICT, Json(json!({ "error": e })))),
    }
}

// --- firmware / core A/B updates (§12, stage C1.5) ----------------------------------

fn release_fail(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

pub(in crate::api) async fn release_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    let data = state.data_dir.clone();
    let tool = crate::release::tool_path(&data);
    let schema = state.store.schema_version().ok();
    Json(json!({
        "installer": {
            "path": tool.display().to_string(),
            "present": tool.is_file(),
        },
        "releases_dir": crate::release::releases_dir(&data).display().to_string(),
        "state": crate::release::state_json(&data),
        "live": {
            "build": { "version": crate::CORE_VERSION, "id": crate::BUILD_ID },
            "schema": schema,
            "data_dir": data.display().to_string(),
        },
    }))
}

#[derive(Deserialize)]
pub(in crate::api) struct ReleaseInstall {
    url: String,
    #[serde(default)]
    activate: bool,
}

const RELEASE_LOCAL_PREFIX: &str = "/data/hm/releases/";
const MAX_RELEASE_REASON_LEN: usize = 256;

fn validate_release_package(raw: &str) -> Result<String, String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err("url is required".into());
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("package path/URL must not contain control or whitespace characters".into());
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        crate::tool::http::validate_outbound_url(
            url,
            crate::tool::http::OutboundPolicy::PublicOrLan {
                allow_loopback: false,
            },
        )
        .map_err(|e| format!("invalid package URL: {e}"))?;
        return Ok(url.to_string());
    }
    if url.starts_with('/') {
        if !url.starts_with(RELEASE_LOCAL_PREFIX) || url.len() <= RELEASE_LOCAL_PREFIX.len() {
            return Err("local package path must be under /data/hm/releases/".into());
        }
        let path = std::path::Path::new(url);
        if url.contains("..")
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("local package path must not contain '..'".into());
        }
        if let Ok(canon) = std::fs::canonicalize(path) {
            if !canon.starts_with(RELEASE_LOCAL_PREFIX) {
                return Err("canonical package path escapes /data/hm/releases/".into());
            }
        }
        return Ok(url.to_string());
    }
    Err("package must be an http(s):// URL or a path under /data/hm/releases/".into())
}

fn validate_release_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    if id.is_empty() || id.len() > 128 {
        return Err("release id must be 1..=128 characters".into());
    }
    if id.starts_with('-') || id.contains("..") {
        return Err("release id must not start with '-' or contain '..'".into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err("release id must match ^[A-Za-z0-9._-]+$".into());
    }
    Ok(id.to_string())
}

fn validate_release_reason(raw: Option<String>, default: &str) -> Result<String, String> {
    let reason = raw.unwrap_or_else(|| default.to_string());
    let reason = reason.trim();
    if reason.is_empty() {
        return Ok(default.to_string());
    }
    if reason.len() > MAX_RELEASE_REASON_LEN {
        return Err(format!(
            "reason must be at most {MAX_RELEASE_REASON_LEN} bytes"
        ));
    }
    if reason.starts_with('-') {
        return Err("reason must not start with '-'".into());
    }
    if reason.chars().any(|c| c.is_control()) {
        return Err("reason must not contain control characters".into());
    }
    Ok(reason.to_string())
}

pub(in crate::api) async fn release_install(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ReleaseInstall>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let url =
        validate_release_package(&req.url).map_err(|e| release_fail(StatusCode::BAD_REQUEST, e))?;
    if crate::release::busy(&state.data_dir) {
        return Err(release_fail(
            StatusCode::CONFLICT,
            "a release install/activation is already running",
        ));
    }
    let mut args = vec!["install".to_string(), "--package".to_string(), url];
    if req.activate {
        args.push("--activate".to_string());
    }
    let pid = crate::release::spawn_tool(&state.data_dir, &args)
        .map_err(|e| release_fail(StatusCode::CONFLICT, e))?;
    tracing::info!(
        "release install started (pid {pid}, activate={})",
        req.activate
    );
    Ok(Json(
        json!({ "state": "started", "action": "install", "pid": pid, "activate": req.activate }),
    ))
}

#[derive(Deserialize, Default)]
pub(in crate::api) struct ReleaseActivate {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

fn parse_optional_json<T: Default + serde::de::DeserializeOwned>(
    body: &[u8],
) -> Result<T, (StatusCode, Json<Value>)> {
    if body.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(body)
        .map_err(|e| release_fail(StatusCode::BAD_REQUEST, format!("bad JSON: {e}")))
}

pub(in crate::api) async fn release_activate(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if crate::release::busy(&state.data_dir) {
        return Err(release_fail(
            StatusCode::CONFLICT,
            "a release install/activation is already running",
        ));
    }
    let req: ReleaseActivate = parse_optional_json(&body)?;
    let mut args = vec!["activate".to_string()];
    if let Some(raw_id) = req.id.filter(|s| !s.trim().is_empty()) {
        let id =
            validate_release_id(&raw_id).map_err(|e| release_fail(StatusCode::BAD_REQUEST, e))?;
        args.push("--id".to_string());
        args.push(id);
    }
    let reason = validate_release_reason(req.reason, "console")
        .map_err(|e| release_fail(StatusCode::BAD_REQUEST, e))?;
    args.push("--reason".to_string());
    args.push(reason);
    let pid = crate::release::spawn_tool(&state.data_dir, &args)
        .map_err(|e| release_fail(StatusCode::CONFLICT, e))?;
    tracing::info!("release activation started (pid {pid})");
    Ok(Json(
        json!({ "state": "started", "action": "activate", "pid": pid }),
    ))
}

pub(in crate::api) async fn release_rollback(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if crate::release::busy(&state.data_dir) {
        return Err(release_fail(
            StatusCode::CONFLICT,
            "a release install/activation is already running",
        ));
    }
    let req: ReleaseActivate = parse_optional_json(&body)?;
    let reason = validate_release_reason(req.reason, "console rollback")
        .map_err(|e| release_fail(StatusCode::BAD_REQUEST, e))?;
    let mut args = vec!["rollback".to_string(), "--reason".to_string()];
    args.push(reason);
    let pid = crate::release::spawn_tool(&state.data_dir, &args)
        .map_err(|e| release_fail(StatusCode::CONFLICT, e))?;
    tracing::warn!("release rollback started (pid {pid})");
    Ok(Json(
        json!({ "state": "started", "action": "rollback", "pid": pid }),
    ))
}

#[derive(Deserialize)]
pub(in crate::api) struct ReleaseLogQuery {
    #[serde(default = "release_log_default")]
    lines: usize,
}

fn release_log_default() -> usize {
    200
}

pub(in crate::api) async fn release_log(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<ReleaseLogQuery>,
) -> Json<Value> {
    let lines = crate::release::log_tail(&state.data_dir, q.lines);
    Json(json!({
        "lines": lines,
        "path": crate::release::releases_dir(&state.data_dir).join("installer.log").display().to_string(),
    }))
}

#[derive(Deserialize)]
pub(in crate::api) struct ReleaseSnapshot {
    #[serde(default)]
    tag: String,
}

/// Pre-activation DB snapshot (called by `hm-release`, also usable by hand).
pub(in crate::api) async fn release_snapshot(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ReleaseSnapshot>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let tag: String = req
        .tag
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .take(64)
        .collect();
    let dir = state.data_dir.join("db-snapshots");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| release_fail(StatusCode::INTERNAL_SERVER_ERROR, format!("mkdir: {e}")))?;
    let ms = crate::id::now_ms();
    let name = if tag.is_empty() {
        format!("home-mate-snapshot-{ms}.db")
    } else {
        format!("home-mate-{tag}-{ms}.db")
    };
    let path = dir.join(name);
    let store = state.store.clone();
    let p = path.clone();
    let schema = tokio::task::spawn_blocking(move || store.snapshot_to(&p))
        .await
        .map_err(|e| {
            release_fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("snapshot task: {e}"),
            )
        })?
        .map_err(|e| release_fail(StatusCode::INTERNAL_SERVER_ERROR, format!("snapshot: {e}")))?;
    prune_snapshots(&dir, 2);
    tracing::info!(
        "release DB snapshot at {} (schema {schema})",
        path.display()
    );
    Ok(Json(json!({
        "path": path.display().to_string(),
        "schema": schema,
        "ms": ms,
    })))
}

fn prune_snapshots(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(u64, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("db") {
                return None;
            }
            let ms = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            Some((ms, p))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(p);
    }
}

// --- household memory & reminders ---------------------------------------------

pub(in crate::api) async fn reminders_get(
    State(state): State<Arc<AppState>>,
    lan_exempt: Option<axum::Extension<crate::api::LanExemptUnauthenticated>>,
) -> Json<Value> {
    let svc = crate::reminder::ReminderService::new(state.store.clone());
    match svc.recent(50) {
        Ok(mut items) => {
            if lan_exempt.is_some() {
                let protected = state.hub.loopback_protected_devices();
                if !protected.is_empty() {
                    items.retain(|r| !protected.iter().any(|p| p == &r.device));
                }
            }
            Json(json!({
                "now_ms": crate::id::now_ms(),
                "reminders": items,
            }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

#[derive(Deserialize)]
pub(in crate::api) struct ReminderAdd {
    note: String,
    device: Option<String>,
    delay_minutes: Option<u64>,
    hour: Option<u64>,
    minute: Option<u64>,
    day_offset: Option<u64>,
}

/// Create from the console: the same resolution rules as the voice tool, so
/// a reminder set either way behaves identically.
pub(in crate::api) async fn reminders_add(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ReminderAdd>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let svc = crate::reminder::ReminderService::new(state.store.clone());
    let when = match (body.delay_minutes, body.hour, body.minute) {
        (Some(m), _, _) => crate::reminder::When::DelayMinutes(m),
        (None, Some(h), mi) => crate::reminder::When::At {
            day_offset: body.day_offset,
            hour: h as u32,
            minute: mi.unwrap_or(0) as u32,
        },
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    json!({ "error": "REMINDER_BAD_ARGS: 要给 delay_minutes，或 hour（可选 minute）" }),
                ),
            ))
        }
    };
    let now_secs = crate::tool::time::now_secs();
    let now = crate::tool::time::local_now().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "REMINDER_UNAVAILABLE: 本机时钟读不出来" })),
    ))?;
    let due_ms = crate::reminder::resolve_due(&when, now_secs, &now).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    require_explicit_device_if_multi(&state, body.device.as_deref())?;
    let device = body
        .device
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            state
                .hub
                .primary_audio_out()
                .map(|d| d.stable_id)
                .or_else(|| {
                    state.hub.snapshot()["devices"]
                        .as_array()
                        .and_then(|a| {
                            a.iter()
                                .find(|d| d["online"].as_bool().unwrap_or(false))
                                .and_then(|d| d["stable_id"].as_str())
                        })
                        .map(str::to_string)
                })
                .or_else(|| {
                    std::env::var("HM_NODE_ID")
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or_else(|| state.auth.core_id().to_string())
        });
    match svc.create(&device, due_ms, &body.note, crate::id::now_ms()) {
        Ok(r) => Ok(Json(serde_json::to_value(r).unwrap_or_else(|_| json!({})))),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )),
    }
}

pub(in crate::api) async fn reminder_delete(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let svc = crate::reminder::ReminderService::new(state.store.clone());
    match svc.cancel(id, crate::id::now_ms()) {
        Ok(r) => Ok(Json(serde_json::to_value(r).unwrap_or_else(|_| json!({})))),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )),
    }
}

pub(in crate::api) async fn memories_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    match state.memory.list() {
        Ok(items) => Json(json!({
            "items": items,
            "count": items.len(),
            "max": crate::memory::MAX_ITEMS,
            "max_text": crate::memory::MAX_TEXT,
            "kinds": crate::memory::KINDS,
        })),
        Err(e) => Json(json!({ "error": e.to_string(), "items": [] })),
    }
}

#[derive(Deserialize)]
pub(in crate::api) struct MemoryBody {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    pinned: Option<bool>,
}

pub(in crate::api) async fn memories_add(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MemoryBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let text = body.text.unwrap_or_default();
    let kind = body.kind.unwrap_or_else(|| "fact".into());
    match state
        .memory
        .add(&text, &kind, "console", body.pinned.unwrap_or(false))
    {
        Ok(item) => Ok(Json(json!({ "state": "applied", "item": item }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

pub(in crate::api) async fn memories_update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<MemoryBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.memory.update(&id, body.text.as_deref(), body.pinned) {
        Ok(Some(item)) => Ok(Json(json!({ "state": "applied", "item": item }))),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such memory" })),
        )),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

pub(in crate::api) async fn memories_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    match state.memory.delete(&id) {
        Ok(true) => Json(json!({ "state": "deleted", "id": id })),
        Ok(false) => Json(json!({ "state": "missing", "id": id })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

pub(in crate::api) async fn memories_clear(State(state): State<Arc<AppState>>) -> Json<Value> {
    match state.memory.clear() {
        Ok(n) => Json(json!({ "state": "cleared", "deleted": n })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

#[derive(Deserialize)]
pub(in crate::api) struct CandidateQuery {
    #[serde(default)]
    status: Option<String>,
}

pub(in crate::api) async fn memory_candidates_get(
    State(state): State<Arc<AppState>>,
    Query(q): Query<CandidateQuery>,
) -> Json<Value> {
    let status = q.status.as_deref().filter(|s| !s.is_empty());
    match state.memory.candidates(status) {
        Ok(items) => Json(json!({
            "items": items,
            "count": items.len(),
            "pending_max": crate::memory::MAX_PENDING,
            "per_session_max": crate::memory::MAX_PER_SESSION,
            "ttl_ms": crate::memory::CANDIDATE_TTL_MS,
        })),
        Err(e) => Json(json!({ "error": e, "items": [] })),
    }
}

pub(in crate::api) async fn memory_candidate_confirm(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.memory.confirm_candidate(&id) {
        Ok(item) => Ok(Json(json!({ "state": "applied", "item": item }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

pub(in crate::api) async fn memory_candidate_reject(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.memory.reject_candidate(&id, "console") {
        Ok(()) => Ok(Json(json!({ "state": "rejected", "id": id }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

pub(in crate::api) async fn memory_candidate_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    match state.memory.delete_candidate(&id) {
        Ok(true) => Json(json!({ "state": "deleted", "id": id })),
        Ok(false) => Json(json!({ "state": "missing", "id": id })),
        Err(e) => Json(json!({ "error": e })),
    }
}

// --- long-term working memory (#23) -------------------------------------------

#[derive(Deserialize)]
pub(in crate::api) struct WorkingMemoryQuery {
    #[serde(default)]
    scope_person: Option<String>,
    #[serde(default)]
    scope_device: Option<String>,
    #[serde(default)]
    key_or_topic: Option<String>,
}

#[derive(Deserialize)]
pub(in crate::api) struct WorkingMemoryPutBody {
    #[serde(default)]
    scope_person: String,
    #[serde(default)]
    scope_device: String,
    key_or_topic: String,
    content: String,
    #[serde(default)]
    pinned: Option<bool>,
}

#[derive(Deserialize)]
pub(in crate::api) struct WorkingMemoryPinBody {
    pinned: bool,
}

pub(in crate::api) async fn working_memory_get(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WorkingMemoryQuery>,
) -> Json<Value> {
    match state
        .working_memory
        .list(q.scope_person.as_deref(), q.scope_device.as_deref())
    {
        Ok(items) => {
            let pinned_count = state.working_memory.pinned_count().unwrap_or(0);
            let evicted_total = state.working_memory.evicted_total();
            Json(json!({
                "items": items,
                "count": items.len(),
                "pinned_count": pinned_count,
                "evicted_total": evicted_total,
                "max": crate::working_memory::MAX_ITEMS,
                "max_key_len": crate::working_memory::MAX_KEY_LEN,
                "max_content_len": crate::working_memory::MAX_CONTENT_LEN,
            }))
        }
        Err(e) => Json(json!({ "error": e, "items": [] })),
    }
}

pub(in crate::api) async fn working_memory_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<WorkingMemoryPutBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.working_memory.set_with_provenance(
        &body.scope_person,
        &body.scope_device,
        &body.key_or_topic,
        &body.content,
        body.pinned,
        Some("console"),
        None,
    ) {
        Ok(item) => Ok(Json(json!({ "state": "applied", "item": item }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

pub(in crate::api) async fn working_memory_pin(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<WorkingMemoryPinBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.working_memory.set_pinned(&id, body.pinned) {
        Ok(Some(item)) => Ok(Json(json!({ "state": "applied", "item": item }))),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "working memory item not found" })),
        )),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

pub(in crate::api) async fn working_memory_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    match state.working_memory.delete_by_id(&id) {
        Ok(true) => Json(json!({ "state": "deleted", "id": id })),
        Ok(false) => Json(json!({ "state": "missing", "id": id })),
        Err(e) => Json(json!({ "error": e })),
    }
}

pub(in crate::api) async fn working_memory_delete_by_key(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WorkingMemoryQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let key = q.key_or_topic.as_deref().unwrap_or("");
    if key.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "key_or_topic is required" })),
        ));
    }
    match state.working_memory.delete(
        q.scope_person.as_deref().unwrap_or(""),
        q.scope_device.as_deref().unwrap_or(""),
        key,
    ) {
        Ok(true) => Ok(Json(json!({ "state": "deleted", "key_or_topic": key }))),
        Ok(false) => Ok(Json(json!({ "state": "missing", "key_or_topic": key }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

pub(in crate::api) async fn working_memory_clear(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WorkingMemoryQuery>,
) -> Json<Value> {
    match state
        .working_memory
        .clear_scoped(q.scope_person.as_deref(), q.scope_device.as_deref())
    {
        Ok(n) => Json(json!({ "state": "cleared", "deleted": n })),
        Err(e) => Json(json!({ "error": e })),
    }
}

pub(in crate::api) async fn shared_memory_status(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    Json(serde_json::to_value(state.shared_memory.status()).unwrap_or_else(|_| json!({})))
}

#[derive(serde::Deserialize)]
pub(in crate::api) struct SharedMemoryConfigPut {
    #[serde(flatten)]
    config: crate::config::SharedMemoryConfig,
    #[serde(default)]
    token: Option<String>,
}

pub(in crate::api) async fn shared_memory_config_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SharedMemoryConfigPut>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state
        .shared_memory
        .save_config_with_token(body.config, body.token.as_deref(), "console")
    {
        Ok(cfg) => {
            if cfg.enabled && !cfg.nas_base_url.is_empty() {
                let svc = state.shared_memory.clone();
                tokio::spawn(async move {
                    let _ = svc.sync_once().await;
                });
            }
            Ok(Json(json!({
                "state": "applied",
                "config": cfg,
                "status": state.shared_memory.status(),
            })))
        }
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

pub(in crate::api) async fn shared_memory_clear_outbox(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    match state.shared_memory.clear_outbox() {
        Ok(n) => Json(json!({ "state": "cleared", "deleted": n })),
        Err(e) => Json(json!({ "error": e })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_package_validation_accepts_lan_url_and_releases_path() {
        assert!(validate_release_package("http://192.168.1.27:18981/hm-0.1.0.tar").is_ok());
        assert!(validate_release_package("https://releases.example.com/hm-0.1.0.tar").is_ok());
        assert!(validate_release_package("/data/hm/releases/hm-0.1.0.tar").is_ok());
    }

    #[test]
    fn release_package_validation_rejects_metadata_and_traversal() {
        assert!(validate_release_package("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(validate_release_package("http://metadata.google.internal/pkg.tar").is_err());
        assert!(validate_release_package("http://127.0.0.1:8080/pkg.tar").is_err());
        assert!(validate_release_package("/data/hm/releases/../etc/passwd").is_err());
        assert!(validate_release_package("/tmp/evil.tar").is_err());
        assert!(validate_release_package("/data/hm/releases/").is_err());
        assert!(validate_release_package("http://192.168.1.27:18981/a\nb.tar").is_err());
        assert!(validate_release_package("file:///data/hm/releases/a.tar").is_err());
    }

    #[test]
    fn release_id_and_reason_validation() {
        assert!(validate_release_id("20261119-0.1.0_rc1").is_ok());
        assert!(validate_release_id("r100").is_ok());
        assert!(validate_release_id("--flag").is_err());
        assert!(validate_release_id("-f").is_err());
        assert!(validate_release_id("../escape").is_err());
        assert!(validate_release_id("a/b").is_err());
        assert!(validate_release_id("has space").is_err());
        assert!(validate_release_id("bad\nid").is_err());

        assert_eq!(validate_release_reason(None, "console").unwrap(), "console");
        assert!(validate_release_reason(Some("routine upgrade".into()), "console").is_ok());
        assert!(validate_release_reason(Some("bad\x00reason".into()), "console").is_err());
        assert!(validate_release_reason(Some("a".repeat(257)), "console").is_err());
    }
}
