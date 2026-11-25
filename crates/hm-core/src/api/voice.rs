use super::*;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;

#[derive(Deserialize, Default)]
pub(super) struct ModeQuery {
    #[serde(default)]
    device: Option<String>,
}

pub(super) async fn mode_get(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ModeQuery>,
) -> Json<Value> {
    let mut snapshot = state.voice.mode_snapshot_for(q.device.as_deref());
    // R1j/P0-b: the home service's state belongs in the same snapshot the
    // console already polls (voice settings + per-device facts). Read from
    // the runtime that would execute the tool, never from a UI constant.
    if let Some(obj) = snapshot.as_object_mut() {
        obj.insert("home".into(), crate::tool::home::status_json(&state.home));
        // R2-BM6: news/OpenAPI are registered services too. The console card,
        // the prompt and the self-check read the same state words.
        obj.insert("news".into(), crate::tool::news::status_json(&state.news));
        obj.insert(
            "openapi".into(),
            crate::tool::openapi::status_json(&state.openapi),
        );
        // M2.5: the same state word the card, the prompt and the probe read.
        obj.insert(
            "search".into(),
            crate::tool::search::status_json(&state.search),
        );
        // #267: provider-agnostic Master Brain status & recent delegated tasks.
        obj.insert("master_brain".into(), state.master_brain.status_json());
        // M3: the live media sessions (queue + position), per device. The
        // console and the acceptance tools read the same facts the voice path
        // acts on.
        obj.insert("media".into(), state.music.sessions_json());
        obj.insert("music_playback".into(), json!(state.music.playback_facts()));
    }
    Json(snapshot)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PromptPut {
    assistant_name: Option<String>,
    persona_prompt: Option<String>,
    #[serde(default, deserialize_with = "crate::voice::deserialize_override_patch")]
    system_prompt_override: Option<Option<String>>,
}
pub(super) async fn prompts_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PromptPut>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    state
        .voice
        .set_prompts(
            body.assistant_name,
            body.persona_prompt,
            body.system_prompt_override,
            "console",
        )
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(json!({"error":e}))))
}

#[derive(Deserialize)]
pub(super) struct SessionCloseBody {
    device: String,
    generation: Option<u64>,
}

pub(super) async fn session_close(
    State(state): State<Arc<AppState>>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
    Json(body): Json<SessionCloseBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    super::reject_lan_exempt_loopback_device(&state, lan_exempt.as_ref(), &body.device)?;
    if !state.hub.device_exists(&body.device) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error":"unknown device"})),
        ));
    }
    state
        .voice
        .close_session(&body.device, body.generation)
        .map(Json)
        .map_err(|e| {
            let status = if e == "generation_mismatch" {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(json!({"error":e})))
        })
}

