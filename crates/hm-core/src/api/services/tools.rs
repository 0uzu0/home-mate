use super::reset_device_history_if_capabilities_changed;
use super::service_fingerprint;
use crate::api::*;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// Music configuration (R1e). Separate from the service cards because the
/// shape is different (two connections, two secrets, local-first) — and
/// because a household with no music must keep working.
pub(in crate::api) async fn config_music_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    match state.config.music_status() {
        Ok(s) => Json(json!(s)),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

#[derive(Deserialize)]
pub(in crate::api) struct MusicConnectionBody {
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    user: Option<String>,
    /// Write-only. Absent = keep the stored value; "" = clear it.
    #[serde(default)]
    secret: Option<String>,
}

#[derive(Deserialize)]
pub(in crate::api) struct MusicBody {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_true_body")]
    prefer_local: bool,
    #[serde(default)]
    library: Option<MusicConnectionBody>,
    #[serde(default)]
    online: Option<MusicConnectionBody>,
    /// Revision the editor loaded. When present it must still match; a stale
    /// form is refused instead of silently overwriting another page's save
    /// (the 2026-11-12 12:39 Navidrome loss).
    #[serde(default)]
    revision: Option<String>,
}

fn default_true_body() -> bool {
    true
}

pub(in crate::api) async fn config_music_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MusicBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cfg = crate::config::MusicConfig {
        enabled: body.enabled,
        prefer_local: body.prefer_local,
        library: body
            .library
            .as_ref()
            .map(|c| crate::config::MusicConnection {
                endpoint: c.endpoint.trim().to_string(),
                user: c.user.as_deref().unwrap_or_default().trim().into(),
            }),
        online: body
            .online
            .as_ref()
            .map(|c| crate::config::MusicConnection {
                endpoint: c.endpoint.trim().to_string(),
                user: c.user.as_deref().unwrap_or_default().trim().into(),
            }),
    };
    match state.config.save_music_expected(
        cfg,
        body.library.as_ref().and_then(|c| c.secret.as_deref()),
        body.online.as_ref().and_then(|c| c.secret.as_deref()),
        "console",
        body.revision.as_deref(),
        body.online.as_ref().is_some_and(|c| c.user.is_none()),
    ) {
        Ok(st) => Ok(Json(json!(st))),
        Err(crate::config::ConfigError::ServiceConflict { revision }) => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "音乐配置已被另一个页面修改（本次未写入）；请刷新后重试",
                "revision": revision,
            })),
        )),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )),
    }
}

pub(in crate::api) async fn config_music_test(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.music.probe().await)
}

// --- home automation ---------------------------------------------------------

pub(in crate::api) async fn config_home_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(crate::tool::home::status_json(&state.home))
}

#[derive(Deserialize)]
pub(in crate::api) struct HomeBody {
    /// "none" (default) or "home_assistant". Anything else is a 400 —
    /// never a silently-wrong state.
    #[serde(default)]
    backend: Option<String>,
    /// Home Assistant root; absent keeps the stored value.
    #[serde(default)]
    base_url: Option<String>,
    /// Long-lived token: absent keeps, "" clears, any other value replaces.
    /// It is never echoed in any response.
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    entities: Option<Vec<crate::config::HomeEntity>>,
    /// R2-BM6.3: "api" (whole house, filtered here) or "exposed" (HA's Assist
    /// exposure, read-only here). Anything else is a 400.
    #[serde(default)]
    entity_source: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    retries: Option<u32>,
}

pub(in crate::api) async fn config_home_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<HomeBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let bad = |e: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    let tools_before = service_fingerprint(&state);
    let backend = match body.backend.as_deref().map(str::trim) {
        None => None,
        Some("" | "none") => Some(crate::config::HomeBackend::None),
        Some("home_assistant" | "home-assistant") => {
            Some(crate::config::HomeBackend::HomeAssistant)
        }
        Some(_) => {
            return Err(bad(
                "unknown home backend (expected none|home_assistant)".into()
            ))
        }
    };
    let entity_source = match body.entity_source.as_deref().map(str::trim) {
        None => None,
        Some("" | "api") => Some(crate::config::EntitySource::Api),
        Some("exposed" | "expose") => Some(crate::config::EntitySource::Exposed),
        Some(_) => return Err(bad("unknown entity_source (expected api|exposed)".into())),
    };
    let saved = state
        .config
        .edit_home(body.token.as_deref(), "console", |cfg| {
            if let Some(value) = backend {
                cfg.backend = value;
            }
            if let Some(value) = entity_source {
                cfg.entity_source = value;
            }
            if let Some(value) = body.base_url {
                cfg.base_url = value;
            }
            if let Some(value) = body.entities {
                cfg.entities = value;
            }
            if let Some(value) = body.timeout_ms {
                cfg.timeout_ms = value;
            }
            if let Some(value) = body.retries {
                cfg.retries = value;
            }
        })
        .map_err(|e| match e {
            crate::config::ConfigError::Store(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not save Home Assistant configuration" })),
            ),
            _ => bad(e.to_string()),
        })?;
    reset_device_history_if_capabilities_changed(&state, tools_before);
    Ok(Json(crate::tool::home::status_from(&saved)))
}

