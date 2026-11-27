use super::*;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) async fn devices(
    State(state): State<Arc<AppState>>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
) -> Json<Value> {
    let mut voice_log = state.voice.recent(30);
    if lan_exempt.is_some() {
        let protected = state.hub.loopback_protected_devices();
        if !protected.is_empty() {
            voice_log.retain(|e| {
                e.get("device")
                    .and_then(|v| v.as_str())
                    .is_none_or(|d| !protected.iter().any(|p| p == d))
            });
        }
    }
    Json(json!({
        "gateway": state.hub.snapshot(),
        "recent": state.hub.recent(30),
        "voice_log": voice_log,
    }))
}

#[derive(Default, Deserialize)]
pub(super) struct DeviceConversationQuery {
    context_session_id: Option<String>,
    archive_before: Option<u64>,
    message_before: Option<i64>,
}
pub(super) async fn device_conversation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
    Query(query): Query<DeviceConversationQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    super::reject_lan_exempt_loopback_device(&state, lan_exempt.as_ref(), &id)?;
    if !state.hub.device_exists(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error":"unknown device"})),
        ));
    }
    let error = |e: String| (StatusCode::CONFLICT, Json(json!({"error":e})));
    let current_context = state.sessions.current_device_context(&id).map_err(error)?;
    let current = match &current_context {
        Some(c) => Some(c.session_id.clone()),
        None => state
            .sessions
            .current_device_session_id(&id)
            .map_err(error)?,
    };
    let selected_context = match query.context_session_id {
        Some(requested) => Some(
            state
                .sessions
                .device_context(&id, &requested)
                .map_err(error)?
                .ok_or_else(|| error("context unavailable in this device scope".into()))?,
        ),
        None => current_context.clone(),
    };
    let selected = selected_context
        .as_ref()
        .map(|c| c.session_id.clone())
        .or(current.clone());
    let mut archives = state
        .sessions
        .device_contexts(&id, query.archive_before, 21)
        .map_err(error)?;
    let more = archives.len() > 20;
    archives.truncate(20);
    let next_archive_before = if more {
        archives.last().map(|c| c.generation)
    } else {
        None
    };
    let (messages, next_message_before) = match &selected {
        Some(session) => state
            .sessions
            .context_messages_page(session, query.message_before)
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error":e.to_string()})),
                )
            })?,
        None => (Vec::new(), None),
    };
    Ok(Json(
        json!({"session_id":selected,"messages":messages,"current_context":current_context,"selected_context":selected_context,"archived_contexts":archives,"next_archive_before":next_archive_before,"next_message_before":next_message_before,"view":if selected==current {"current"} else {"archive"}}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClearDeviceConversation {
    expected_session_id: String,
    #[serde(default)]
    include_archived: bool,
}
pub(super) async fn device_conversation_clear(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
    Json(body): Json<ClearDeviceConversation>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    super::reject_lan_exempt_loopback_device(&state, lan_exempt.as_ref(), &id)?;
    if !state.hub.device_exists(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error":"unknown device"})),
        ));
    }
    let cleared = state
        .sessions
        .clear_device_conversation_with_archives(
            &id,
            &body.expected_session_id,
            body.include_archived,
        )
        .map_err(|e| {
            (
                StatusCode::CONFLICT,
                Json(
                    json!({"error":if e.contains("SESSION_CHANGED") {"SESSION_CHANGED"} else {&e}}),
                ),
            )
        })?;
    Ok(Json(
        json!({"session_id":body.expected_session_id,"cleared":cleared,"include_archived":body.include_archived}),
    ))
}

pub(super) async fn device_stop(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    super::reject_lan_exempt_loopback_device(&state, lan_exempt.as_ref(), &id)?;
    if !state.hub.device_exists(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown device '{id}'") })),
        ));
    }
    let mut result = state.voice.stop_device(&id);
    // 停 means discard (R1e): a console stop must not leave a resume point.
    let discarded = state.voice.discard_program(&id);
    if let Some(map) = result.as_object_mut() {
        map.insert("program_discarded".into(), json!(discarded));
    }
    Ok(Json(result))
}