#[derive(Deserialize)]
pub(super) struct ModePut {
    #[serde(flatten)]
    follow_window: crate::voice::FollowWindowPatch,
    duplex_requested: Option<String>,
    proactive_speech: Option<bool>,
    /// Global wake words (core-side KWS, G4 console debt). Replaces the
    /// whole list on write; rides the config-history/restore chain.
    #[serde(default)]
    wake_words: Option<Vec<String>>,
    /// Measured VAD thresholds (G4 calibration), applied live.
    #[serde(default)]
    vad: Option<crate::voice::VadSettings>,
    /// Follow-window end tone toggle (stage C UX).
    #[serde(default)]
    follow_window_tone: Option<bool>,
    /// Local wake-ack phrases (user directive 2026-11-09): bare wakes
    /// answer from pre-synthesized device-local WAVs, never the cloud.
    #[serde(default)]
    wake_ack: Option<crate::voice::WakeAckPatch>,
    /// Deterministic retreat intent ("退下" family) toggle.
    #[serde(default)]
    retreat_intent: Option<bool>,
    /// The session-end words themselves (replaces the whole list).
    #[serde(default)]
    retreat_words: Option<Vec<String>>,
    /// The retreat ACK phrase (was a fixed "好嘞"; one phrase, pre-synthesized
    /// with the wake acks).
    #[serde(default)]
    retreat_ack: Option<String>,
    /// Follow-window timeout announcement, independent of the retreat reply.
    #[serde(default)]
    window_close_ack: Option<String>,
    /// Post-play tail blind window in ms (T8 echo governance): 0 disables,
    /// 2000 max. As the device AEC reaches the ≥20dB ERLE target this
    /// demotes from main defense to insurance.
    #[serde(default)]
    tail_blind_ms: Option<u64>,
    /// Media-channel output trim in dB (0..-12, default -6). Applied to
    /// media plays only; voice is never trimmed.
    #[serde(default)]
    media_trim_db: Option<f64>,
    /// D/T4: stop a live VOICE playback when the device's local spotter
    /// hears the wake phrase. Off = detections recorded, nothing acts.
    #[serde(default)]
    keyword_interrupt: Option<bool>,
    /// D/T5: stop a live VOICE playback on ANY genuine near-end speech
    /// (device-side onset + echo cross-correlation guard) and treat the
    /// words as a follow-up turn. Off = facts recorded, nothing acts.
    #[serde(default)]
    barge_in: Option<bool>,
    /// D/T5 R1: answer protection period in ms (0–3000, default 800).
    /// Barge-in facts inside it are ignored — a fresh answer's TTS onset
    /// decorrelates the echo residual (the 2026-11-09 self-interrupt loop).
    #[serde(default)]
    barge_protect_ms: Option<u64>,
    /// B-M0 waiting cues ("垫话"): per-scene phrase lists (an empty list
    /// disables only that scene), the delay before a cue plays, and the
    /// command-only re-synthesis flag. Same validated path as the rest.
    #[serde(default)]
    filler: Option<crate::voice::FillerSettings>,
    /// B-M1 stop / keep-going intents: enabled flag plus both word lists.
    #[serde(default)]
    stop_intents: Option<crate::voice::StopIntentSettings>,
    /// R1h: the sentence spoken after a truncated program's last line.
    /// Empty = silent. Same pre-synthesis cache as every other local phrase.
    #[serde(default)]
    truncation_notice: Option<String>,
    /// R2 (2026-11-13): the spoken-delivery policy (sentences/characters per
    /// voice answer, what counts as a long-content request, the continuation
    /// clue). Partial patch so omitting `detail_words`/`long_form_words`
    /// preserves existing stored lists instead of overwriting with defaults.
    #[serde(default)]
    spoken: Option<crate::voice::SpokenDeliveryPatch>,
    /// 2026-11-14 (commercial privacy): whether idle room speech is
    /// transcribed so a missed keyword can still wake the assistant. Off =
    /// ASR runs only inside an open session. Global policy, applied live.
    #[serde(default)]
    asr_wake_fallback: Option<bool>,
    #[serde(default)]
    proactive_asr_wake: Option<bool>,
    /// v0.30 (wake arbitration): `off` | `observe` | `enforce`. Same
    /// management path, change record and applied-state feedback as every
    /// other interaction setting.
    #[serde(default)]
    wake_arbitration: Option<String>,
}

impl ModePut {
    /// Fields that alter prompts, privacy policy, lexicons, calibration, or
    /// synthesis jobs require an authenticated admin Bearer token — never
    /// unauthenticated `lan_gateway_exempt` LAN access.
    fn has_privileged_fields(&self) -> bool {
        let fw = &self.follow_window;
        fw.follow_window_enabled.is_some()
            || fw.follow_window_ms.is_some()
            || fw.context_window_ms.is_some()
            || fw.followup_credits.is_some()
            || fw.assistant_name.is_some()
            || fw.persona_prompt.is_some()
            || fw.system_prompt_override.is_some()
            || fw.kws_threshold.is_some()
            || fw.kws_score.is_some()
            || self.proactive_speech.is_some()
            || self.wake_words.is_some()
            || self.vad.is_some()
            || self.follow_window_tone.is_some()
            || self.wake_ack.as_ref().is_some_and(|w| {
                w.enabled.is_some() || w.phrases.is_some() || w.resynth == Some(true)
            })
            || self.retreat_intent.is_some()
            || self.retreat_words.is_some()
            || self.retreat_ack.is_some()
            || self.window_close_ack.is_some()
            || self.tail_blind_ms.is_some()
            || self.media_trim_db.is_some()
            || self.barge_in.is_some()
            || self.keyword_interrupt.is_some()
            || self.barge_protect_ms.is_some()
            || self.filler.is_some()
            || self.stop_intents.is_some()
            || self.truncation_notice.is_some()
            || self.spoken.is_some()
            || self.asr_wake_fallback.is_some()
            || self.proactive_asr_wake.is_some()
            || self.wake_arbitration.is_some()
    }
}