/// R2-BM5: a REAL probe. Verifies address, token and one whitelisted entity
/// (auth + state read) and returns the state word, latency and how many HTTP
/// requests were actually made. Unconfigured = zero requests. It never
/// changes an entity.
pub(in crate::api) async fn config_home_test(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.home.probe().await)
}

/// R2-BM6.1: read `GET /api/states` and return the house's entity catalogue
/// for the console picker. Read-only; nothing becomes callable until the
/// household saves a whitelist. Unconfigured = zero requests.
pub(in crate::api) async fn config_home_discover(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    Json(state.home.discover().await)
}

// --- news (R2-BM6): a registered service with the weather shape ------------

pub(in crate::api) async fn config_news_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(crate::tool::news::status_json(&state.news))
}

#[derive(Deserialize)]
pub(in crate::api) struct NewsBody {
    #[serde(default)]
    enabled: Option<bool>,
    /// Absent = keep the stored sources; present replaces the whole list.
    #[serde(default)]
    sources: Option<Vec<crate::config::NewsSource>>,
    #[serde(default)]
    default_alias: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_sources: Option<usize>,
    #[serde(default)]
    revision: Option<String>,
}

pub(in crate::api) async fn config_news_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<NewsBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let bad = |e: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    match state
        .config
        .edit_news("console", body.revision.as_deref(), |cfg| {
            if let Some(v) = body.enabled {
                cfg.enabled = v;
            }
            if let Some(sources) = body.sources {
                cfg.sources = sources;
            }
            if let Some(alias) = body.default_alias {
                cfg.default_alias = alias;
            }
            if let Some(ms) = body.timeout_ms {
                cfg.timeout_ms = ms;
            }
            if let Some(m) = body.max_sources {
                cfg.max_sources = m;
            }
        }) {
        Ok(st) => Ok(Json(json!(st))),
        Err(crate::config::ConfigError::ServiceConflict { revision }) => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "新闻配置已被另一个页面修改（本次未写入）；请刷新后重试",
                "revision": revision,
            })),
        )),
        Err(e) => Err(bad(e.to_string())),
    }
}

pub(in crate::api) async fn config_news_test(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.news.probe().await)
}

// --- general web search (M2.5): the weather/news shape --------------------

pub(in crate::api) async fn config_search_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(crate::tool::search::status_json(&state.search))
}

#[derive(Deserialize)]
pub(in crate::api) struct SearchBody {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    count: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Write-only: absent keeps, "" clears. Never read back.
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    revision: Option<String>,
}

pub(in crate::api) async fn config_search_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SearchBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let bad = |e: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    match state.config.edit_search(
        body.api_key.as_deref(),
        "console",
        body.revision.as_deref(),
        |cfg| {
            if let Some(v) = body.enabled {
                cfg.enabled = v;
            }
            if let Some(url) = body.base_url {
                cfg.base_url = url;
            }
            if let Some(c) = body.count {
                cfg.count = c.clamp(1, crate::config::MAX_SEARCH_COUNT as u64) as u8;
            }
            if let Some(ms) = body.timeout_ms {
                cfg.timeout_ms = ms;
            }
        },
    ) {
        Ok(st) => Ok(Json(json!(st))),
        Err(crate::config::ConfigError::ServiceConflict { revision }) => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "搜索配置已被另一个页面修改（本次未写入）；请刷新后重试",
                "revision": revision,
            })),
        )),
        Err(e) => Err(bad(e.to_string())),
    }
}

pub(in crate::api) async fn config_search_test(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.search.probe().await)
}

// --- OpenAPI query adapter (R2-BM6) ---------------------------------------

pub(in crate::api) async fn config_openapi_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(crate::tool::openapi::status_json(&state.openapi))
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(in crate::api) enum OpenApiOperationSelection {
    Id(String),
    Detailed {
        id: String,
        #[serde(default)]
        effect_level: Option<crate::config::EffectLevel>,
        #[serde(default)]
        verification: Option<crate::config::VerificationStrategy>,
    },
}

