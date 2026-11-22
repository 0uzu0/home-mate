use crate::api::AppState;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
type Response = Result<Json<Value>, (StatusCode, Json<Value>)>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transition {
    expected_state: String,
    expected_revision: u64,
}
#[derive(Default, Deserialize)]
pub struct Filter {
    state: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/confirmations", get(list))
        .route("/api/v1/confirmations/{id}", get(read))
        .route("/api/v1/confirmations/{id}/approve", post(approve))
        .route("/api/v1/confirmations/{id}/reject", post(reject))
        .route("/api/v1/confirmations/{id}/cancel", post(cancel))
        .route("/api/v1/confirmations/{id}/execute", post(execute))
}
fn principal(state: &AppState, headers: &HeaderMap) -> Result<String, (StatusCode, Json<Value>)> {
    state
        .execution
        .principal(crate::execution::api::token(headers))
        .map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"PAIRING_REQUIRED"})),
            )
        })
}
fn result(
    state: &AppState,
    id: &str,
    principal: &str,
    value: crate::store::Result<Value>,
) -> Response {
    value.map(Json).map_err(|error| {
        let text = error.to_string();
        let code = [
            "CONFIRMATION_NOT_FOUND",
            "CONFIRMATION_EXPIRED",
            "CONFIRMATION_STALE",
            "CONFIRMATION_CANCELLED",
            "CONFIRMATION_AUTHORITY_INVALID",
            "CONFIRMATION_UNAVAILABLE",
            "CONFIRMATION_CONFLICT",
        ]
        .into_iter()
        .find(|code| text.contains(code))
        .unwrap_or("CONFIRMATION_STORE_ERROR");
        let status = if code == "CONFIRMATION_NOT_FOUND" {
            StatusCode::NOT_FOUND
        } else if code == "CONFIRMATION_STORE_ERROR" || code == "CONFIRMATION_UNAVAILABLE" {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::CONFLICT
        };
        let mut body = json!({"error":code});
        if status == StatusCode::CONFLICT {
            if let Ok(action) = state.confirmations.read(id, principal) {
                body["action"] = action;
            }
        }
        (status, Json(body))
    })
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(filter): Query<Filter>,
) -> Response {
    let principal = principal(&state, &headers)?;
    let states = filter
        .state
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    result(
        &state,
        "",
        &principal,
        state.confirmations.list(
            &principal,
            &states,
            filter.limit.unwrap_or(32),
            filter.cursor.as_deref(),
        ),
    )
}
async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let principal = principal(&state, &headers)?;
    result(
        &state,
        &id,
        &principal,
        state.confirmations.read(&id, &principal),
    )
}
macro_rules! transition {
    ($name:ident) => {
        async fn $name(
            State(state): State<Arc<AppState>>,
            headers: HeaderMap,
            Path(id): Path<String>,
            Json(body): Json<Transition>,
        ) -> Response {
            let principal = principal(&state, &headers)?;
            result(
                &state,
                &id,
                &principal,
                state.confirmations.change(
                    &id,
                    &principal,
                    &body.expected_state,
                    body.expected_revision,
                    stringify!($name),
                ),
            )
        }
    };
}
transition!(approve);
transition!(reject);
transition!(cancel);
async fn execute(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Transition>,
) -> Response {
    let principal = principal(&state, &headers)?;
    result(
        &state,
        &id,
        &principal,
        state
            .confirmations
            .execute(
                &id,
                &principal,
                &body.expected_state,
                body.expected_revision,
            )
            .await,
    )
}