/// R1e: pause / resume the spoken program from the management side. This is
/// the entry that does not depend on the mic at all — the voice command has to
/// cross the barge-in guard, a button or this API does not.
#[derive(Deserialize)]
pub(super) struct MicCaptureBody {
    #[serde(default)]
    seconds: Option<u64>,
}

pub(super) async fn device_mic_capture_start(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<MicCaptureBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.hub.device_exists(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown device '{id}'") })),
        ));
    }
    let seconds = body.and_then(|b| b.0.seconds).unwrap_or(5);
    state
        .voice
        .start_mic_capture(&id, seconds)
        .map(Json)
        .map_err(|e| (StatusCode::CONFLICT, Json(json!({ "error": e }))))
}

pub(super) async fn device_mic_capture_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    Json(state.voice.mic_capture_status(&id))
}

pub(super) async fn device_mic_capture_wav(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, (StatusCode, Json<Value>)> {
    match state.voice.mic_capture_wav(&id) {
        Some(bytes) => {
            use axum::response::IntoResponse;
            Ok(([(axum::http::header::CONTENT_TYPE, "audio/wav")], bytes).into_response())
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no finished capture for this device" })),
        )),
    }
}

pub(super) async fn device_program_pause(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    super::reject_lan_exempt_loopback_device(&state, lan_exempt.as_ref(), &id)?;
    if !state.hub.device_exists(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown device '{id}'") })),
        ));
    }
    if state.voice.pause_program(&id, "console") {
        return Ok(Json(json!({
            "paused": true,
            "target": "program",
            "bookmark": state.voice.paused_bookmark(&id),
        })));
    }
    if state.music.media_present(&id) {
        if let Ok(outcome) = state.music.control(&id, "pause", Some("media")).await {
            return Ok(Json(json!({
                "paused": outcome.confirmed && !outcome.no_op,
                "target": outcome.target,
                "title": outcome.subject,
                "detail": outcome.detail,
                "confirmed": outcome.confirmed,
                "no_op": outcome.no_op,
                "bookmark": state.voice.paused_bookmark(&id),
            })));
        }
    }
    Ok(Json(
        json!({ "paused": false, "bookmark": state.voice.paused_bookmark(&id) }),
    ))
}

pub(super) async fn device_program_resume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    super::reject_lan_exempt_loopback_device(&state, lan_exempt.as_ref(), &id)?;
    if !state.hub.device_exists(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown device '{id}'") })),
        ));
    }
    if state.voice.resume_program(&id) {
        return Ok(Json(json!({
            "resumed": true,
            "target": "program",
            "bookmark": state.voice.paused_bookmark(&id),
        })));
    }
    if state.music.media_present(&id) {
        if let Ok(outcome) = state.music.control(&id, "resume", Some("media")).await {
            return Ok(Json(json!({
                "resumed": outcome.confirmed && !outcome.no_op,
                "target": outcome.target,
                "title": outcome.subject,
                "detail": outcome.detail,
                "confirmed": outcome.confirmed,
                "no_op": outcome.no_op,
                "bookmark": state.voice.paused_bookmark(&id),
            })));
        }
    }
    Ok(Json(
        json!({ "resumed": false, "bookmark": state.voice.paused_bookmark(&id) }),
    ))
}

#[derive(Deserialize)]
pub(super) struct MediaControlBody {
    action: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    mode: Option<String>,
}

