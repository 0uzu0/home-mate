use crate::api::AppState;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
type Response = Result<Json<Value>, (StatusCode, Json<Value>)>;
pub(crate) fn token(headers: &HeaderMap) -> &str {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim()
}
pub(crate) fn error(detail: String) -> (StatusCode, Json<Value>) {
    let detail = if detail.contains("SESSION_SCOPE_UNVERIFIED") {
        "SESSION_SCOPE_UNVERIFIED".into()
    } else {
        detail
    };
    let status = if detail == "PAIRING_REQUIRED" {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::CONFLICT
    };
    (status, Json(json!({"error":detail})))
}
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/chat/scopes", post(issue))
        .route("/api/v1/chat/scopes/{id}", get(read).delete(revoke))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    person_id: Option<String>,
    device_id: Option<String>,
    #[serde(default)]
    household_write: bool,
}
async fn issue(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Selection>,
) -> Response {
    let principal = state.execution.principal(token(&headers)).map_err(error)?;
    state
        .execution
        .issue(
            &principal,
            body.person_id,
            body.device_id,
            body.household_write,
        )
        .map(Json)
        .map_err(error)
}
async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let principal = state.execution.principal(token(&headers)).map_err(error)?;
    state
        .execution
        .read(&id, &principal)
        .map(Json)
        .map_err(error)
}
async fn revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let principal = state.execution.principal(token(&headers)).map_err(error)?;
    state.execution.revoke(&id, &principal).map_err(error)?;
    Ok(Json(json!({"binding_id":id,"state":"revoked"})))
}
