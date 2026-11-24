use super::*;
use crate::config::{
    PersonalEndpointConfig, JOKE_KEY, JOKE_TOKEN_SECRET, LEARNING_KEY, LEARNING_TOKEN_SECRET,
    RADIO_KEY, RADIO_TOKEN_SECRET, STORY_KEY, STORY_TOKEN_SECRET,
};

#[derive(Deserialize)]
pub(in crate::api) struct PersonalEndpointPut {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    url: String,
    timeout_ms: Option<u64>,
    bearer_token: Option<String>,
}

fn personal_view(
    state: &AppState,
    key: &str,
    secret_key: &str,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let cfg = state.config.personal_endpoint(key).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    let token_set = state
        .config
        .personal_token(secret_key)
        .ok()
        .flatten()
        .is_some();
    Ok(json!({
        "enabled": cfg.enabled,
        "url": cfg.url,
        "timeout_ms": cfg.timeout_ms,
        "bearer_token_set": token_set,
        "state": if cfg.is_ready() { "ready" } else { "unconfigured" },
    }))
}

fn save_personal(
    state: &AppState,
    key: &str,
    secret_key: &str,
    body: PersonalEndpointPut,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let tools_before = service_fingerprint(state);
    let cfg = PersonalEndpointConfig {
        enabled: body.enabled,
        url: body.url,
        timeout_ms: body
            .timeout_ms
            .unwrap_or(crate::config::DEFAULT_PERSONAL_TIMEOUT_MS),
    };
    let (saved, token_set) = state
        .config
        .save_personal_endpoint(
            key,
            secret_key,
            cfg,
            body.bearer_token.as_deref(),
            "console",
        )
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": e.to_string() })),
            )
        })?;
    reset_device_history_if_capabilities_changed(state, tools_before);
    Ok(json!({
        "enabled": saved.enabled,
        "url": saved.url,
        "timeout_ms": saved.timeout_ms,
        "bearer_token_set": token_set,
        "state": if saved.is_ready() { "ready" } else { "unconfigured" },
    }))
}

pub(in crate::api) async fn config_story_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    personal_view(&state, STORY_KEY, STORY_TOKEN_SECRET).map(Json)
}

pub(in crate::api) async fn config_story_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PersonalEndpointPut>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    save_personal(&state, STORY_KEY, STORY_TOKEN_SECRET, body).map(Json)
}

pub(in crate::api) async fn config_story_test(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.personal_media.test_story().await)
}

pub(in crate::api) async fn config_joke_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    personal_view(&state, JOKE_KEY, JOKE_TOKEN_SECRET).map(Json)
}

pub(in crate::api) async fn config_joke_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PersonalEndpointPut>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    save_personal(&state, JOKE_KEY, JOKE_TOKEN_SECRET, body).map(Json)
}

pub(in crate::api) async fn config_joke_test(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.personal_media.test_joke().await)
}

pub(in crate::api) async fn config_radio_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    personal_view(&state, RADIO_KEY, RADIO_TOKEN_SECRET).map(Json)
}

pub(in crate::api) async fn config_radio_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PersonalEndpointPut>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    save_personal(&state, RADIO_KEY, RADIO_TOKEN_SECRET, body).map(Json)
}

pub(in crate::api) async fn config_radio_test(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.personal_media.test_radio().await)
}

pub(in crate::api) async fn config_learning_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    personal_view(&state, LEARNING_KEY, LEARNING_TOKEN_SECRET).map(Json)
}

pub(in crate::api) async fn config_learning_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PersonalEndpointPut>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    save_personal(&state, LEARNING_KEY, LEARNING_TOKEN_SECRET, body).map(Json)
}

pub(in crate::api) async fn config_learning_test(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    Json(state.learning.test_connection().await)
}

// --- Master Brain (#267 Slice 2) --------------------------------------------

#[derive(Deserialize)]
pub(in crate::api) struct MasterBrainPut {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    protocol: Option<crate::config::MasterBrainProtocol>,
    #[serde(default)]
    url: String,
    #[serde(default)]
    callback_url: String,
    timeout_ms: Option<u64>,
    bearer_token: Option<String>,
}

pub(in crate::api) async fn config_master_brain_get(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    Json(state.master_brain.status_json())
}

pub(in crate::api) async fn config_master_brain_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MasterBrainPut>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let tools_before = service_fingerprint(&state);
    let existing = state.master_brain.config().0;
    let cfg = crate::config::MasterBrainConfig {
        enabled: body.enabled,
        protocol: body.protocol.unwrap_or(existing.protocol),
        url: body.url,
        callback_url: body.callback_url,
        timeout_ms: body.timeout_ms.unwrap_or(existing.timeout_ms),
    };
    state
        .config
        .save_master_brain(cfg, body.bearer_token.as_deref(), "console")
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": e.to_string() })),
            )
        })?;
    reset_device_history_if_capabilities_changed(&state, tools_before);
    Ok(Json(state.master_brain.status_json()))
}

pub(in crate::api) async fn config_master_brain_test(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    Json(state.master_brain.test_connection().await)
}
