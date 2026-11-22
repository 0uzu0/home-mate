//! Integration tests for #22 (Open Socket) and #21 (Computer Socket).
//!
//! Proves:
//! - Registering a declarative open socket via the management API makes it
//!   automatically advertised in the prompt/specs and callable by `agent/mod.rs`
//!   without any modification to `agent/mod.rs`'s loop.
//! - Calling an unregistered socket name fails with an explicit error.
//! - Mutating open sockets require a confirmed receipt (`confirmed: true`).
//! - Computer socket (`computer.query` / `computer.action`) calls a live
//!   remote service implementing the Home Mate tool contract, and fails honestly
//!   when the remote service is stopped/unreachable.
//! - `shell_command` remains completely absent from core tools.

use axum::extract::State as AxumState;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use hm_core::api::{router, AppState};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::tool::mocks::MockInject;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

async fn spawn_app(llm: FakeLlm) -> (String, Arc<AppState>) {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    let state = AppState::new(
        store,
        PathBuf::from("./data-test-sockets"),
        MockInject::default(),
        Some(Arc::new(llm)),
    );
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

#[derive(Clone, Default)]
struct MockRemoteState {
    calls: Arc<Mutex<Vec<Value>>>,
}

async fn spawn_mock_remote() -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let st = MockRemoteState::default();
    let calls = st.calls.clone();
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
        )
        .route(
            "/tools",
            get(|| async {
                Json(json!({
                    "tools": [
                        { "name": "clipboard.read", "description": "读取电脑剪贴板", "side_effect": "Query" },
                        { "name": "slides.next", "description": "下一页幻灯片", "side_effect": "Mutate" }
                    ]
                }))
            }),
        )
        .route(
            "/call",
            post(|AxumState(st): AxumState<MockRemoteState>, Json(req): Json<Value>| async move {
                st.calls.lock().unwrap().push(req.clone());
                let tool = req.get("tool").and_then(Value::as_str).unwrap_or("");
                match tool {
                    "clipboard.read" => Json(json!({
                        "status": "completed",
                        "evidence": "observed",
                        "result": { "text": "明天上午十点开会" }
                    })),
                    "slides.next" => Json(json!({
                        "status": "completed",
                        "evidence": "device_confirmed",
                        "result": { "page": 4 },
                        "fact": {
                            "subject": "演示文稿",
                            "action": "switch",
                            "basis": "device_receipt",
                            "confirmed": true,
                            "value": "第4页"
                        }
                    })),
                    _ => Json(json!({
                        "status": "failed",
                        "evidence": "observed",
                        "error": { "code": "UNKNOWN_CAPABILITY", "retryable": false, "detail": "unknown capability" }
                    })),
                }
            }),
        )
        .with_state(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), calls, handle)
}

#[tokio::test]
async fn open_socket_registers_and_executes_without_agent_loop_changes() {
    let (mock_url, _calls, _handle) = spawn_mock_remote().await;
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

#[tokio::test]
async fn computer_socket_calls_remote_service_and_fails_honestly_when_stopped() {
    let (mock_url, calls, handle) = spawn_mock_remote().await;
    let llm = FakeLlm::new(vec![
        // Turn 1: service running -> query + action
        Scripted::Calls {
            calls: vec![
                (
                    "computer.query".into(),
                    r#"{"capability":"clipboard.read"}"#.into(),
                ),
                (
                    "computer.action".into(),
                    r#"{"capability":"slides.next","subject":"演示文稿"}"#.into(),
                ),
            ],
        },
        Scripted::Text {
            chunks: vec!["剪贴板里写着明天上午十点开会。".into()],
        },
        // Turn 2: service stopped -> query must fail honestly
        Scripted::Calls {
            calls: vec![(
                "computer.query".into(),
                r#"{"capability":"clipboard.read"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["远端电脑服务没有启动，读不到剪贴板。".into()],
        },
    ]);
    let (base, state) = spawn_app(llm).await;
    let client = reqwest::Client::new();

    // Configure remote_socket via API
    let put = client
        .put(format!("{base}/api/v0/config/services/remote_socket"))
        .json(&json!({
            "enabled": true,
            "endpoint_url": mock_url,
            "capabilities": [
                { "name": "clipboard.read", "description": "读取剪贴板", "side_effect": "Query" },
                { "name": "slides.next", "description": "下一页幻灯片", "side_effect": "Mutate" }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), reqwest::StatusCode::OK);

    // Probe & discover
    let probe: Value = client
        .post(format!("{base}/api/v0/config/services/remote_socket/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["ok"], json!(true));

    let disc: Value = client
        .post(format!(
            "{base}/api/v0/config/services/remote_socket/discover"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(disc["ok"], json!(true));

    // Turn 1: live remote service
    let resp1 = client
        .post(format!("{base}/api/v0/chat"))
        .json(&json!({ "session_id": "s-comp", "text": "看看电脑剪贴板，然后翻下一页" }))
        .send()
        .await
        .unwrap();
    let events1 = sse_events(resp1).await;
    let q_end = events1
        .iter()
        .find(|e| e["type"] == "tool_result" && e["tool"] == "computer.query")
        .expect("computer.query tool_result");
    assert_eq!(q_end["status"], "completed");

    let trace_id1 = events1.last().and_then(|e| e["trace_id"].as_str()).unwrap();
    let td1: Value = client
        .get(format!("{base}/api/v0/trace/{trace_id1}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let q_trace = td1["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "tool_result" && e["payload"]["tool"] == "computer.query")
        .unwrap();
    assert_eq!(
        q_trace["payload"]["result"]["text"],
        json!("明天上午十点开会")
    );

    let a_end = events1
        .iter()
        .find(|e| e["type"] == "tool_result" && e["tool"] == "computer.action")
        .expect("computer.action tool_result");
    assert_eq!(a_end["status"], "completed");
    assert_eq!(a_end["evidence"], "deviceconfirmed");
    assert!(
        events1
            .iter()
            .any(|e| e["type"] == "fact" && e["text"].as_str().unwrap_or("").contains("演示文稿")),
        "confirmed remote action fact must be emitted"
    );
    assert_eq!(calls.lock().unwrap().len(), 2);

    // Stop the remote service
    handle.abort();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Turn 2: remote service stopped -> must fail honestly, never fabricate success
    let resp2 = client
        .post(format!("{base}/api/v0/chat"))
        .json(&json!({ "session_id": "s-comp", "text": "再看一次剪贴板" }))
        .send()
        .await
        .unwrap();
    let events2 = sse_events(resp2).await;
    let q_fail = events2
        .iter()
        .find(|e| e["type"] == "tool_result" && e["tool"] == "computer.query")
        .expect("computer.query tool_result after stop");
    assert_eq!(q_fail["status"], "failed");
    let trace_id2 = events2.last().and_then(|e| e["trace_id"].as_str()).unwrap();
    let td2: Value = client
        .get(format!("{base}/api/v0/trace/{trace_id2}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let q_fail_trace = td2["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "tool_result" && e["payload"]["tool"] == "computer.query")
        .unwrap();
    assert_eq!(
        q_fail_trace["payload"]["error"]["code"],
        "REMOTE_SOCKET_UNREACHABLE"
    );

    // Verify shell_command is absent from core tools
    assert!(!state.registry.names().iter().any(|n| n == "shell_command"));
}

#[path = "sockets/reviewed.rs"]
mod reviewed;

#[path = "sockets/reviewed_open.rs"]
mod reviewed_open;
