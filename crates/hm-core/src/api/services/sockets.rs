use super::reset_device_history_if_capabilities_changed;
use super::service_fingerprint;
use crate::api::*;
use crate::tool::open_socket::{HttpMethod, OpenSocketContract};
use crate::tool::remote_socket::{RemoteCapabilitySpec, RemoteSocketConfig};
use crate::tool::SideEffect;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

// --- #22 Open Sockets (Declarative Socket Registry) -------------------------

pub(in crate::api) async fn config_sockets_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({ "sockets": state.open_sockets.list() }))
}

#[derive(Deserialize)]
pub(in crate::api) struct OpenSocketBody {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: Option<Value>,
    #[serde(default)]
    pub effect: Option<SideEffect>,
    #[serde(default)]
    pub endpoint_url: String,
    #[serde(default)]
    pub method: Option<HttpMethod>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub auth_header: Option<String>,
    #[serde(default)]
    pub default_subject: Option<String>,
    #[serde(default)]
    pub default_action: Option<String>,
    /// Write-only: absent keeps stored token, "" clears, non-empty replaces.
    #[serde(default)]
    pub token: Option<String>,
}

fn upsert_open_socket(
    state: &AppState,
    name_override: Option<String>,
    body: OpenSocketBody,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let tools_before = service_fingerprint(state);
    let name = name_override.unwrap_or(body.name);
    let status = state
        .open_sockets
        .edit(
            &state.registry,
            &name,
            body.token.as_deref(),
            "console",
            |existing| OpenSocketContract {
                name: name.clone(),
                service: if body.service.trim().is_empty() {
                    existing
                        .as_ref()
                        .map(|e| e.service.clone())
                        .unwrap_or_else(|| "open_socket".into())
                } else {
                    body.service
                },
                description: if body.description.trim().is_empty() {
                    existing
                        .as_ref()
                        .map(|e| e.description.clone())
                        .unwrap_or_default()
                } else {
                    body.description
                },
                parameters: body.parameters.unwrap_or_else(|| {
                    existing
                        .as_ref()
                        .map(|e| e.parameters.clone())
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }))
                }),
                effect: body
                    .effect
                    .or_else(|| existing.as_ref().map(|e| e.effect))
                    .unwrap_or(SideEffect::Query),
                endpoint_url: if body.endpoint_url.trim().is_empty() {
                    existing
                        .as_ref()
                        .map(|e| e.endpoint_url.clone())
                        .unwrap_or_default()
                } else {
                    body.endpoint_url
                },
                method: body
                    .method
                    .or_else(|| existing.as_ref().map(|e| e.method))
                    .unwrap_or(HttpMethod::GET),
                enabled: body
                    .enabled
                    .or_else(|| existing.as_ref().map(|e| e.enabled))
                    .unwrap_or(true),
                timeout_ms: body
                    .timeout_ms
                    .or_else(|| existing.as_ref().map(|e| e.timeout_ms))
                    .unwrap_or(10_000),
                auth_header: body.auth_header.unwrap_or_else(|| {
                    existing
                        .as_ref()
                        .map(|e| e.auth_header.clone())
                        .unwrap_or_default()
                }),
                default_subject: body.default_subject.unwrap_or_else(|| {
                    existing
                        .as_ref()
                        .map(|e| e.default_subject.clone())
                        .unwrap_or_default()
                }),
                default_action: body.default_action.unwrap_or_else(|| {
                    existing
                        .as_ref()
                        .map(|e| e.default_action.clone())
                        .unwrap_or_else(|| "other".into())
                }),
            },
        )
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))))?;
    reset_device_history_if_capabilities_changed(state, tools_before);
    Ok(Json(json!(status)))
}

pub(in crate::api) async fn config_sockets_post(
    State(state): State<Arc<AppState>>,
    Json(body): Json<OpenSocketBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    upsert_open_socket(&state, None, body)
}

pub(in crate::api) async fn config_sockets_put(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<OpenSocketBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    upsert_open_socket(&state, Some(name), body)
}

pub(in crate::api) async fn config_sockets_delete(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    remove_open_socket(&state, &name)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct RemoveSocketBody {
    name: String,
}

/// Body identity also reaches malformed persisted names, including empty or
/// slash-containing values that are not reliably representable as a route.
pub(in crate::api) async fn config_sockets_remove(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RemoveSocketBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    remove_open_socket(&state, &body.name)
}

fn remove_open_socket(
    state: &AppState,
    name: &str,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let tools_before = service_fingerprint(state);
    let removed = state
        .open_sockets
        .remove(&state.registry, name, "console")
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
        })?;
    if !removed {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("open socket '{name}' not found") })),
        ));
    }
    reset_device_history_if_capabilities_changed(state, tools_before);
    Ok(Json(json!({ "removed": true, "name": name })))
}

// --- #21 Remote / Computer Socket -------------------------------------------

pub(in crate::api) async fn config_remote_socket_get(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    Json(state.remote_socket.status_json())
}

#[derive(Deserialize)]
pub(in crate::api) struct RemoteSocketBody {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub endpoint_url: Option<String>,
    #[serde(default)]
    pub service_name: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub capabilities: Option<Vec<RemoteCapabilitySpec>>,
    /// Write-only: absent keeps, "" clears.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
}

pub(in crate::api) async fn config_remote_socket_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RemoteSocketBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let fail = |error: crate::config_delivery::DeliveryError| {
        (
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({"error": error.detail, "code": error.code, "pointer": error.pointer})),
        )
    };
    let baseline = state.remote_socket.snapshot().map_err(fail)?;
    let tools_before = service_fingerprint(&state);
    let mut cfg: RemoteSocketConfig = baseline.config;
    if let Some(v) = body.enabled {
        cfg.enabled = v;
    }
    if let Some(url) = body.endpoint_url {
        cfg.endpoint_url = url;
    }
    if let Some(name) = body.service_name {
        cfg.service_name = name;
    }
    if let Some(ms) = body.timeout_ms {
        cfg.timeout_ms = ms;
    }
    if let Some(caps) = body.capabilities {
        cfg.capabilities = caps;
    }
    let res = state
        .remote_socket
        .save_expected(
            cfg,
            body.token.as_deref(),
            "console",
            Some(body.revision.as_deref().unwrap_or(&baseline.revision)),
        )
        .map_err(fail)?;
    reset_device_history_if_capabilities_changed(&state, tools_before);
    Ok(Json(res))
}

pub(in crate::api) async fn config_remote_socket_test(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    Json(state.remote_socket.probe().await)
}

pub(in crate::api) async fn config_remote_socket_discover(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    Json(state.remote_socket.discover().await)
}