/// Direct playback control (`pause` | `resume` | `next` | `previous` | `stop` | `repeat`)
/// for a specific device without invoking an LLM turn.
pub(super) async fn device_media_control(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
    Json(body): Json<MediaControlBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    super::reject_lan_exempt_loopback_device(&state, lan_exempt.as_ref(), &id)?;
    if !state.hub.device_exists(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown device '{id}'") })),
        ));
    }
    let action = body.action.trim().to_ascii_lowercase();
    if !matches!(
        action.as_str(),
        "pause" | "resume" | "next" | "previous" | "stop" | "repeat"
    ) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("unsupported media action '{}'", body.action) })),
        ));
    }
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    match state
        .music
        .control_with_mode(
            &id,
            &action,
            body.target.as_deref(),
            body.mode.as_deref(),
            cancel,
            None,
        )
        .await
    {
        Ok(outcome) => Ok(Json(json!({
            "ok": outcome.confirmed || outcome.no_op,
            "action": outcome.action,
            "target": outcome.target,
            "title": outcome.subject,
            "detail": outcome.detail,
            "confirmed": outcome.confirmed,
            "no_op": outcome.no_op,
        }))),
        Err(e) => {
            let status = match e {
                crate::tool::music::MusicError::InvalidRepeatMode => StatusCode::BAD_REQUEST,
                _ => StatusCode::CONFLICT,
            };
            Err((status, Json(json!({ "ok": false, "error": e.to_string() }))))
        }
    }
}

#[derive(Deserialize)]
pub(super) struct VolumeBody {
    value: f64,
}

pub(super) async fn device_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
    Json(body): Json<VolumeBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    super::reject_lan_exempt_loopback_device(&state, lan_exempt.as_ref(), &id)?;
    if !state.hub.device_exists(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown device '{id}'") })),
        ));
    }
    match state.voice.set_volume(&id, body.value) {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

#[derive(Deserialize)]
pub(super) struct DeviceWifiBody {
    ssid: String,
    psk: String,
}

/// v0.2.3 online WiFi change (wizard step 1 / replaced-capability #1-2).
/// Waits for the device's join-watchdog outcome; an old firmware (no
/// "wifi" capability / pre-0.0.2 marker) is refused honestly.
pub(super) async fn device_wifi(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<DeviceWifiBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Device facts come from the hub snapshot (JSON; the API layer never
    // holds live device state).
    let snap = state.hub.snapshot();
    let Some(dev) = snap["devices"]
        .as_array()
        .and_then(|a| a.iter().find(|d| d["stable_id"] == json!(id)))
    else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown device '{id}'") })),
        ));
    };
    let has_wifi = dev["capabilities"]
        .as_array()
        .map(|c| c.iter().any(|x| x == "wifi"))
        .unwrap_or(false);
    if !has_wifi {
        return Err((
            StatusCode::CONFLICT,
            Json(
                json!({ "error": format!("device firmware {} does not support online wifi change (needs hm-firmware 0.0.2+)", dev["firmware"].as_str().unwrap_or("?")) }),
            ),
        ));
    }
    if body.ssid.is_empty()
        || body.ssid.contains('"')
        || body.psk.len() < 8
        || body.psk.contains('"')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "error": "ssid must be non-empty and psk at least 8 characters; quote characters are not allowed" }),
            ),
        ));
    }
    let mut rx = state
        .hub
        .wifi_config(&id, body.ssid.clone(), body.psk)
        .map_err(|e| (StatusCode::CONFLICT, Json(json!({ "error": e }))))?;
    match tokio::time::timeout(std::time::Duration::from_secs(70), &mut rx).await {
        Ok(Ok(crate::gateway::DeviceEvent::WifiState {
            ssid,
            state,
            detail,
            ..
        })) => Ok(Json(
            json!({ "state": state, "ssid": ssid, "detail": detail }),
        )),
        _ => Ok(Json(
            json!({ "state": "pending", "ssid": body.ssid, "detail": "watchdog reply not received - check the device link and voice log" }),
        )),
    }
}

#[derive(Deserialize)]
pub struct DeviceMuteBody {
    muted: bool,
}

