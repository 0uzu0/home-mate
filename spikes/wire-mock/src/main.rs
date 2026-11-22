//! Local SSE wire-mock for the M3.5 kernel spikes.
//!
//! Serves recorded/synthetic SSE transcripts on POST /v1/chat/completions,
//! selected by the `model` field of the incoming request (so any client —
//! hm-core's own, genai, rig — can drive the same fixtures without custom
//! headers), and records connect/close timestamps per connection so a test
//! can prove that dropping a client stream closes the connection promptly.
//!
//! Synthetic behaviors (model names):
//!   mock-stall-headers  — respond with headers, then send nothing (header timeout)
//!   mock-stall-body     — one event, then hang (read/idle timeout)
//!   mock-heartbeat      — SSE comments every 300ms for ~1.8s, then a normal completion
//!   mock-truncated      — a partial JSON event, then hard close
//!   mock-<name>         — stream fixtures/<name>.sse verbatim (with per-line delay)

use axum::{
    extract::State, http::{header, StatusCode}, response::IntoResponse, routing::{get, post}, Router,
};
use futures::stream::StreamExt;
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;

#[derive(Clone)]
struct AppState {
    fixtures_dir: PathBuf,
    line_delay: Duration,
    events: Arc<Mutex<Vec<ConnEvent>>>,
}

#[derive(Clone, serde::Serialize)]
struct ConnEvent {
    model: String,
    connected_ms: u128,
    closed_ms: Option<u128>,
    close_reason: Option<&'static str>,
}

/// Guard that records the close timestamp when the response body stream is
/// dropped (axum drops it when the client disconnects).
struct CloseGuard {
    events: Arc<Mutex<Vec<ConnEvent>>>,
    index: usize,
}

impl Drop for CloseGuard {
    fn drop(&mut self) {
        let mut evs = self.events.lock().unwrap();
        if let Some(ev) = evs.get_mut(self.index) {
            ev.closed_ms = Some(now_ms());
            ev.close_reason = Some("body-dropped");
        }
    }
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

#[tokio::main]
async fn main() {
    let dir = std::env::var("HM_SPIKE_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("../fixtures"));
    let line_delay = Duration::from_millis(
        std::env::var("HM_SPIKE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(5),
    );
    let bind = std::env::var("HM_SPIKE_BIND").unwrap_or_else(|_| "127.0.0.1:8977".into());

    let state = AppState {
        fixtures_dir: dir,
        line_delay,
        events: Arc::new(Mutex::new(Vec::new())),
    };

    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        // Alias: genai's custom-endpoint Url::join drops the last path segment
        // of a base without a trailing slash, so it posts /chat/completions.
        .route("/chat/completions", post(chat_completions))
        .route("/__events", get(list_events))
        .route("/__reset", post(reset))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    println!("wire-mock listening on http://{bind}");
    axum::serve(listener, app).await.expect("serve");
}

async fn chat_completions(
    State(state): State<AppState>,
    body: String,
) -> impl IntoResponse {
    let model = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".into());

    let index = {
        let mut evs = state.events.lock().unwrap();
        evs.push(ConnEvent {
            model: model.clone(),
            connected_ms: now_ms(),
            closed_ms: None,
            close_reason: None,
        });
        evs.len() - 1
    };
    let guard = CloseGuard { events: state.events.clone(), index };

    println!("[wire-mock] request model={model}");

    let body_stream = build_body(state.clone(), model.clone(), guard);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream"), (header::CACHE_CONTROL, "no-cache")],
        axum::body::Body::from_stream(body_stream),
    )
}

fn build_body(
    state: AppState,
    model: String,
    guard: CloseGuard,
) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send + 'static {
    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>(16);
    tokio::spawn(async move {
        let _guard = guard; // moved into the task; dropped when the task ends or is aborted
        match model.as_str() {
            "mock-stall-headers" => {
                // headers sent by the framework already; body never arrives
                tokio::time::sleep(Duration::from_secs(20)).await;
            }
            "mock-stall-body" => {
                let _ = tx.send(Ok(b"data: {\"id\":\"x\",\"choices\":[{\"delta\":{},\"index\":0}]}\n\n".to_vec())).await;
                tokio::time::sleep(Duration::from_secs(20)).await;
            }
            "mock-heartbeat" => {
                for _ in 0..6 {
                    let _ = tx.send(Ok(b": ping\n\n".to_vec())).await;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                let done = normal_completion_sse("heartbeat-done");
                let _ = tx.send(Ok(done)).await;
            }
            "mock-truncated" => {
                let _ = tx.send(Ok(b"data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"hel".to_vec())).await;
                let _ = tx.send(Ok(b"lo\"},\"index\":0}]}".to_vec())).await;
                // hard close without [DONE]
            }
            name => {
                let path = state.fixtures_dir.join(format!("{name}.sse"));
                match tokio::fs::read_to_string(&path).await {
                    Ok(content) => {
                        for line in content.split('\n') {
                            if line.is_empty() {
                                continue;
                            }
                            let mut chunk = line.to_owned();
                            chunk.push('\n');
                            if line.starts_with("data:") || line.starts_with(':') {
                                chunk.push('\n'); // SSE events are blank-line separated
                            }
                            if tx.send(Ok(chunk.into_bytes())).await.is_err() {
                                break; // client gone
                            }
                            tokio::time::sleep(state.line_delay).await;
                        }
                    }
                    Err(e) => {
                        let err = format!("wire-mock: fixture for model {name} not readable: {e}");
                        eprintln!("{err}");
                        let _ = tx.send(Err(std::io::Error::other(err))).await;
                    }
                }
            }
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

fn normal_completion_sse(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let evt = serde_json::json!({
        "id": "mock", "object": "chat.completion.chunk", "created": 0,
        "model": "mock",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}]
    });
    out.extend_from_slice(format!("data: {evt}\n\n").as_bytes());
    let done = serde_json::json!({
        "id": "mock", "object": "chat.completion.chunk", "created": 0,
        "model": "mock",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    out.extend_from_slice(format!("data: {done}\n\n").as_bytes());
    out.extend_from_slice(b"data: [DONE]\n\n");
    out
}

async fn list_events(State(state): State<AppState>) -> impl IntoResponse {
    let evs = state.events.lock().unwrap().clone();
    axum::Json(evs)
}

async fn reset(State(state): State<AppState>) -> impl IntoResponse {
    state.events.lock().unwrap().clear();
    StatusCode::OK
}