pub(super) async fn mode_put_global(
    State(state): State<Arc<AppState>>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
    Json(body): Json<ModePut>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if lan_exempt.is_some() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "admin bearer token required for global mode settings"
            })),
        ));
    }
    apply_mode_put(&state, "global", body)
}

pub(super) async fn mode_put(
    State(state): State<Arc<AppState>>,
    Path(device): Path<String>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
    Json(body): Json<ModePut>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    super::reject_lan_exempt_loopback_device(&state, lan_exempt.as_ref(), &device)?;
    if lan_exempt.is_some() && body.has_privileged_fields() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "admin bearer token required for privileged mode settings"
            })),
        ));
    }
    if !state.hub.device_exists(&device) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown device '{device}'") })),
        ));
    }
    apply_mode_put(&state, &device, body)
}

fn apply_mode_put(
    state: &Arc<AppState>,
    device: &str,
    body: ModePut,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let wake_ack = body.wake_ack;
    let mut patch = body.follow_window;
    patch.spoken_patch = body.spoken;
    patch.idle_asr_enabled = body.proactive_asr_wake.or(body.asr_wake_fallback);
    patch.wake_arbitration = body
        .wake_arbitration
        .as_deref()
        .map(|mode| {
            crate::voice::WakeArbitration::parse(mode).ok_or_else(|| {
                bad_request(r#"wake_arbitration must be "off", "observe" or "enforce""#)
            })
        })
        .transpose()?;
    match state.voice.set_mode_with_follow_window(
        device,
        body.duplex_requested,
        body.proactive_speech,
        body.wake_words,
        body.vad,
        body.follow_window_tone,
        wake_ack,
        body.retreat_intent,
        body.retreat_words,
        body.retreat_ack,
        body.tail_blind_ms,
        body.media_trim_db,
        body.keyword_interrupt,
        body.barge_in,
        body.barge_protect_ms,
        body.filler,
        body.stop_intents,
        body.truncation_notice,
        body.window_close_ack,
        patch,
        "console",
    ) {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

pub(super) async fn voice_log(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({ "events": state.voice.recent(100) }))
}

/// M6b (S3): the EXPORTABLE journal — the console's `/voice/log` stays the
/// last 100; acceptance needs the whole window.
///
/// `since` is an exclusive wall-clock cursor (`ts_ms > since`). `since_seq`
/// is the exact cursor and wins when `since_epoch` equals the running core's
/// epoch (a sequence from a previous process would skip the new epoch).
#[derive(serde::Deserialize)]
pub(super) struct VoiceEventsQuery {
    #[serde(default)]
    since: u64,
    #[serde(default)]
    since_seq: Option<u64>,
    #[serde(default)]
    since_epoch: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    tail: bool,
}

pub(super) async fn voice_events(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<VoiceEventsQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(crate::voice_events::EXPORT_DEFAULT_LIMIT);
    // The export parses up to the whole capped directory (32 MiB). That is
    // fine for the acceptance tool but must not park a tokio worker while
    // other requests wait.
    let voice = state.voice.clone();
    let out = tokio::task::spawn_blocking(move || {
        voice.export_events_with_tail(q.since, q.since_seq, q.since_epoch, limit, q.tail)
    })
    .await
    .unwrap_or_else(|e| json!({ "events": [], "error": format!("export failed: {e}") }));
    Json(out)
}

pub(super) async fn voice_announce_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.voice.announce_snapshot())
}

pub(super) async fn voice_announce_post(
    State(state): State<Arc<AppState>>,
    Json(body): Json<crate::voice::AnnounceRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let task_known = body
        .task_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some_and(|tid| {
            state.tasks.exists(tid) || state.master_brain.task_snapshot(tid).is_some()
        });
    state
        .voice
        .enqueue_announce_checked(body, task_known)
        .map(|v| (StatusCode::ACCEPTED, Json(v)))
        .map_err(|e| {
            let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_REQUEST);
            (status, Json(json!({ "error": e.detail, "code": e.code })))
        })
}