/// v1 mechanism (#8/#13, 2026-11-16): set the device's mute latch remotely —
/// the ONE management surface allowed to write the latch the physical key
/// owns. The response carries the DEVICE's `MuteState` fact (correlated by
/// task id), never the request: a send is not an application. An unknown
/// after the window is honest — check the device link; the latch itself is
/// unaffected. A firmware without `remote_mute` (or an offline device) is
/// refused up front and keeps every other function.
pub(super) async fn device_mute(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
    Json(body): Json<DeviceMuteBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    super::reject_lan_exempt_loopback_device(&state, lan_exempt.as_ref(), &id)?;
    if !state.hub.device_exists(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown device '{id}'") })),
        ));
    }
    let (task_id, mut rx) = state.hub.mute_set(&id, body.muted).map_err(|e| {
        if e.contains("remote_mute") || e.contains("offline") {
            (StatusCode::CONFLICT, Json(json!({ "error": e })))
        } else {
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": e })))
        }
    })?;
    match tokio::time::timeout(std::time::Duration::from_secs(5), &mut rx).await {
        Ok(Ok(crate::gateway::GatewayFrame::MuteState {
            device: frame_device,
            muted,
            source,
            ..
        })) => {
            if frame_device != id || muted != body.muted {
                return Ok(Json(json!({
                    "applied": false,
                    "stage": "mismatch",
                    "task_id": task_id,
                    "device": id,
                    "reported_device": frame_device,
                    "requested_muted": body.muted,
                    "muted": muted,
                    "source": source,
                    "detail": format!(
                        "device MuteState mismatch: expected device={id} muted={}, got device={frame_device} muted={muted}",
                        body.muted
                    ),
                })));
            }
            Ok(Json(json!({
                "applied": true,
                "stage": "device_confirmed",
                "task_id": task_id,
                "device": id,
                "muted": muted,
                "source": source,
            })))
        }
        // The waiter also resolves with the frame itself on older paths; any
        // other outcome (timeout, dropped channel, alien frame) is NOT a
        // confirmation — the latch state must come from the device.
        _ => Ok(Json(json!({
            "applied": false,
            "stage": "unknown",
            "task_id": task_id,
            "device": id,
            "detail": "no correlated MuteState within 5s - check the device link and voice log",
        }))),
    }
}

fn firmware_layout_json(layout: &crate::fwupdate::SlotLayout) -> Value {
    let slots: Vec<Value> = layout
        .slots
        .iter()
        .filter_map(|s| layout.slot_info(s))
        .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
        .collect();
    json!({
        "available": layout.available(),
        "dir": layout.dir.display().to_string(),
        "active_slot": layout.active_slot(),
        "slots": slots,
    })
}

pub(super) async fn firmware_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "audio": firmware_layout_json(&state.fw_audio),
        "core": firmware_layout_json(&state.fw_core),
    }))
}

pub(super) const FIRMWARE_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Stream the (single) `file` field from a multipart body directly into a
/// same-filesystem staging file, avoiding full-payload RAM buffering on 512 MB
/// devices. On any error or early drop, `StagedSlotFile` removes the staging file.
async fn multipart_file_to_staging(
    mut mp: axum::extract::Multipart,
    layout: &crate::fwupdate::SlotLayout,
    max_bytes: u64,
) -> Result<(crate::fwupdate::StagedSlotFile, u64), (StatusCode, Json<Value>)> {
    use std::io::Write;

    while let Some(mut field) = mp.next_field().await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("multipart: {e}") })),
        )
    })? {
        if field.name() == Some("file") {
            let (staged, mut file) =
                crate::fwupdate::StagedSlotFile::prepare(layout).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("stage create: {e}") })),
                    )
                })?;
            let mut written: u64 = 0;
            while let Some(chunk) = field.chunk().await.map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("read file: {e}") })),
                )
            })? {
                written = written.saturating_add(chunk.len() as u64);
                if written > max_bytes {
                    return Err((
                        StatusCode::PAYLOAD_TOO_LARGE,
                        Json(json!({ "error": format!("payload exceeds {max_bytes} byte limit") })),
                    ));
                }
                file.write_all(&chunk).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("stage write: {e}") })),
                    )
                })?;
            }
            file.sync_all().map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("stage sync: {e}") })),
                )
            })?;
            return Ok((staged, written));
        }
    }
    Err((
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "missing multipart field 'file'" })),
    ))
}

