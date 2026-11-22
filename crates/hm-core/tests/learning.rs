use axum::{
    http::HeaderMap,
    routing::{get, post},
    Json, Router,
};
use hm_core::api::AppState;
use hm_core::tool::learning::{
    LearningAnswerTool, LearningControlTool, LearningCurrentTool, LearningDecksTool,
    LearningRevealTool, LearningStartTool, LEARNING_WAIT_TTL_MS, LEARNING_WAIT_WINDOW_MS,
};
use hm_core::tool::mocks::MockInject;
use hm_core::tool::{Availability, Tool, ToolCtx, ToolStatus};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[tokio::test]
async fn learning_http_api_and_current_binding_tool_end_to_end() {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    let state = AppState::with_overrides(
        Arc::clone(&store),
        std::path::PathBuf::from("./data-test"),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let api = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let base = format!("http://{api_addr}");
    let client = reqwest::Client::new();

    let cur_tool = LearningCurrentTool::new(state.learning.clone());
    assert_eq!(cur_tool.availability(), Availability::Unconfigured);

    store.allow_loopback_public_for_test();

    // Mock NAS learning-svc matching authority.py:141-153 schema + Bearer check
    let l_app = Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true })) }))
        .route("/ready", get(|| async { Json(json!({ "ready": true })) }))
        .route(
            "/api/v1/learning/device/bindings/bind-1/current",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer dev-tok");
                Json(json!({
                    "attempt_id": "A1",
                    "dimension": "en_to_meaning",
                    "generation": 1,
                    "revision": 3,
                    "expires_at": 1795014000,
                    "prompt": "apple",
                    "binding_id": "bind-1",
                    "dataset_epoch": "E1",
                    "server_time": 1795013500
                }))
            }),
        );
    let l_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let l_addr = l_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l_listener, l_app).await.unwrap() });
    let l_url = format!("http://{l_addr}");

    let put_res: Value = client
        .put(format!("{base}/api/v0/config/services/learning"))
        .json(&json!({ "enabled": true, "url": l_url, "bearer_token": "dev-tok" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put_res["state"], "ready");
    assert_eq!(put_res["bearer_token_set"], true);

    let test_res: Value = client
        .post(format!("{base}/api/v0/config/services/learning/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(test_res["ok"], true);
    assert_eq!(cur_tool.availability(), Availability::Ready);

    let ctx = ToolCtx::new("T-learn", "C-learn", Arc::new(AtomicBool::new(false)));
    let cur = cur_tool
        .execute(json!({ "binding_id": "bind-1" }), &ctx)
        .await;
    assert_eq!(cur.status, ToolStatus::Completed);
    let body = cur.result.unwrap();
    assert_eq!(body["attempt_id"], "A1");
    assert_eq!(body["prompt"], "apple");
    assert_eq!(body["binding_id"], "bind-1");

    // Path traversal blocked
    let bad = cur_tool
        .execute(json!({ "binding_id": "../admin" }), &ctx)
        .await;
    assert_eq!(bad.status, ToolStatus::Failed);
    assert_eq!(bad.error.unwrap().code, "INVALID_ARGS");
}

#[tokio::test]
async fn speaker_first_learning_tools_and_learn10_wait_state() {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let state = AppState::with_overrides(
        Arc::clone(&store),
        std::path::PathBuf::from("./data-test"),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );

    let l_app = Router::new()
        .route("/api/v1/learning/device/decks", get(|| async {
            Json(json!({ "learners": [{ "id": "L1", "name": "五年级英语" }], "decks": [{ "id": "D1", "name": "五上" }] }))
        }))
        .route("/api/v1/learning/device/sessions/start", post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            let key = headers.get("idempotency-key").unwrap().to_str().unwrap();
            assert_eq!(key.len(), 36, "UUID v4 Idempotency-Key expected");
            assert_eq!(body["limit"], 10);
            Json(json!({
                "status": "active",
                "session_id": "S1",
                "attempt": { "attempt_id": "A1", "prompt": "去慢跑" }
            }))
        }))
        .route("/api/v1/learning/device/current", get(|| async {
            Json(json!({
                "status": "active",
                "session_id": "S1",
                "attempt": { "attempt_id": "A1", "prompt": "去慢跑" }
            }))
        }))
        .route("/api/v1/learning/device/current/reveal", post(|headers: HeaderMap| async move {
            assert_eq!(headers.get("idempotency-key").unwrap().to_str().unwrap().len(), 36);
            Json(json!({
                "status": "active",
                "session_id": "S1",
                "attempt": { "attempt_id": "A1", "answer": "go jogging" }
            }))
        }))
        .route("/api/v1/learning/device/current/answer", post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers.get("idempotency-key").unwrap().to_str().unwrap().len(), 36);
            assert_eq!(body["answer"], "go jogging");
            Json(json!({
                "status": "active",
                "session_id": "S1",
                "evaluated": { "correct": true, "rating_applied": "good" },
                "attempt": { "attempt_id": "A2", "prompt": "忙碌的" }
            }))
        }))
        .route("/api/v1/learning/device/current/control", post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers.get("idempotency-key").unwrap().to_str().unwrap().len(), 36);
            let act = body["action"].as_str().unwrap();
            let status = match act {
                "pause" => "paused",
                "resume" => "active",
                _ => "ended",
            };
            Json(json!({ "status": status, "session_id": "S1" }))
        }));

    let l_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let l_addr = l_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l_listener, l_app).await.unwrap() });

    let api = hm_core::api::router(state.clone());
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(api_listener, api).await.unwrap() });
    reqwest::Client::new()
        .put(format!("http://{api_addr}/api/v0/config/services/learning"))
        .json(&json!({ "enabled": true, "url": format!("http://{l_addr}"), "bearer_token": "dev-tok" }))
        .send()
        .await
        .unwrap();

    let ctx = ToolCtx::new("T-spk", "C-spk", Arc::new(AtomicBool::new(false)));
    let decks = LearningDecksTool::new(state.learning.clone())
        .execute(json!({}), &ctx)
        .await;
    assert_eq!(decks.status, ToolStatus::Completed);

    // Start activates LEARN-10 bounded wait state
    let start = LearningStartTool::new(state.learning.clone())
        .execute(json!({ "limit": 8 }), &ctx)
        .await;
    assert_eq!(start.status, ToolStatus::Completed);
    let now = hm_core::id::now_ms();
    let lease = state
        .learning
        .active_wait(now)
        .expect("LEARN-10 wait lease active");
    assert_eq!(lease.window_ms, LEARNING_WAIT_WINDOW_MS);
    assert_eq!(lease.session_id.as_deref(), Some("S1"));
    assert_eq!(lease.attempt_id.as_deref(), Some("A1"));
    // Hard TTL ceiling prevents infinite open mic
    assert!(state
        .learning
        .active_wait(now + LEARNING_WAIT_TTL_MS + 1)
        .is_none());

    // Invalid dimension / mode / oversized answer rejected locally
    let bad_dim = LearningStartTool::new(state.learning.clone())
        .execute(json!({ "dimension": "invalid" }), &ctx)
        .await;
    assert_eq!(bad_dim.status, ToolStatus::Failed);
    let long_ans = "a".repeat(201);
    let bad_ans = LearningAnswerTool::new(state.learning.clone())
        .execute(json!({ "answer": long_ans }), &ctx)
        .await;
    assert_eq!(bad_ans.status, ToolStatus::Failed);

    // Speaker-first current without binding_id re-activates wait state
    let cur = LearningCurrentTool::new(state.learning.clone())
        .execute(json!({}), &ctx)
        .await;
    assert_eq!(cur.status, ToolStatus::Completed);
    assert!(state.learning.active_wait(hm_core::id::now_ms()).is_some());

    // Reveal & answer keep session active
    let rev = LearningRevealTool::new(state.learning.clone())
        .execute(json!({}), &ctx)
        .await;
    assert_eq!(rev.status, ToolStatus::Completed);
    let ans = LearningAnswerTool::new(state.learning.clone())
        .execute(json!({ "answer": "go jogging" }), &ctx)
        .await;
    assert_eq!(ans.status, ToolStatus::Completed);
    assert_eq!(ans.result.unwrap()["evaluated"]["correct"], true);

    // Pause clears LEARN-10 wait state immediately
    let pause = LearningControlTool::new(state.learning.clone())
        .execute(json!({ "action": "pause" }), &ctx)
        .await;
    assert_eq!(pause.status, ToolStatus::Completed);
    assert!(state.learning.active_wait(hm_core::id::now_ms()).is_none());
}
