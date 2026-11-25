//! End-to-end integration tests for #267 (Two-Tier Master Brain Async Architecture):
//! - Slice 1: Controlled downward proactive voice announcements (`POST/GET /api/v0/voice/announce`)
//!   * Household Bearer auth enforcement
//!   * Input validation & offline/unknown device honesty
//!   * Unsolicited `proactive_speech=false` rejection vs `task_id` callback allowance
//!   * Immediate spoken delivery when idle + `voice_announce_spoken` event
//!   * Non-interrupting deferral while busy + drain once idle
//!   * `open_followup: true` bounded listening window + `voice_announce_followup_opened`
//!   * Bounded queue cap (8) + high-priority eviction of oldest normal entry
//! - Slice 2: Provider-agnostic Master Brain delegation (`brain.*` + config API)
//!   * `GET/PUT /api/v0/config/services/master_brain` + `/test`
//!   * Vault write-only Bearer token redaction
//!   * Live hot-reload of tool availability without restart
//!   * Full async round-trip: `brain.delegate` -> mock Master Brain receives `{task, device, room, callback_url}`
//!     -> immediate speaker ack -> mock Master Brain posts `POST /api/v0/voice/announce` -> speaker announces result!

mod common;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use hm_core::api::AppState;
use hm_core::speech::{SpeechError, TtsAudio, TtsClient};
use hm_core::tool::mocks::MockInject;
use hm_core::tool::{Availability, ToolCtx, ToolStatus};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct RecordingTts {
    spoken: Arc<Mutex<Vec<String>>>,
}

impl RecordingTts {
    fn new() -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        let spoken = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                spoken: spoken.clone(),
            }),
            spoken,
        )
    }
}

#[async_trait::async_trait]
impl TtsClient for RecordingTts {
    fn provider(&self) -> &str {
        "recording-mock"
    }

    async fn synthesize(&self, text: &str) -> Result<TtsAudio, SpeechError> {
        self.spoken.lock().unwrap().push(text.to_string());
        let rate = 16_000u32;
        let samples = vec![0i16; 1600]; // 100 ms silence WAV
        let data_len = (samples.len() * 2) as u32;
        let mut wav = Vec::with_capacity(44 + samples.len() * 2);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            wav.extend_from_slice(&s.to_le_bytes());
        }
        Ok(TtsAudio {
            wav,
            sample_rate: rate,
            channels: 1,
            duration_ms: 100,
            provider: "recording-mock".into(),
            voice: "test".into(),
        })
    }
}

async fn boot_test_core() -> (Arc<AppState>, String, Arc<Mutex<Vec<String>>>) {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let (tts, spoken) = RecordingTts::new();
    let state = AppState::with_overrides(
        store,
        std::path::PathBuf::from("./data-test"),
        MockInject::default(),
        None,
        Some(tts),
        None,
        None,
    );
    let gw_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = gw_listener.local_addr().unwrap().port();
    drop(gw_listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    common::spawn_fake_device(gw_port).await;
    for _ in 0..100 {
        if state.hub.device_exists("dot2-d") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(state.hub.device_exists("dot2-d"), "fake device connected");
    state
        .config
        .save_device_meta("dot2-d", "书房音箱", "书房", "test")
        .unwrap();

    let api = hm_core::api::router(state.clone());
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(api_listener, api).await;
    });
    (state, format!("http://{api_addr}"), spoken)
}

