//! Integration tests for #22 (Open Socket).
//!
//! Proves:
//! - Registering a declarative open socket via the management API makes it
//!   automatically advertised in the prompt/specs and callable by `agent/mod.rs`
//!   without any modification to `agent/mod.rs`'s loop.
//! - Calling an unregistered socket name fails with an explicit error.
//! - Mutating open sockets require a confirmed receipt (`confirmed: true`).
//! - `shell_command` remains completely absent from core tools.

use axum::extract::State as AxumState;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use hm_core::api::{router, AppState};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::tool::mocks::MockInject;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

async fn spawn_app(llm: FakeLlm) -> (String, Arc<AppState>) {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    let tmp_dir = std::env::temp_dir().join(format!(
        "hm-core-test-sockets-{}-{}",
        std::process::id(),
        hm_core::id::now_ms()
    ));
    let state = AppState::new(store, tmp_dir, MockInject::default(), Some(Arc::new(llm)));
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

async fn sse_events(resp: reqwest::Response) -> Vec<Value> {
    let mut bytes = resp.bytes_stream();
    let mut buf = String::new();
    let mut events = Vec::new();
    while let Some(chunk) = bytes.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        while let Some(nl) = buf.find('\n') {
            let line: String = buf.drain(..=nl).collect();
            let line = line.trim();
            if let Some(data) = line.strip_prefix("data:") {
                if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                    events.push(v);
                }
            }
        }
    }
    events
}

async fn spawn_mock_socket_server() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route(
            "/desk/temp",
            get(|| async { Json(json!({ "celsius": 23.5, "room": "书房" })) }),
        )
        .route(
            "/desk/lamp",
            post(|Json(body): Json<Value>| async move {
                let state = body.get("state").and_then(Value::as_str).unwrap_or("on");
                Json(json!({
                    "confirmed": true,
                    "subject": "书桌台灯",
                    "action": state,
                }))
            }),
        )
        .route(
            "/desk/unconfirmed",
            post(|| async { Json(json!({ "ok": true })) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn open_socket_registers_and_executes_without_agent_loop_changes() {
    let (mock_url, _handle) = spawn_mock_socket_server().await;
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![
                ("desk.temp".into(), "{}".into()),
                ("desk.lamp".into(), r#"{"state":"on"}"#.into()),
                ("desk.unconfirmed".into(), "{}".into()),
                ("nonexistent.socket".into(), "{}".into()),
            ],
        },
        Scripted::Text {
            chunks: vec!["书房温度 23.5 度。".into()],
        },
    ]);
    let (base, state) = spawn_app(llm).await;
    let client = reqwest::Client::new();

    // shell_command must never exist in core tools.
    assert!(!state.registry.names().iter().any(|n| n == "shell_command"));

    // Registering shell_command as an open socket must be refused.
    let bad = client
        .post(format!("{base}/api/v0/config/sockets"))
        .json(&json!({
            "name": "shell_command",
            "description": "forbidden",
            "effect": "Mutate",
            "endpoint_url": format!("{mock_url}/desk/lamp"),
            "method": "POST"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);

    // Register query socket `desk.temp`
    let r1 = client
        .post(format!("{base}/api/v0/config/sockets"))
        .json(&json!({
            "name": "desk.temp",
            "service": "desk",
            "description": "查询书房温度",
            "effect": "Query",
            "endpoint_url": format!("{mock_url}/desk/temp"),
            "method": "GET"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), reqwest::StatusCode::OK);

    // Register mutating socket `desk.lamp`
    let r2 = client
        .post(format!("{base}/api/v0/config/sockets"))
        .json(&json!({
            "name": "desk.lamp",
            "service": "desk",
            "description": "开关书桌台灯",
            "effect": "Mutate",
            "endpoint_url": format!("{mock_url}/desk/lamp"),
            "method": "POST",
            "parameters": {
                "type": "object",
                "properties": { "state": { "type": "string" } },
                "required": ["state"]
            },
            "default_subject": "书桌台灯",
            "default_action": "on"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), reqwest::StatusCode::OK);

    // Register mutating socket without receipt `desk.unconfirmed`
    let r3 = client
        .post(format!("{base}/api/v0/config/sockets"))
        .json(&json!({
            "name": "desk.unconfirmed",
            "service": "desk",
            "description": "无回执测试插座",
            "effect": "Mutate",
            "endpoint_url": format!("{mock_url}/desk/unconfirmed"),
            "method": "POST",
            "default_subject": "测试设备",
            "default_action": "on"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r3.status(), reqwest::StatusCode::OK);

    // Run a chat turn through agent/mod.rs without modifying agent/mod.rs.
    let chat_resp = client
        .post(format!("{base}/api/v0/chat"))
        .json(&json!({ "session_id": "s-open-socket", "text": "书房几度，顺便开台灯" }))
        .send()
        .await
        .unwrap();
    assert_eq!(chat_resp.status(), reqwest::StatusCode::OK);
    let events = sse_events(chat_resp).await;

    let trace_id = events
        .last()
        .and_then(|e| e["trace_id"].as_str())
        .expect("turn_end trace_id");
    let trace_detail: Value = client
        .get(format!("{base}/api/v0/trace/{trace_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let trace_events = trace_detail["events"].as_array().expect("trace events");
    let find_trace_result = |tool_name: &str| -> &Value {
        trace_events
            .iter()
            .find(|e| e["kind"] == "tool_result" && e["payload"]["tool"] == tool_name)
            .expect(tool_name)
    };

    // 1) desk.temp returned real 23.5
    let temp_sse = events
        .iter()
        .find(|e| e["type"] == "tool_result" && e["tool"] == "desk.temp")
        .expect("desk.temp tool_result");
    assert_eq!(temp_sse["status"], "completed");
    assert_eq!(
        find_trace_result("desk.temp")["payload"]["result"]["celsius"],
        json!(23.5)
    );

    // 2) desk.lamp returned confirmed ToolFact
    let lamp_sse = events
        .iter()
        .find(|e| e["type"] == "tool_result" && e["tool"] == "desk.lamp")
        .expect("desk.lamp tool_result");
    assert_eq!(lamp_sse["status"], "completed");
    assert_eq!(lamp_sse["evidence"], "deviceconfirmed");
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "fact"
                && e["text"].as_str().unwrap_or("").contains("书桌台灯已打开")),
        "confirmed fact must be emitted: {events:?}"
    );

    // 3) desk.unconfirmed did NOT fabricate confirmation
    let unconf_sse = events
        .iter()
        .find(|e| e["type"] == "tool_result" && e["tool"] == "desk.unconfirmed")
        .expect("desk.unconfirmed tool_result");
    assert_eq!(unconf_sse["status"], "unknown");
    assert!(
        !events
            .iter()
            .any(|e| e["type"] == "fact" && e["text"].as_str().unwrap_or("").contains("测试设备已")),
        "unconfirmed socket must never emit 已... fact"
    );

    // 4) nonexistent.socket failed with explicit UNKNOWN_TOOL error
    let missing_sse = events
        .iter()
        .find(|e| e["type"] == "tool_result" && e["tool"] == "nonexistent.socket")
        .expect("nonexistent.socket tool_result");
    assert_eq!(missing_sse["status"], "failed");
    assert_eq!(
        find_trace_result("nonexistent.socket")["payload"]["error"]["code"],
        "UNKNOWN_TOOL"
    );
}

#[path = "sockets/reviewed_open.rs"]
mod reviewed_open;