#[derive(serde::Deserialize)]
pub(super) struct FirmwareAudioQuery {
    #[serde(default = "yes_by_default")]
    restart: bool,
}
fn yes_by_default() -> bool {
    true
}

fn unsigned_fwupdate_allowed() -> bool {
    std::env::var("HM_ALLOW_UNSIGNED_FWUPDATE")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

pub(super) async fn firmware_audio_update(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<FirmwareAudioQuery>,
    mp: axum::extract::Multipart,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !unsigned_fwupdate_allowed() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(
                json!({ "error": "unsigned firmware upload disabled; use signed hm-release or set HM_ALLOW_UNSIGNED_FWUPDATE=1" }),
            ),
        ));
    }
    if !state.fw_audio.available() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "audio firmware slot directory not present on this host" })),
        ));
    }
    let (mut staged, size) =
        multipart_file_to_staging(mp, &state.fw_audio, FIRMWARE_MAX_BYTES as u64).await?;
    let version =
        crate::fwupdate::validate_elf_file(staged.path(), crate::fwupdate::ARCH_ARM32, 100 * 1024)
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("validation: {e}") })),
                )
            })?;
    let slot = crate::fwupdate::commit_staged_slot(&state.fw_audio, &mut staged).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("slot write: {e}") })),
        )
    })?;
    let restarted = if q.restart {
        crate::fwupdate::restart_audio_firmware()
    } else {
        false
    };
    tracing::info!(
        "firmware update staged to slot {slot} (version {:?}, {} bytes), restart={}",
        version,
        size,
        restarted
    );
    Ok(Json(json!({
        "staged_slot": slot,
        "version": version,
        "size": size,
        "restarted": restarted,
    })))
}

pub(super) async fn firmware_core_update(
    State(state): State<Arc<AppState>>,
    mp: axum::extract::Multipart,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !unsigned_fwupdate_allowed() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(
                json!({ "error": "unsigned firmware upload disabled; use signed hm-release or set HM_ALLOW_UNSIGNED_FWUPDATE=1" }),
            ),
        ));
    }
    if !state.fw_core.available() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "core slot directory not present on this host" })),
        ));
    }
    let (mut staged, size) =
        multipart_file_to_staging(mp, &state.fw_core, FIRMWARE_MAX_BYTES as u64).await?;
    crate::fwupdate::validate_elf_file(staged.path(), crate::fwupdate::ARCH_AARCH64, 1024 * 1024)
        .map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("validation: {e}") })),
        )
    })?;
    let slot = crate::fwupdate::commit_staged_slot(&state.fw_core, &mut staged).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("slot write: {e}") })),
        )
    })?;
    tracing::info!(
        "core update staged to slot {slot} ({} bytes) — exiting for supervisor respawn",
        size
    );
    // Respond first; the supervisor restarts us from the flipped `current`
    // within seconds. The console shows the reconnect.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        std::process::exit(0);
    });
    Ok(Json(
        json!({ "staged_slot": slot, "size": size, "restarting": true }),
    ))
}

// --- per-unit acoustic profile + night LED / orientation (#57 / #19 / #35) ----