impl OpenApiOperationSelection {
    fn id(&self) -> &str {
        match self {
            Self::Id(s) => s,
            Self::Detailed { id, .. } => id,
        }
    }
}

#[derive(Deserialize)]
pub(in crate::api) struct OpenApiBody {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    spec_url: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    /// Optional extra auth header NAME (value = the stored token). Empty = off.
    #[serde(default)]
    auth_header: Option<String>,
    /// Write-only: absent keeps, "" clears.
    #[serde(default)]
    token: Option<String>,
    /// Whitelist as operation ids or detailed selections. Present = refetch
    /// the document and re-resolve; absent = keep the stored catalogue (a
    /// timeout-only edit must not require the service to be reachable).
    #[serde(default)]
    operations: Option<Vec<OpenApiOperationSelection>>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    retries: Option<u32>,
    #[serde(default)]
    revision: Option<String>,
}

pub(in crate::api) async fn config_openapi_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<OpenApiBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let bad = |e: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    let captured = state
        .config
        .openapi_snapshot()
        .map_err(|e| bad(e.to_string()))?;
    let captured_revision = captured.revision;
    if body
        .revision
        .as_ref()
        .is_some_and(|revision| revision != &captured_revision)
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "OpenAPI 配置已被另一个页面修改（本次未写入）；请刷新后重试",
                "revision": captured_revision,
            })),
        ));
    }
    let mut cfg = captured.config;
    // The token is bound to its source: changing the document URL must not
    // forward the stored credential to the new host (audit P1).
    let old_spec_url = cfg.spec_url.clone();
    if let Some(v) = body.enabled {
        cfg.enabled = v;
    }
    if let Some(url) = &body.spec_url {
        cfg.spec_url = url.trim().to_string();
    }
    if let Some(base) = &body.base_url {
        cfg.base_url = base.trim().to_string();
    }
    if let Some(h) = &body.auth_header {
        crate::config::validate_auth_header_name(h).map_err(bad)?;
        cfg.auth_header = h.trim().to_string();
    }
    if let Some(ms) = body.timeout_ms {
        cfg.timeout_ms = ms;
    }
    if let Some(r) = body.retries {
        cfg.retries = r;
    }
    let clearing = body
        .operations
        .as_ref()
        .map(|list| list.is_empty())
        .unwrap_or(false);
    if clearing {
        cfg.operations.clear();
    }
    let disabling = body.enabled == Some(false);
    if disabling {
        let url_changed = body
            .spec_url
            .as_deref()
            .map(|u| u.trim() != old_spec_url.trim())
            .unwrap_or(false);
        if url_changed {
            cfg.operations.clear();
        }
    }
    let needs_resolve = !clearing
        && !disabling
        && (body.operations.is_some() || (body.spec_url.is_some() && !cfg.operations.is_empty()));
    if needs_resolve {
        if cfg.spec_url.trim().is_empty() {
            return Err(bad("先填 OpenAPI 文档地址".into()));
        }
        let ids: Vec<String> = match &body.operations {
            Some(list) => list
                .iter()
                .map(|s| s.id().trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            None => cfg.operations.iter().map(|o| o.id.clone()).collect(),
        };
        let prev_ops = cfg.operations.clone();
        let token = crate::tool::openapi::token_for_source(
            &old_spec_url,
            captured.token.as_deref(),
            &cfg.spec_url,
            body.token.as_deref(),
        );
        let doc = state
            .openapi
            .fetch_spec_for_save(
                &cfg.spec_url,
                token.as_deref(),
                cfg.timeout_ms,
                &cfg.auth_header,
            )
            .await
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("{}: {}", e.code, e.detail), "code": e.code })),
                )
            })?;
        let mut resolved = crate::tool::openapi::resolve_operations(&doc, &ids).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("{}: {}", e.code, e.detail), "code": e.code })),
            )
        })?;
        if let Some(selections) = &body.operations {
            for op in resolved.iter_mut() {
                if let Some(OpenApiOperationSelection::Detailed {
                    effect_level,
                    verification,
                    ..
                }) = selections.iter().find(|s| s.id().trim() == op.id)
                {
                    if let Some(el) = effect_level {
                        op.effect_level = *el;
                    }
                    if let Some(v) = verification {
                        op.verification = Some(v.clone());
                    }
                }
            }
        } else {
            for op in resolved.iter_mut() {
                if let Some(prev) = prev_ops.iter().find(|p| p.id == op.id) {
                    op.effect_level = prev.effect_level;
                    op.verification = prev.verification.clone();
                }
            }
        }
        cfg.operations = resolved;
        if let Some(t) = token.as_deref() {
            for op in cfg.operations.iter_mut() {
                crate::tool::openapi::redact_operation(op, t);
            }
        }
    }
    let source_changed = body
        .spec_url
        .as_deref()
        .map(|u| u.trim() != old_spec_url.trim())
        .unwrap_or(false);
    let token_to_save: Option<&str> = if source_changed && body.token.is_none() {
        Some("")
    } else {
        body.token.as_deref()
    };
    match state.config.save_openapi_expected(
        cfg,
        token_to_save,
        "console",
        Some(&captured_revision),
    ) {
        Ok(st) => Ok(Json(json!(st))),
        Err(e) if e.to_string().contains("OPENAPI_REVISION_CONFLICT") => Err((
            StatusCode::CONFLICT,
            Json(
                json!({"error":"OPENAPI_REVISION_CONFLICT","revision":state.config.openapi_revision().ok()}),
            ),
        )),
        Err(e) if e.to_string().contains("OPENAPI_SOURCE_BINDING_CONFLICT") => Err((
            StatusCode::CONFLICT,
            Json(json!({"error":"OPENAPI_SOURCE_BINDING_CONFLICT"})),
        )),
        Err(e) => Err(bad(e.to_string())),
    }
}