// --- chat -------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChatRequest {
    scope_binding_id: Option<String>,
    session_id: Option<String>,
    text: String,
    #[serde(default = "default_source")]
    source: String,
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    room: Option<String>,
    #[serde(default)]
    person_id: Option<String>,
}

fn default_source() -> String {
    "text".into()
}

pub(super) async fn chat(
    State(state): State<Arc<AppState>>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, (StatusCode, Json<Value>)>
{
    if req.text.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "text required" })),
        ));
    }
    let env = state
        .agent_env()
        .map_err(|e| (StatusCode::CONFLICT, Json(json!({ "error": e }))))?;
    if req.device_id.is_some() || req.room.is_some() || req.person_id.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"SCOPE_BINDING_REQUIRED"})),
        ));
    }
    let token = crate::execution::api::token(&headers);
    let principal = if token.is_empty() {
        None
    } else {
        Some(
            state
                .execution
                .principal(token)
                .map_err(crate::execution::api::error)?,
        )
    };
    let (authority, session) = if let Some(id) = req.scope_binding_id {
        let principal =
            principal.ok_or_else(|| crate::execution::api::error("PAIRING_REQUIRED".into()))?;
        let (authority, session) = state
            .execution
            .resolve(&id, &principal, req.session_id.as_deref())
            .map_err(crate::execution::api::error)?;
        (authority, Some(session))
    } else {
        let authority = crate::execution::Authority::anonymous(state.store.clone(), principal)
            .map_err(crate::execution::api::error)?
            .with_lan_exempt(lan_exempt.is_some());
        if req
            .session_id
            .as_deref()
            .is_some_and(|session| session.starts_with("dev:"))
        {
            return Err(crate::execution::api::error(
                "PHYSICAL_SESSION_REUSE_REJECTED".into(),
            ));
        }
        let session = match req.session_id {
            Some(session)
                if state
                    .sessions
                    .exists(&session)
                    .map_err(|e| crate::execution::api::error(e.to_string()))? =>
            {
                authority
                    .bind_session(&session)
                    .map_err(crate::execution::api::error)?;
                Some(session)
            }
            // Preserve create-new semantics; never reserve caller-invented IDs.
            _ => None,
        };
        (authority, session)
    };
    // Console text is READ, not heard: the full answer IS the answer, so the
    // spoken budget (a voice-turn rule) does not shape this prompt.
    let stream = crate::agent::run_turn_authorized(
        env,
        session,
        req.text,
        req.source,
        crate::agent::prompt::Delivery::Text,
        Arc::new(crate::task::CancellationScope::default()),
        Ok(authority),
    );
    let sse = stream.map(|ev| {
        Ok(Event::default()
            .data(serde_json::to_string(&ev).unwrap_or_else(|_| "{\"type\":\"error\"}".into())))
    });
    Ok(Sse::new(sse).keep_alive(KeepAlive::default()))
}

// --- cancel -----------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct CancelRequest {
    task_id: String,
}

pub(super) async fn cancel(
    State(state): State<Arc<AppState>>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
    Json(req): Json<CancelRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if lan_exempt.is_some() {
        if let Some(session_id) = state.tasks.session_id_for(&req.task_id) {
            super::reject_lan_exempt_loopback_session(&state, lan_exempt.as_ref(), &session_id)?;
        }
    }
    Ok(match state.tasks.request_cancel(&req.task_id) {
        Some(true) => Json(json!({ "task_id": req.task_id, "result": "cancel_requested" })),
        Some(false) => Json(json!({ "task_id": req.task_id, "result": "not_running" })),
        None => Json(json!({ "task_id": req.task_id, "result": "unknown_task" })),
    })
}

pub(super) async fn session_messages(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    lan_exempt: Option<axum::Extension<super::LanExemptUnauthenticated>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    super::reject_lan_exempt_loopback_session(&state, lan_exempt.as_ref(), &id)?;
    match state.sessions.messages(&id) {
        Ok(msgs) => Ok(Json(json!({ "session_id": id, "messages": msgs }))),
        Err(e) => Ok(Json(json!({ "error": e.to_string() }))),
    }
}

