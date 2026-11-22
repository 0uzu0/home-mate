use super::*;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Deserialize)]
pub(in crate::api) struct AgentMdPutBody {
    pub content: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub author: String,
}

#[derive(Deserialize)]
pub(in crate::api) struct AgentMdRollbackBody {
    #[serde(alias = "revision_id", alias = "version")]
    pub id: i64,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub author: String,
}

#[derive(Deserialize)]
pub(in crate::api) struct UserMdQuery {
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Deserialize)]
pub(in crate::api) struct UserMdPutBody {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub person_id: String,
    pub key: String,
    pub content: String,
    #[serde(default)]
    pub rationale: String,
}

pub(in crate::api) async fn agent_md_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.agent_md.snapshot() {
        Ok(snap) => Ok(Json(
            serde_json::to_value(snap).unwrap_or_else(|_| json!({})),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )),
    }
}

pub(in crate::api) async fn agent_md_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AgentMdPutBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let author = if body.author.trim().is_empty() {
        "console"
    } else {
        body.author.trim()
    };
    match state.agent_md.update(&body.content, author, &body.note) {
        Ok(snap) => Ok(Json(json!({
            "state": "applied",
            "content": snap.content,
            "version": snap.version,
            "updated_ms": snap.updated_ms,
            "revisions": snap.revisions,
        }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

pub(in crate::api) async fn agent_md_rollback(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AgentMdRollbackBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let author = if body.author.trim().is_empty() {
        "console"
    } else {
        body.author.trim()
    };
    match state
        .agent_md
        .rollback(body.id, author, body.note.as_deref())
    {
        Ok(snap) => Ok(Json(json!({
            "state": "rolled_back",
            "restored_from": body.id,
            "content": snap.content,
            "version": snap.version,
            "updated_ms": snap.updated_ms,
            "revisions": snap.revisions,
        }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

pub(in crate::api) async fn user_md_get(
    State(state): State<Arc<AppState>>,
    Query(q): Query<UserMdQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.user_md.list(q.status.as_deref()) {
        Ok(entries) => Ok(Json(json!({ "entries": entries }))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )),
    }
}

pub(in crate::api) async fn user_md_put(
    State(state): State<Arc<AppState>>,
    Json(body): Json<UserMdPutBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.user_md.upsert_console(
        body.id.as_deref(),
        &body.person_id,
        &body.key,
        &body.content,
        &body.rationale,
    ) {
        Ok(entry) => Ok(Json(json!({ "state": "applied", "entry": entry }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

pub(in crate::api) async fn user_md_revert(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.user_md.revert(&id, "console") {
        Ok(entry) => Ok(Json(json!({ "state": "reverted", "entry": entry }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}