pub(in crate::api) async fn config_openapi_test(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.openapi.probe().await)
}

#[derive(Deserialize)]
pub(in crate::api) struct OpenApiDiscoverBody {
    #[serde(default)]
    spec_url: String,
    /// Write-only; absent = use the stored token.
    #[serde(default)]
    token: Option<String>,
}

/// Discovery for the console: list the document's GET operations so a person
/// can pick a whitelist. It reads the document but saves nothing.
pub(in crate::api) async fn config_openapi_discover(
    State(state): State<Arc<AppState>>,
    Json(body): Json<OpenApiDiscoverBody>,
) -> Json<Value> {
    Json(
        state
            .openapi
            .discover(&body.spec_url, body.token.as_deref())
            .await,
    )
}

// --- weather (R1h): a registered service with the same management shape ----

pub(in crate::api) async fn config_weather_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    match state.config.weather_status() {
        Ok(s) => Json(json!(s)),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

#[derive(Deserialize)]
pub(in crate::api) struct WeatherBody {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    api_key_env: String,
    #[serde(default)]
    places: Vec<crate::config::WeatherPlace>,
    #[serde(default)]
    default_alias: String,
    #[serde(default = "default_weather_days_body")]
    max_days: u8,
    /// Write-only. Absent = keep the stored value; "" = clear it.
    #[serde(default)]
    api_key: Option<String>,
    /// Revision the editor loaded (same stale-form rule as music).
    #[serde(default)]
    revision: Option<String>,
}

fn default_weather_days_body() -> u8 {
    3
}

pub(in crate::api) async fn config_weather_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<WeatherBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cfg = crate::config::WeatherConfig {
        enabled: body.enabled,
        provider: body.provider,
        endpoint: body.endpoint,
        api_key_env: body.api_key_env,
        places: body.places,
        default_alias: body.default_alias,
        max_days: body.max_days,
    };
    match state.config.save_weather_expected(
        cfg,
        body.api_key.as_deref(),
        "console",
        body.revision.as_deref(),
    ) {
        Ok(st) => Ok(Json(json!(st))),
        Err(crate::config::ConfigError::WeatherConflict { revision }) => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "天气配置已被另一个页面修改（本次未写入）；请刷新后重试",
                "revision": revision,
            })),
        )),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )),
    }
}

pub(in crate::api) async fn config_weather_test(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.weather.probe().await)
}

#[derive(Deserialize)]
pub(in crate::api) struct GeocodeQuery {
    #[serde(default)]
    q: String,
}

/// Resolve a city name into coordinates for the console's place editor. This
/// is a management action: tools never geocode (they use stored coordinates),
/// so a voice request cannot fan out into arbitrary lookups.
pub(in crate::api) async fn config_weather_geocode(
    State(state): State<Arc<AppState>>,
    Query(q): Query<GeocodeQuery>,
) -> Json<Value> {
    match state.weather.geocode(&q.q).await {
        Ok(results) => Json(json!({ "results": results, "query": q.q })),
        Err(e) => {
            Json(json!({ "results": [], "query": q.q, "error": e.code(), "detail": e.describe() }))
        }
    }
}