/// Clear a session's conversation context (the management counterpart of
/// 「退下＝清零」; traces stay as the diagnostic record).
pub(super) async fn session_clear(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    match state.sessions.clear(&id) {
        Ok(n) => Json(json!({ "session_id": id, "cleared": n })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

// --- traces / tasks ----------------------------------------------------------

pub(super) async fn trace_list(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let limit = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(50);
    match state.traces.list(limit.min(500)) {
        Ok(list) => Json(json!({ "traces": list })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

pub(super) async fn trace_detail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    match state.traces.detail(&id) {
        Ok(Some(d)) => Json(d),
        _ => Json(json!({ "error": "not found" })),
    }
}

pub(super) async fn tasks_list(State(state): State<Arc<AppState>>) -> Json<Value> {
    match state.tasks.list(100) {
        Ok(list) => Json(json!({ "tasks": list })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

pub(super) async fn reflex_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    let cfg = state.config.reflex().unwrap_or_default();
    let compiled = crate::config::compile_keywords(&cfg);
    Json(json!({
        "words": cfg.words,
        "compiled": compiled.as_deref().unwrap_or(""),
        "hash": compiled.as_deref().map(|c| crate::config::fnv1a_hex(c.as_bytes())).unwrap_or_default(),
        "error": compiled.err(),
        "warnings": crate::config::reflex_warnings(&cfg),
    }))
}

#[derive(Deserialize)]
pub(super) struct ReflexBody {
    #[serde(default)]
    words: Option<Vec<crate::config::ReflexWord>>,
    /// Also deploy right after saving (one click from the console).
    #[serde(default)]
    deploy: bool,
    #[serde(default)]
    device: Option<String>,
}

pub(super) async fn reflex_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ReflexBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let current = state.config.reflex().unwrap_or_default();
    let cfg = crate::config::ReflexConfig {
        words: body.words.unwrap_or(current.words),
    };
    state.config.save_reflex(cfg, "console").map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    let saved = state.config.reflex().unwrap_or_default();
    let compiled = crate::config::compile_keywords(&saved)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))))?;
    let mut out = json!({
        "state": "saved",
        "words": saved.words,
        "compiled": compiled,
        "hash": crate::config::fnv1a_hex(compiled.as_bytes()),
        "warnings": crate::config::reflex_warnings(&saved),
    });
    if body.deploy {
        let targets: Vec<String> = match body
            .device
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(d) => vec![d.to_string()],
            None => {
                let mut devs = state.hub.devices_with_cap("audio_in");
                if devs.is_empty() {
                    devs = state.hub.audio_out_devices();
                }
                devs
            }
        };
        if targets.is_empty() {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({ "error": "no audio device online" })),
            ));
        }
        let mut results = Vec::with_capacity(targets.len());
        let mut first_ok: Option<Value> = None;
        let mut first_err: Option<String> = None;
        for dev in &targets {
            match state.voice.deploy_reflex_auto(dev).await {
                Ok(deployed) => {
                    if first_ok.is_none() {
                        first_ok = Some(deployed.clone());
                    }
                    results.push(deployed);
                }
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e.clone());
                    }
                    results.push(json!({ "device": dev, "error": e }));
                }
            }
        }
        if let Some(mut primary) = first_ok {
            if targets.len() > 1 {
                if let Some(obj) = primary.as_object_mut() {
                    obj.insert("devices".into(), json!(results));
                }
            }
            out["deploy"] = primary;
        } else {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({ "error": first_err.unwrap_or_else(|| "deploy failed".into()) })),
            ));
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub(super) struct ReflexTokensQuery {
    #[serde(default)]
    text: String,
}

/// R1j/P1: 汉字 → device token spelling. The console fills the field from
/// this; the same function runs when a word is saved without tokens, so a
/// word is entered once.
#[derive(Deserialize)]
pub(super) struct AudioEqBody {
    #[serde(default)]
    curve: Option<String>,
    #[serde(default)]
    bands: Option<Vec<crate::config::EqBandCfg>>,
}