async fn wait_for_voice_event(state: &AppState, kind: &str, id_match: Option<&str>) -> Value {
    for _ in 0..150 {
        let events = state.voice.recent(100);
        if let Some(ev) = events.into_iter().find(|e| {
            e["kind"].as_str() == Some(kind)
                && id_match.is_none_or(|id| e["id"].as_str() == Some(id))
        }) {
            return ev;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "timed out waiting for voice event '{kind}' (recent={:?})",
        state.voice.recent(20)
    );
}

#[tokio::test]
async fn slice1_announce_auth_gating_delivery_and_followup_window() {
    let (state, base, spoken) = boot_test_core().await;
    let client = reqwest::Client::new();

    // Provision household bearer auth and verify /api/v0/voice/announce rejects unauthenticated calls.
    let token = state.auth.provision("test-owner").unwrap();
    let unauth = client
        .post(format!("{base}/api/v0/voice/announce"))
        .json(&json!({ "text": "未鉴权测试", "task_id": "t-unauth" }))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Validation: empty text -> 400; bad priority -> 400; unknown device -> 404.
    let empty = client
        .post(format!("{base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(&json!({ "text": "   ", "task_id": "t1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), reqwest::StatusCode::BAD_REQUEST);

    let bad_prio = client
        .post(format!("{base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(&json!({ "text": "测试", "priority": "critical", "task_id": "t1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_prio.status(), reqwest::StatusCode::BAD_REQUEST);

    let known_t1 = state.tasks.create("slice1-validation").unwrap().id.clone();
    let unknown = client
        .post(format!("{base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(&json!({ "text": "测试", "device": "ghost-box", "task_id": known_t1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), reqwest::StatusCode::NOT_FOUND);

    // Unsolicited announcement (no task_id) and forged/unknown task_id are BOTH
    // rejected when proactive_speech is off (default).
    let unsolicited = client
        .post(format!("{base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(&json!({ "text": "主动打扰测试", "device": "dot2-d" }))
        .send()
        .await
        .unwrap();
    assert_eq!(unsolicited.status(), reqwest::StatusCode::CONFLICT);
    let err_body: Value = unsolicited.json().await.unwrap();
    assert_eq!(err_body["code"], "PROACTIVE_SPEECH_DISABLED");

    let forged_task = client
        .post(format!("{base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(
            &json!({ "text": "伪造任务测试", "device": "dot2-d", "task_id": "forged-unknown-999" }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(forged_task.status(), reqwest::StatusCode::CONFLICT);
    let forged_body: Value = forged_task.json().await.unwrap();
    assert_eq!(forged_body["code"], "PROACTIVE_SPEECH_DISABLED");

    // Task callback for a KNOWN task IS allowed even when proactive_speech=false,
    // speaks immediately on idle device, and opens bounded followup window when open_followup=true.
    let cb_task_1 = state.tasks.create("slice1-cb-1").unwrap().id.clone();
    let cb = client
        .post(format!("{base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(&json!({
            "device": "dot2-d",
            "title": "全屋音箱巡检",
            "text": "主脑查完了，客厅音箱运行正常。需要继续排查吗？",
            "priority": "high",
            "open_followup": true,
            "task_id": cb_task_1,
            "source": "master_brain"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(cb.status(), reqwest::StatusCode::ACCEPTED);
    let cb_body: Value = cb.json().await.unwrap();
    let announce_id = cb_body["id"].as_str().unwrap().to_string();
    assert_eq!(cb_body["device"], "dot2-d");
    assert_eq!(cb_body["open_followup"], true);

    let spoken_ev = wait_for_voice_event(&state, "voice_announce_spoken", Some(&announce_id)).await;
    assert_eq!(spoken_ev["task_id"], cb_task_1);
    assert!(spoken
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.contains("客厅音箱运行正常")));

    let followup_ev =
        wait_for_voice_event(&state, "voice_announce_followup_opened", Some(&announce_id)).await;
    assert_eq!(followup_ev["device"], "dot2-d");
    assert_eq!(followup_ev["credits"], 1);

    // Verify the device is now in a listening session, so a second announcement DEFERS (queued)
    // instead of interrupting the open listening window!
    let cb_task_2 = state.tasks.create("slice1-cb-2").unwrap().id.clone();
    let deferred = client
        .post(format!("{base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(&json!({
            "device": "dot2-d",
            "text": "这是排队的第二条通知。",
            "task_id": cb_task_2,
            "source": "master_brain"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(deferred.status(), reqwest::StatusCode::ACCEPTED);
    let def_body: Value = deferred.json().await.unwrap();
    let deferred_id = def_body["id"].as_str().unwrap().to_string();
    assert_eq!(def_body["status"], "queued");
    assert_eq!(def_body["deferred_reason"], "listening_session");

    // Inspect GET /api/v0/voice/announce shows pending + recent.
    let snap: Value = client
        .get(format!("{base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snap["pending"].as_array().unwrap().len(), 1);
    assert_eq!(snap["pending"][0]["id"], deferred_id);
    assert!(snap["recent"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["id"] == announce_id && r["status"] == "spoken"));

    // Queue cap (8) + high-priority eviction while the listening window is open
    // (1 deferred item is already queued above -> add 7 more normal to hit cap 8).
    for i in 1..8 {
        let tid = state
            .tasks
            .create(&format!("slice1-norm-{i}"))
            .unwrap()
            .id
            .clone();
        let r = client
            .post(format!("{base}/api/v0/voice/announce"))
            .bearer_auth(&token)
            .json(&json!({
                "device": "dot2-d",
                "text": format!("普通消息 {i}"),
                "priority": "normal",
                "task_id": tid
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::ACCEPTED);
    }
    // 9th normal is rejected with 429.
    let overflow_tid = state.tasks.create("slice1-norm-9").unwrap().id.clone();
    let overflow = client
        .post(format!("{base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(&json!({
            "device": "dot2-d",
            "text": "第9条普通消息",
            "priority": "normal",
            "task_id": overflow_tid
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(overflow.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);

    // A high-priority announcement evicts the oldest normal item (`deferred_id`) cleanly.
    let high_tid = state.tasks.create("slice1-high-1").unwrap().id.clone();
    let hi = client
        .post(format!("{base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(&json!({
            "device": "dot2-d",
            "text": "紧急高优播报",
            "priority": "high",
            "task_id": high_tid
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(hi.status(), reqwest::StatusCode::ACCEPTED);
    let hi_id = hi.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_voice_event(&state, "voice_announce_evicted", None).await;

    // Close the listening session -> 1 Hz sweep prioritizes & drains the high-priority item FIRST!
    let close_res = client
        .post(format!("{base}/api/v0/session/close"))
        .bearer_auth(&token)
        .json(&json!({ "device": "dot2-d" }))
        .send()
        .await
        .unwrap();
    assert_eq!(close_res.status(), reqwest::StatusCode::OK);

    let drained = wait_for_voice_event(&state, "voice_announce_spoken", Some(&hi_id)).await;
    assert_eq!(drained["task_id"], high_tid);
    assert_eq!(drained["priority"], "high");
    assert!(spoken
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.contains("紧急高优播报")));
}

#[tokio::test]
async fn slice2_master_brain_config_hot_reload_and_full_async_callback_loop() {
    let (state, base, spoken) = boot_test_core().await;
    let client = reqwest::Client::new();

    // Initially unconfigured: tools report Unconfigured, GET returns state="unconfigured".
    let initial: Value = client
        .get(format!("{base}/api/v0/config/services/master_brain"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(initial["state"], "unconfigured");
    assert_eq!(initial["bearer_token_set"], false);
    assert_eq!(
        state.master_brain.availability(),
        Availability::Unconfigured
    );

    // Mock NAS Master Brain bridge that accepts /v1/tasks and asynchronously calls back
    // speaker POST {callback_url}/api/v0/voice/announce!
    #[derive(Clone)]
    struct BridgeState {
        callback_client: reqwest::Client,
        received_tasks: Arc<Mutex<Vec<Value>>>,
    }
    let bridge_state = BridgeState {
        callback_client: reqwest::Client::new(),
        received_tasks: Arc::new(Mutex::new(Vec::new())),
    };
    let bridge_app = Router::new()
        .route(
            "/health",
            get(|headers: HeaderMap| async move {
                assert_eq!(
                    headers.get("authorization").unwrap(),
                    "Bearer nas-bridge-token"
                );
                Json(json!({ "status": "ok", "backend": "hermes-mock" }))
            }),
        )
        .route(
            "/v1/tasks",
            post(
                |State(st): State<BridgeState>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(
                        headers.get("authorization").unwrap(),
                        "Bearer nas-bridge-token"
                    );
                    st.received_tasks.lock().unwrap().push(body.clone());
                    let cb_url = body["callback_url"].as_str().unwrap().to_string();
                    let device = body["device"].as_str().unwrap().to_string();
                    let http = st.callback_client.clone();
                    // Simulate async background work on NAS, then push spoken result back!
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        let resp = http
                            .post(format!("{cb_url}/api/v0/voice/announce"))
                            .json(&json!({
                                "device": device,
                                "text": "主脑巡检完毕，家里六台音箱全部在线且语音链路正常。",
                                "task_id": "mb_run_42",
                                "source": "master_brain",
                                "open_followup": false
                            }))
                            .send()
                            .await
                            .unwrap();
                        assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);
                    });
                    (
                        StatusCode::ACCEPTED,
                        Json(json!({
                            "task_id": "mb_run_42",
                            "status": "queued",
                            "ack": "好的，这件事我交给主脑在后台巡检，好了告诉你。"
                        })),
                    )
                },
            ),
        )
        .with_state(bridge_state.clone());

    let bridge_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bridge_addr = bridge_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(bridge_listener, bridge_app).await;
    });
    let bridge_url = format!("http://{bridge_addr}");

    // Save Master Brain config via Management API (also verifying /api/v0/config/master-brain alias + live hot-reload).
    let put_res = client
        .put(format!("{base}/api/v0/config/master-brain"))
        .json(&json!({
            "enabled": true,
            "protocol": "generic",
            "url": bridge_url,
            "callback_url": base,
            "timeout_ms": 5000,
            "bearer_token": "nas-bridge-token"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(put_res.status(), reqwest::StatusCode::OK);
    let saved: Value = put_res.json().await.unwrap();
    assert_eq!(saved["state"], "ready");
    assert_eq!(saved["protocol"], "generic");
    assert_eq!(saved["bearer_token_set"], true);
    // Secret must NEVER be returned in plaintext.
    assert!(!saved.to_string().contains("nas-bridge-token"));

    // Config export includes master_brain without leaking plaintext secret.
    let exported: Value = client
        .get(format!("{base}/api/v0/config/export"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(exported["master_brain"]["enabled"], true);
    assert_eq!(exported["master_brain"]["protocol"], "generic");
    assert_eq!(exported["master_brain"]["bearer_token_set"], true);
    assert!(!exported.to_string().contains("nas-bridge-token"));

    // Mode snapshot reflects master_brain live state.
    let mode: Value = client
        .get(format!("{base}/api/v0/mode"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mode["master_brain"]["state"], "ready");
    assert!(mode["announce_queue"].is_object());

    // Connection probe succeeds against mock bridge /health with Bearer token via /master-brain/test.
    let probe: Value = client
        .post(format!("{base}/api/v0/config/master-brain/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["ok"], true);
    assert_eq!(state.master_brain.availability(), Availability::Ready);

    // Invoke `brain.delegate` via Registry (same path the agent loop executes!).
    let ctx = ToolCtx::new("T-mb", "C-mb", Arc::new(AtomicBool::new(false)))
        .with_provenance(Some("dot2-d".into()), Some("TR-mb".into()));
    let delegate_res = state
        .registry
        .call(
            "brain.delegate",
            json!({ "task": "检查全屋音箱是否都在线" }),
            &ctx,
        )
        .await;
    assert_eq!(
        delegate_res.status,
        ToolStatus::Completed,
        "{delegate_res:?}"
    );
    let out = delegate_res.result.unwrap();
    assert_eq!(out["task_id"], "mb_run_42");
    assert_eq!(out["replace_answer"], true);
    assert_eq!(
        out["rendered_answer"],
        "好的，这件事我交给主脑在后台巡检，好了告诉你。"
    );

    // Wait for the async callback from mock Master Brain to be announced through RecordingTts!
    let spoken_ev = wait_for_voice_event(&state, "voice_announce_spoken", None).await;
    assert_eq!(spoken_ev["task_id"], "mb_run_42");
    assert_eq!(spoken_ev["device"], "dot2-d");
    assert!(spoken
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.contains("家里六台音箱全部在线")));
}

#[tokio::test]
async fn slice2_direct_hermes_zero_bridge_sse_approval_and_cancel() {
    use axum::body::Body;
    use axum::extract::Path;
    use axum::response::Response;

    let (state, base, spoken) = boot_test_core().await;
    let client = reqwest::Client::new();

    #[derive(Clone)]
    struct MockHermesState {
        next_run: Arc<std::sync::atomic::AtomicUsize>,
        approvals: Arc<Mutex<Vec<Value>>>,
        stopped: Arc<Mutex<Vec<String>>>,
        approval_notify: Arc<tokio::sync::Notify>,
        cancel_notify: Arc<tokio::sync::Notify>,
    }
    let hermes_st = MockHermesState {
        next_run: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        approvals: Arc::new(Mutex::new(Vec::new())),
        stopped: Arc::new(Mutex::new(Vec::new())),
        approval_notify: Arc::new(tokio::sync::Notify::new()),
        cancel_notify: Arc::new(tokio::sync::Notify::new()),
    };

    let hermes_app = Router::new()
        .route(
            "/health",
            get(|| async {
                Json(json!({
                    "status": "ok",
                    "platform": "hermes-agent",
                    "version": "0.23.3"
                }))
            }),
        )
        .route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                if headers.get("authorization").and_then(|v| v.to_str().ok())
                    != Some("Bearer mac-hermes-secret")
                {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "error": "invalid api key" })),
                    );
                }
                (
                    StatusCode::OK,
                    Json(json!({ "data": [{ "id": "hermes-agent" }] })),
                )
            }),
        )
        .route(
            "/v1/runs",
            post(
                |State(st): State<MockHermesState>, headers: HeaderMap, Json(_body): Json<Value>| async move {
                    assert_eq!(
                        headers.get("authorization").unwrap(),
                        "Bearer mac-hermes-secret"
                    );
                    let n = st
                        .next_run
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        + 1;
                    (
                        StatusCode::ACCEPTED,
                        Json(json!({
                            "run_id": format!("hermes_e2e_{n}"),
                            "status": "started"
                        })),
                    )
                },
            ),
        )
        .route(
            "/v1/runs/{id}",
            get(|Path(id): Path<String>| async move {
                Json(json!({ "run_id": id, "status": "running" }))
            }),
        )
        .route(
            "/v1/runs/{id}/events",
            get(
                |State(st): State<MockHermesState>, Path(id): Path<String>| async move {
                    let (tx, rx) =
                        tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);
                    tokio::spawn(async move {
                        if id == "hermes_e2e_1" {
                            let _ = tx
                                .send(Ok(bytes::Bytes::from(
                                    "event: tool.start\ndata: {\"tool\":\"terminal\"}\n\n",
                                )))
                                .await;
                            tokio::time::sleep(Duration::from_millis(60)).await;
                            let _ = tx
                                .send(Ok(bytes::Bytes::from(
                                    "event: approval_required\ndata: {\"command\":\"ls ~/Desktop\"}\n\n",
                                )))
                                .await;
                            st.approval_notify.notified().await;
                            let _ = tx
                                .send(Ok(bytes::Bytes::from(
                                    "event: run.completed\ndata: {\"output\":\"# 检查结果\\n**桌面**共有 `12` 个文件 [详情](https://example.local/x)\"}\n\n",
                                )))
                                .await;
                        } else {
                            let _ = tx
                                .send(Ok(bytes::Bytes::from(
                                    "event: tool.start\ndata: {\"tool\":\"web_search\"}\n\n",
                                )))
                                .await;
                            st.cancel_notify.notified().await;
                            // Simulate a racy `run.completed` arriving synchronously during `/stop`:
                            // local cancellation MUST suppress any spurious announcement!
                            let _ = tx
                                .send(Ok(bytes::Bytes::from(
                                    "event: run.completed\ndata: {\"output\":\"这句不该被播报\"}\n\n",
                                )))
                                .await;
                        }
                    });
                    let stream = futures::stream::unfold(rx, |mut rx| async move {
                        rx.recv().await.map(|item| (item, rx))
                    });
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(stream))
                        .unwrap()
                },
            ),
        )
        .route(
            "/v1/runs/{id}/approval",
            post(
                |State(st): State<MockHermesState>, Json(body): Json<Value>| async move {
                    st.approvals.lock().unwrap().push(body);
                    st.approval_notify.notify_waiters();
                    Json(json!({ "ok": true }))
                },
            ),
        )
        .route(
            "/v1/runs/{id}/stop",
            post(
                |State(st): State<MockHermesState>, Path(id): Path<String>| async move {
                    st.stopped.lock().unwrap().push(id);
                    st.cancel_notify.notify_waiters();
                    // Hold `/stop` briefly so the SSE `run.completed` arrives BEFORE `/stop` responds,
                    // proving the pre-flight `"cancelling"` guard suppresses the race!
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    Json(json!({ "status": "stopped" }))
                },
            ),
        )
        .with_state(hermes_st.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hermes_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, hermes_app).await;
    });
    let hermes_url = format!("http://{hermes_addr}");

    // 1. Default protocol is "hermes" (no callback_url needed). Wrong key fails /test honestly!
    let put_bad = client
        .put(format!("{base}/api/v0/config/master-brain"))
        .json(&json!({
            "enabled": true,
            "url": hermes_url,
            "bearer_token": "wrong-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(put_bad.status(), reqwest::StatusCode::OK);
    let saved_bad: Value = put_bad.json().await.unwrap();
    assert_eq!(saved_bad["protocol"], "hermes");

    let bad_probe: Value = client
        .post(format!("{base}/api/v0/config/master-brain/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(bad_probe["ok"], false);
    assert_eq!(bad_probe["protocol"], "hermes");

    // 2. Save correct Hermes API key -> /test passes.
    let put_ok = client
        .put(format!("{base}/api/v0/config/master-brain"))
        .json(&json!({
            "enabled": true,
            "protocol": "hermes",
            "url": hermes_url,
            "bearer_token": "mac-hermes-secret"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(put_ok.status(), reqwest::StatusCode::OK);

    let ok_probe: Value = client
        .post(format!("{base}/api/v0/config/master-brain/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ok_probe["ok"], true);
    assert_eq!(ok_probe["protocol"], "hermes");

    // 3. Delegate -> SSE tool.start -> approval_required -> in-process voice announce + open_followup!
    let ctx = ToolCtx::new("T-h1", "C-h1", Arc::new(AtomicBool::new(false)))
        .with_provenance(Some("dot2-d".into()), Some("TR-h1".into()));
    let del = state
        .registry
        .call(
            "brain.delegate",
            json!({ "task": "帮我看一下桌面上有多少文件" }),
            &ctx,
        )
        .await;
    assert_eq!(del.status, ToolStatus::Completed, "{del:?}");
    assert_eq!(del.result.unwrap()["task_id"], "hermes_e2e_1");

    // Wait for approval prompt announcement & followup window opened directly in-process!
    let approval_ev = wait_for_voice_event(&state, "voice_announce_spoken", None).await;
    assert_eq!(approval_ev["task_id"], "hermes_e2e_1");
    assert_eq!(approval_ev["open_followup"], true);
    let approval_id = approval_ev["id"].as_str().unwrap().to_string();
    wait_for_voice_event(&state, "voice_announce_followup_opened", Some(&approval_id)).await;

    // `brain.status` exposes `last_tool` captured from SSE `tool.start`!
    let st_res = state.registry.call("brain.status", json!({}), &ctx).await;
    assert_eq!(st_res.status, ToolStatus::Completed, "{st_res:?}");
    let st_body = st_res.result.unwrap();
    assert_eq!(st_body["last_tool"], "terminal");
    assert!(st_body["pending_approval"].is_string());

    // Close the listening window so the completion announcement can speak immediately after approval.
    let close_res = client
        .post(format!("{base}/api/v0/session/close"))
        .json(&json!({ "device": "dot2-d" }))
        .send()
        .await
        .unwrap();
    assert_eq!(close_res.status(), reqwest::StatusCode::OK);

    // 4. User approves via `brain.steer("确认执行")` -> routes to Hermes `/approval` with choice="once"!
    let approve = state
        .registry
        .call("brain.steer", json!({ "guidance": "确认执行" }), &ctx)
        .await;
    assert_eq!(approve.status, ToolStatus::Completed, "{approve:?}");
    let appr_out = approve.result.unwrap();
    assert_eq!(appr_out["choice"], "once");
    assert_eq!(hermes_st.approvals.lock().unwrap().len(), 1);
    assert_eq!(hermes_st.approvals.lock().unwrap()[0]["choice"], "once");

    // 5. Wait for final `run.completed` spoken announcement with Markdown stripped!
    let mut final_spoken = None;
    for _ in 0..150 {
        let lines = spoken.lock().unwrap().clone();
        if let Some(line) = lines.into_iter().find(|s| s.contains("12")) {
            final_spoken = Some(line);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let final_spoken = final_spoken.expect("expected final Hermes result to be spoken");
    assert!(final_spoken.contains("桌面"));
    assert!(final_spoken.contains("12"));
    assert!(!final_spoken.contains("**"));
    assert!(!final_spoken.contains('`'));
    assert!(!final_spoken.contains("https://"));

    // 6. Second run: cancel stops run and produces NO spurious completion/failure announcement.
    let spoken_count_before = spoken.lock().unwrap().len();
    let del2 = state
        .registry
        .call(
            "brain.delegate",
            json!({ "task": "做一个很长的调研" }),
            &ctx,
        )
        .await;
    assert_eq!(del2.status, ToolStatus::Completed);
    assert_eq!(del2.result.unwrap()["task_id"], "hermes_e2e_2");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let cancel = state.registry.call("brain.cancel", json!({}), &ctx).await;
    assert_eq!(cancel.status, ToolStatus::Completed, "{cancel:?}");
    assert_eq!(
        hermes_st.stopped.lock().unwrap().as_slice(),
        &["hermes_e2e_2"]
    );
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        spoken.lock().unwrap().len(),
        spoken_count_before,
        "cancelled Hermes run must not emit a spurious voice announcement"
    );
}