#[derive(Deserialize, Default)]
pub(super) struct DeviceProfilePut {
    #[serde(default)]
    acoustic: Option<crate::config::UnitAcousticProfile>,
    #[serde(default)]
    unit_acoustic_profile: Option<crate::config::UnitAcousticProfile>,
    #[serde(default)]
    volume_cap: Option<f64>,
    #[serde(default)]
    mix_gain: Option<f32>,
    #[serde(default)]
    vad_threshold: Option<f32>,
    #[serde(default)]
    onset_threshold_dbfs: Option<f32>,
    #[serde(default)]
    mic_gain_offset_db: Option<f32>,
    #[serde(default)]
    mic_calibrated: Option<bool>,
    #[serde(default)]
    led: Option<crate::config::DeviceLedConfig>,
    #[serde(default)]
    led_config: Option<crate::config::DeviceLedConfig>,
    #[serde(default)]
    night_enabled: Option<bool>,
    #[serde(default)]
    night_start_minute: Option<u16>,
    #[serde(default)]
    night_end_minute: Option<u16>,
    #[serde(default)]
    night_brightness_pct: Option<u8>,
    #[serde(default)]
    orientation_rotation: Option<u8>,
    #[serde(default)]
    orientation_reverse: Option<bool>,
}

pub(super) async fn device_profile_get(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let acoustic = state.config.unit_acoustic_profile(&id).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    let led = state.config.device_led_config(&id).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    let (acoustics_state, acoustics_receipt) = match state
        .hub
        .device_config_blocks(&id)
        .get(&crate::gateway::ConfigBlock::Acoustics)
    {
        Some(block) => {
            let st = match block.state {
                crate::gateway::ConfigReceiptStatus::Active => "applied",
                crate::gateway::ConfigReceiptStatus::Failed
                | crate::gateway::ConfigReceiptStatus::Unsupported => "failed",
                crate::gateway::ConfigReceiptStatus::Superseded => "superseded",
                crate::gateway::ConfigReceiptStatus::Validated
                | crate::gateway::ConfigReceiptStatus::Persisted
                | crate::gateway::ConfigReceiptStatus::Unknown => "saved_pending",
            };
            (st, serde_json::to_value(block).unwrap_or(json!(null)))
        }
        None => ("saved_pending", json!(null)),
    };
    Ok(Json(json!({
        "device": id,
        "acoustic": acoustic,
        "unit_acoustic_profile": acoustic,
        "acoustics_state": acoustics_state,
        "acoustics_receipt": acoustics_receipt,
        "led": led,
        "led_config": led,
    })))
}