pub(super) async fn config_audio_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    let cfg = state.config.audio_eq().unwrap_or_default();
    // The device's applied tone is a device fact: it is in the firmware log
    // and the journal (`audio_eq_sent`), not guessed here.
    let devices: Vec<String> = state.hub.audio_out_devices();
    Json(json!({
        "curve": cfg.curve,
        "bands": cfg.bands,
        "curves": crate::config::AUDIO_CURVES,
        "default": crate::config::AudioEqConfig::default(),
        // Display-only: the voicing's linear part (the device also runs the
        // bass guard and three compressors, so the chart is not the whole
        // response; the console labels it as such).
        "voicing": crate::config::VOICING_REF
            .iter()
            .map(|&(freq, gain_db, q)| json!({ "freq": freq, "gain_db": gain_db, "q": q }))
            .collect::<Vec<_>>(),
        "devices": devices,
    }))
}

pub(super) async fn config_audio_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AudioEqBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let current = state.config.audio_eq().unwrap_or_default();
    let cfg = crate::config::AudioEqConfig {
        curve: body.curve.unwrap_or(current.curve),
        bands: body.bands.unwrap_or(current.bands),
    };
    let saved = state.config.save_audio_eq(cfg, "console").map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    // Apply to legacy (pre-v2) audio-out devices and v2 devices that advertise
    // `config_block_audio_eq` (or `audio_eq`); skip v2 gateways (e.g. Android)
    // that advertise `config_apply_v2` without EQ support.
    let devices: Vec<String> = state
        .hub
        .audio_out_devices()
        .into_iter()
        .filter(|d| {
            !state.hub.device_supports(d, "config_apply_v2")
                || state.hub.device_supports(d, "config_block_audio_eq")
                || state.hub.device_supports(d, "audio_eq")
        })
        .collect();
    let mut sent = Vec::new();
    let mut errors = Vec::new();
    for device in devices {
        match state.voice.push_audio_eq(&device) {
            Ok(()) => sent.push(device),
            Err(e) => errors.push(json!({ "device": device, "error": e })),
        }
    }
    Ok(Json(json!({
        "state": if errors.is_empty() { "applied" } else { "saved_not_applied" },
        "curve": saved.curve,
        "bands": saved.bands,
        "sent": sent,
        "errors": errors,
    })))
}

pub(super) async fn reflex_tokens_get(Query(q): Query<ReflexTokensQuery>) -> Json<Value> {
    Json(match crate::config::reflex_tokens(&q.text) {
        Ok(tokens) => json!({ "text": q.text, "tokens": tokens, "error": null }),
        Err(e) => json!({ "text": q.text, "tokens": "", "error": e }),
    })
}

pub(super) async fn reflex_deploy(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // An EMPTY body means "deploy the saved table" — the natural CLI call.
    // `Option<Json<_>>` refused it with 415 whenever a client declared an
    // empty body (urllib sends Content-Length: 0), so the body is read as
    // bytes: absent and empty are the same thing, malformed JSON is a named
    // 400, never silently defaulted.
    let body = if body.is_empty() {
        ReflexBody {
            words: None,
            deploy: true,
            device: None,
        }
    } else {
        serde_json::from_slice::<ReflexBody>(&body).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("body: {e}") })),
            )
        })?
    };
    let targets: Vec<String> = match body
        .device
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(d) => vec![d.to_string()],
        None => {
            let mut devs = state.hub.devices_with_cap("audio_in");
            if devs.is_empty() {
                devs = state.hub.audio_out_devices();
            }
            devs
        }
    };
    if targets.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": "no audio device online" })),
        ));
    }
    let mut results = Vec::with_capacity(targets.len());
    let mut first_ok: Option<Value> = None;
    let mut first_err: Option<String> = None;
    for dev in &targets {
        match state.voice.deploy_reflex_auto(dev).await {
            Ok(deployed) => {
                if first_ok.is_none() {
                    first_ok = Some(deployed.clone());
                }
                results.push(deployed);
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e.clone());
                }
                results.push(json!({ "device": dev, "error": e }));
            }
        }
    }
    if let Some(mut primary) = first_ok {
        if targets.len() > 1 {
            if let Some(obj) = primary.as_object_mut() {
                obj.insert("devices".into(), json!(results));
            }
        }
        Ok(Json(primary))
    } else {
        Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": first_err.unwrap_or_else(|| "deploy failed".into()) })),
        ))
    }
}