pub(super) async fn device_profile_put(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<DeviceProfilePut>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let has_flat_acoustic = body.volume_cap.is_some()
        || body.mix_gain.is_some()
        || body.vad_threshold.is_some()
        || body.onset_threshold_dbfs.is_some()
        || body.mic_gain_offset_db.is_some()
        || body.mic_calibrated.is_some();
    let has_acoustic =
        body.acoustic.is_some() || body.unit_acoustic_profile.is_some() || has_flat_acoustic;
    let pending_acoustic = if let Some(a) = body.acoustic.or(body.unit_acoustic_profile) {
        a.validate().map_err(bad_request)?;
        Some(a)
    } else if has_flat_acoustic {
        let mut a = state
            .config
            .unit_acoustic_profile(&id)
            .map_err(bad_request)?;
        if let Some(v) = body.volume_cap {
            a.volume_cap = v;
        }
        if let Some(v) = body.mix_gain {
            a.mix_gain = v;
        }
        if let Some(v) = body.vad_threshold {
            a.vad_threshold = v;
        }
        if let Some(v) = body.onset_threshold_dbfs {
            a.onset_threshold_dbfs = Some(v);
        }
        if let Some(v) = body.mic_gain_offset_db {
            a.mic_gain_offset_db = v;
        }
        if let Some(v) = body.mic_calibrated {
            a.mic_calibrated = v;
        }
        a.validate().map_err(bad_request)?;
        Some(a)
    } else {
        None
    };

    let has_led_obj = body.led.is_some() || body.led_config.is_some();
    let has_flat_led = body.night_enabled.is_some()
        || body.night_start_minute.is_some()
        || body.night_end_minute.is_some()
        || body.night_brightness_pct.is_some()
        || body.orientation_rotation.is_some()
        || body.orientation_reverse.is_some();
    let led = if let Some(l) = body.led.or(body.led_config) {
        state
            .config
            .save_device_led_config(&id, l, "console")
            .map_err(bad_request)?
    } else if has_flat_led {
        let mut l = state.config.device_led_config(&id).map_err(bad_request)?;
        if let Some(v) = body.night_enabled {
            l.night_enabled = v;
        }
        if let Some(v) = body.night_start_minute {
            l.night_start_minute = v;
        }
        if let Some(v) = body.night_end_minute {
            l.night_end_minute = v;
        }
        if let Some(v) = body.night_brightness_pct {
            l.night_brightness_pct = v;
        }
        if let Some(v) = body.orientation_rotation {
            l.orientation_rotation = v;
        }
        if let Some(v) = body.orientation_reverse {
            l.orientation_reverse = v;
        }
        state
            .config
            .save_device_led_config(&id, l, "console")
            .map_err(bad_request)?
    } else {
        state.config.device_led_config(&id).map_err(bad_request)?
    };

    let (acoustic, acoustics_state, acoustics_pushed, acoustics_error, acoustics_receipt) =
        if let Some(candidate) = pending_acoustic {
            let outcome =
                crate::config_delivery::dispatch_unit_acoustics(&state, &id, &candidate).await;
            let persisted = if matches!(outcome.state, "applied" | "saved_pending") {
                state
                    .config
                    .save_unit_acoustic_profile(&id, candidate, "console")
                    .map_err(bad_request)?
            } else {
                state
                    .config
                    .unit_acoustic_profile(&id)
                    .map_err(bad_request)?
            };
            (
                persisted,
                outcome.state,
                outcome.state == "applied",
                outcome.error,
                outcome.receipt,
            )
        } else {
            let existing = state
                .config
                .unit_acoustic_profile(&id)
                .map_err(bad_request)?;
            (existing, "unchanged", false, None, json!(null))
        };

    let has_led = has_led_obj || has_flat_led;
    let mut led_pushed = false;
    if has_led {
        let has_v2 = state.hub.device_supports(&id, "config_apply_v2");
        if state
            .hub
            .supports_config_apply_v2(&id, crate::gateway::ConfigBlock::Led)
            .is_ok()
        {
            let outcome =
                crate::config_delivery::dispatch_led_v2(&state.store, &state.hub, &id, &led).await;
            led_pushed = outcome.state == "applied";
        } else if !has_v2 {
            led_pushed = state
                .hub
                .send_led_config(
                    &id,
                    led.night_enabled,
                    led.night_start_minute,
                    led.night_end_minute,
                    led.night_brightness_pct,
                    led.orientation_rotation,
                    led.orientation_reverse,
                )
                .is_ok();
        }
        // V2 devices without `config_block_led` (e.g. Android) skip cleanly with `led_pushed: false`.
    }

    let overall_state = if has_acoustic && matches!(acoustics_state, "failed" | "superseded") {
        acoustics_state
    } else if (has_acoustic && !acoustics_pushed) || (has_led && !led_pushed) {
        "saved_pending"
    } else {
        "applied"
    };

    Ok(Json(json!({
        "state": overall_state,
        "device": id,
        "acoustic": acoustic,
        "unit_acoustic_profile": acoustic,
        "acoustics_state": acoustics_state,
        "acoustics_pushed": acoustics_pushed,
        "acoustics_error": acoustics_error,
        "acoustics_receipt": acoustics_receipt,
        "led": led,
        "led_config": led,
        "led_pushed": led_pushed,
    })))
}

// --- release management (M8a) -------------------------------------------------
//
// The core is NOT a verifier: it spawns the bootstrapped `hm-release` binary
// (Ed25519 public key compiled in) and exposes its durable state. Activation
// restarts the core, so jobs are detached children that survive this process.
