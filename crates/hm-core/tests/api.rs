//! HTTP-level integration: management API + chat SSE over a real listener,
//! with the scripted fake model injected. Verifies the console's actual
//! backend, including cancel-via-API (shared task registry).

use futures::StreamExt;
use hm_core::api::{router, AppState};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::tool::mocks::MockInject;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

async fn spawn_app(llm: FakeLlm, inject: MockInject) -> (String, Arc<AppState>) {
    spawn_app_at(llm, inject, PathBuf::from("./data-test")).await
}

/// Same app, but with an explicit data dir — the journal test needs an
/// isolated `voice-events/` because every pipeline in this test process
/// shares `./data-test` and the export is a bounded read.
async fn spawn_app_at(
    llm: FakeLlm,
    inject: MockInject,
    data_dir: PathBuf,
) -> (String, Arc<AppState>) {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    let state = AppState::new(store, data_dir, inject, Some(Arc::new(llm)));
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

/// Read an SSE response body into parsed JSON events.
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

#[tokio::test]
async fn chat_sse_then_trace_and_status() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("hotlist.query".into(), "{}".into())],
        },
        // Music needs a configured service (R1e); this test is about the SSE /
        // trace / config surface, so it uses the household light instead.
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                r#"{"entity":"room.light","state":"on"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["好的，灯开了。".into()],
        },
    ]);
    let (base, _state) = spawn_app(llm, MockInject::default()).await;
    let client = reqwest::Client::new();

    // Chat over SSE.
    let resp = client
        .post(format!("{base}/api/v0/chat"))
        .json(&json!({ "text": "放热榜第一首" }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let events = sse_events(resp).await;
    let types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
    assert_eq!(types.first(), Some(&"session"));
    assert!(types.contains(&"task"));
    // R2: the core's own action sentence travels on its own event (`fact`);
    // both are the assistant speaking, and the console renders both.
    assert!(
        types.contains(&"token") || types.contains(&"fact"),
        "the answer must reach the console as text: {types:?}"
    );
    assert!(types.contains(&"tool_start"));
    assert!(types.contains(&"tool_result"));
    assert_eq!(types.last(), Some(&"turn_end"));

    let session_id = events[0]["session_id"].as_str().unwrap().to_string();
    let trace_id = events.last().unwrap()["trace_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Session history persisted.
    let msgs: Value = client
        .get(format!("{base}/api/v0/session/{session_id}/messages"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(serde_json::to_string(&msgs)
        .unwrap()
        .contains("放热榜第一首"));

    // Trace detail explains the turn.
    let detail: Value = client
        .get(format!("{base}/api/v0/trace/{trace_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let evs = detail["events"].as_array().unwrap();
    let kinds: Vec<&str> = evs.iter().filter_map(|e| e["kind"].as_str()).collect();
    assert!(kinds.contains(&"turn_start"));
    assert!(kinds.contains(&"tool_start"));
    assert!(kinds.contains(&"tool_result"));
    assert!(kinds.contains(&"llm_end"));
    assert_eq!(detail["status"], "completed");

    // Status endpoint.
    let status: Value = client
        .get(format!("{base}/api/v0/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["product"], "Home Mate");
    assert_eq!(status["assistant_name"], "Home Mate");
    assert_eq!(status["services"]["llm"]["provider"], "deepseek");

    // Config: read, update tts independently, history recorded.
    let put: Value = client
        .put(format!("{base}/api/v0/config/services/tts"))
        .json(&json!({ "provider": "mock-tts", "base_url": "", "model": "v1", "api_key_env": "", "api_key": "secret-xyz" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put["state"], "applied");
    assert_eq!(put["service"]["provider"], "mock-tts");
    assert!(put["service"]["api_key_set"].as_bool().unwrap());
    assert!(
        !serde_json::to_string(&put).unwrap().contains("secret-xyz"),
        "key never echoed"
    );

    let hist: Value = client
        .get(format!("{base}/api/v0/config/history"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(serde_json::to_string(&hist).unwrap().contains("tts"));

    // 2026-11-14: a save that does not own `api_key_env` must not erase it.
    // The wizard omitted the field, the route reset it to "", and a working
    // `DEEPSEEK_API_KEY` reference turned into "no key" behind a card that
    // still said configured.
    let put: Value = client
        .put(format!("{base}/api/v0/config/services/llm"))
        .json(
            &json!({ "provider": "deepseek", "base_url": "https://api.deepseek.com",
                       "model": "deepseek-flash", "api_key_env": "HM_TEST_KEEP_ENV" }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put["service"]["api_key_env"], "HM_TEST_KEEP_ENV");
    let put: Value = client
        .put(format!("{base}/api/v0/config/services/llm"))
        .json(
            &json!({ "provider": "deepseek", "base_url": "https://api.deepseek.com",
                       "model": "deepseek-flash" }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        put["service"]["api_key_env"], "HM_TEST_KEEP_ENV",
        "an absent field keeps the stored name"
    );
    let put: Value = client
        .put(format!("{base}/api/v0/config/services/llm"))
        .json(
            &json!({ "provider": "deepseek", "base_url": "https://api.deepseek.com",
                       "model": "deepseek-flash", "api_key_env": "" }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        put["service"]["api_key_env"], "",
        "explicit empty clears it"
    );

    // Cancel of an unknown task is honest, not an error.
    let cancel: Value = client
        .post(format!("{base}/api/v0/session/cancel"))
        .json(&json!({ "task_id": "T-nope" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cancel["result"], "unknown_task");
}

#[tokio::test]
async fn cancel_via_api_stops_a_running_turn() {
    let llm = FakeLlm::new(vec![
        // The chart lookup sleeps 300 ms: the window the cancel lands in.
        Scripted::Calls {
            calls: vec![("hotlist.query".into(), "{}".into())],
        },
        Scripted::Text {
            chunks: vec!["不应该到达".into()],
        },
    ]);
    let (base, _state) = spawn_app(
        llm,
        MockInject {
            hotlist_delay_ms: 300,
            ..Default::default()
        },
    )
    .await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/v0/chat"))
        .json(&json!({ "text": "放首歌" }))
        .send()
        .await
        .unwrap();
    let mut bytes = resp.bytes_stream();
    let mut buf = String::new();
    let mut types: Vec<String> = Vec::new();
    #[allow(unused_assignments)]
    let mut task_id = String::new();
    while let Some(chunk) = bytes.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        while let Some(nl) = buf.find('\n') {
            let line: String = buf.drain(..=nl).collect();
            let line = line.trim().to_string();
            if let Some(data) = line.strip_prefix("data:") {
                if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                    if v["type"] == "task" {
                        task_id = v["task_id"].as_str().unwrap().to_string();
                        // Cancel while the sink is still sleeping.
                        let c = reqwest::Client::new();
                        let r: Value = c
                            .post(format!("{base}/api/v0/session/cancel"))
                            .json(&json!({ "task_id": task_id }))
                            .send()
                            .await
                            .unwrap()
                            .json()
                            .await
                            .unwrap();
                        assert_eq!(r["result"], "cancel_requested");
                    }
                    types.push(v["type"].as_str().unwrap_or("?").to_string());
                }
            }
        }
    }
    assert!(
        types.contains(&"cancelled".to_string()),
        "events: {types:?}"
    );
    assert_eq!(types.last().unwrap(), "turn_end");

    let tasks: Value = client
        .get(format!("{base}/api/v0/tasks"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tasks["tasks"][0]["status"], "cancelled");
}

#[tokio::test]
async fn config_history_restore_applies_old_values_keeps_current_key() {
    let (base, _state) = spawn_app(FakeLlm::new(vec![]), MockInject::default()).await;
    let client = reqwest::Client::new();

    // v1 then v2 of the tts config, each with its own key.
    for (provider, model, key) in [("alpha", "v1", "key-one"), ("beta", "v2", "key-two")] {
        let put: Value = client
            .put(format!("{base}/api/v0/config/services/tts"))
            .json(&json!({ "provider": provider, "base_url": "", "model": model, "api_key_env": "", "api_key": key }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(put["state"], "applied");
    }

    // History rows carry the restorable snapshot but never key values.
    let hist: Value = client
        .get(format!("{base}/api/v0/config/history"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = hist["history"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let raw = serde_json::to_string(&hist).unwrap();
    assert!(
        !raw.contains("key-one") && !raw.contains("key-two"),
        "secrets never enter history"
    );
    let v1_id = rows
        .iter()
        .rev()
        .find(|r| r["payload"]["model"] == json!("v1"))
        .unwrap()["id"]
        .as_i64()
        .unwrap();

    // Restore v1: provider/model go back; the key stays the CURRENT one.
    let restore: Value = client
        .post(format!("{base}/api/v0/config/history/{v1_id}/restore"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(restore["state"], "applied", "{restore:?}");
    let services: Value = client
        .get(format!("{base}/api/v0/config/services"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(services["tts"]["provider"], "alpha");
    assert_eq!(services["tts"]["model"], "v1");
    assert_eq!(
        services["tts"]["api_key_set"], true,
        "the stored key survives a restore"
    );

    // The restore itself is recorded with its own actor.
    let hist: Value = client
        .get(format!("{base}/api/v0/config/history"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let latest = &hist["history"][0];
    assert_eq!(latest["actor"], "restore");
    assert_eq!(latest["payload"]["model"], json!("v1"));

    // Unknown row is a 404, not a silent no-op.
    let missing = client
        .post(format!("{base}/api/v0/config/history/99999/restore"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}

/// R1f: a config backup is one file that restores services, music and voice
/// settings through the same validated paths; secrets are opt-in and never
/// leak into the default export.
#[tokio::test]
async fn config_backup_roundtrips_services_music_and_mode() {
    let (base, state) = spawn_app(FakeLlm::new(vec![]), MockInject::default()).await;
    let client = reqwest::Client::new();

    let put: Value = client
        .put(format!("{base}/api/v0/config/services/llm"))
        .json(&json!({
            "provider": "deepseek", "base_url": "https://api.deepseek.com",
            "model": "deepseek-flash", "api_key_env": "DEEPSEEK_API_KEY",
            "thinking": "disabled", "api_key": "sk-backup-1"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put["state"], "applied");
    let put: Value = client
        .put(format!("{base}/api/v0/config/services/music"))
        .json(&json!({
            "enabled": true, "prefer_local": true,
            "library": {"endpoint": "http://192.168.1.168:4533", "user": "u", "secret": "lib-backup-1"},
            "online": {"endpoint": "http://192.168.1.168:18082", "secret": "online-backup-1"}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put["enabled"], true);
    state
        .voice
        .set_mode(
            "dot2-d",
            None,
            None,
            Some(vec!["你好高达".into()]),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(200),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "test",
        )
        .unwrap();

    // Default export: key flags, no values.
    let plain: Value = client
        .get(format!("{base}/api/v0/config/export"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let raw = serde_json::to_string(&plain).unwrap();
    assert!(
        !raw.contains("sk-backup-1")
            && !raw.contains("lib-backup-1")
            && !raw.contains("online-backup-1"),
        "the default export must not contain secrets"
    );
    assert_eq!(plain["services"]["llm"]["api_key_set"], true);
    assert_eq!(plain["music"]["library"]["secret_set"], true);
    assert!(plain.get("secrets").is_none());

    // Explicit opt-in with POST + confirmation header carries them.
    let full: Value = client
        .post(format!("{base}/api/v0/config/export?secrets=1"))
        .header("X-HM-Confirm-Secrets", "EXPORT_SECRETS")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(full["secrets"]["llm.api_key"], "sk-backup-1");
    assert_eq!(full["secrets"]["music.library.secret"], "lib-backup-1");
    assert_eq!(full["secrets"]["music.online.secret"], "online-backup-1");

    // Change every scope.
    let _ = client
        .put(format!("{base}/api/v0/config/services/llm"))
        .json(&json!({
            "provider": "deepseek", "base_url": "https://api.deepseek.com",
            "model": "changed", "api_key_env": "", "thinking": "disabled"
        }))
        .send()
        .await
        .unwrap();
    let _ = client
        .put(format!("{base}/api/v0/config/services/music"))
        .json(&json!({"enabled": false, "prefer_local": true}))
        .send()
        .await
        .unwrap();
    state
        .voice
        .set_mode(
            "dot2-d",
            None,
            None,
            Some(vec!["别的".into()]),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(400),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "test",
        )
        .unwrap();
    assert_eq!(
        state
            .voice
            .settings_snapshot()
            .effective_for("dot2-d")
            .wake_words,
        vec!["别的".to_string()]
    );

    // Import restores all three through the normal save paths.
    let res: Value = client
        .post(format!("{base}/api/v0/config/import"))
        .json(&full)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["state"], "applied", "{res:?}");
    let services: Value = client
        .get(format!("{base}/api/v0/config/services"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(services["llm"]["model"], "deepseek-flash");
    assert_eq!(services["llm"]["api_key_set"], true);
    let music: Value = client
        .get(format!("{base}/api/v0/config/services/music"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(music["enabled"], true);
    assert_eq!(music["library"]["endpoint"], "http://192.168.1.168:4533");
    assert_eq!(
        music["library"]["secret_set"], true,
        "the secret comes back through the import"
    );
    assert_eq!(
        state
            .voice
            .settings_snapshot()
            .effective_for("dot2-d")
            .wake_words,
        vec!["你好高达".to_string()]
    );
    assert_eq!(
        state
            .voice
            .settings_snapshot()
            .effective_for("dot2-d")
            .tail_blind_ms,
        200
    );

    // Import lands in the change record with its own actor.
    let hist: Value = client
        .get(format!("{base}/api/v0/config/history"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(hist["history"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["actor"] == "import"));
}

/// R1f: a stale music form must not wipe the other source. The console
/// echoes the revision it loaded; a save after another page changed the
/// block is refused with 409 (the 2026-11-12 12:39 Navidrome loss).
#[tokio::test]
async fn a_stale_music_revision_is_refused_instead_of_overwriting() {
    let (base, _state) = spawn_app(FakeLlm::new(vec![]), MockInject::default()).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/api/v0/config/services/music");

    let before: Value = client.get(&url).send().await.unwrap().json().await.unwrap();
    let r0 = before["revision"].as_str().unwrap().to_string();

    // Page A saves the library with the revision it loaded.
    let saved: Value = client
        .put(&url)
        .json(&json!({
            "enabled": true, "prefer_local": true,
            "library": {"endpoint": "http://192.168.1.168:4533", "user": "u", "secret": "s"},
            "revision": r0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["library"]["endpoint"], "http://192.168.1.168:4533");

    // Page B still holds r0 and saves only the online source → 409, no write.
    let stale = client
        .put(&url)
        .json(&json!({
            "enabled": true, "prefer_local": true,
            "online": {"endpoint": "http://192.168.1.168:18082", "secret": "t"},
            "revision": r0,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    let now: Value = client.get(&url).send().await.unwrap().json().await.unwrap();
    assert_eq!(
        now["library"]["endpoint"], "http://192.168.1.168:4533",
        "the stale save must not wipe the library"
    );
    assert!(now["online"].is_null());
}

/// R1f: the next turn's context replays the previous tool call structurally
/// (assistant.tool_calls + role=tool), not as `[调用 …]` assistant prose.
/// That prose was what a weak model imitated and read out ("念调用").
#[tokio::test]
async fn a_follow_up_turn_replays_tool_calls_structurally() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                r#"{"entity":"room.light","state":"on"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["灯开了。".into()],
        },
        Scripted::Text {
            chunks: vec!["刚才开的是灯。".into()],
        },
    ]);
    let (base, _state) = spawn_app(llm.clone(), MockInject::default()).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/v0/chat"))
        .json(&json!({ "text": "开灯" }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let first = sse_events(resp).await;
    // Same session: otherwise there is no history to replay at all.
    let sid = first[0]["session_id"].as_str().unwrap().to_string();
    let resp = client
        .post(format!("{base}/api/v0/chat"))
        .json(&json!({ "session_id": sid, "text": "我刚才让你做什么" }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let _ = sse_events(resp).await;
    let reqs = llm.seen_requests();
    assert!(
        reqs.len() >= 3,
        "two turns with a tool call expected: {reqs:?}"
    );
    let second = reqs.last().unwrap();
    assert!(
        second
            .iter()
            .any(|m| m.role == "tool" && m.tool_call_id.is_some()),
        "the tool result must replay as role=tool: {second:?}"
    );
    assert!(
        second.iter().any(|m| m
            .tool_calls
            .as_ref()
            .map(|c| !c.is_empty())
            .unwrap_or(false)),
        "the call must replay as assistant.tool_calls: {second:?}"
    );
    assert!(
        !second
            .iter()
            .any(|m| m.content.as_deref().unwrap_or("").contains("[调用")),
        "the model must never see the readable call line: {second:?}"
    );
}

/// R1f: clearing a session drops its conversation context (the management
/// counterpart of 退下＝清零). Traces stay; the next turn starts fresh.
#[tokio::test]
async fn clearing_a_session_drops_its_context() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["好的。".into()],
    }]);
    let (base, _state) = spawn_app(llm, MockInject::default()).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/v0/chat"))
        .json(&json!({ "text": "你好" }))
        .send()
        .await
        .unwrap();
    let events = sse_events(resp).await;
    let sid = events[0]["session_id"].as_str().unwrap().to_string();
    let msgs: Value = client
        .get(format!("{base}/api/v0/session/{sid}/messages"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!msgs["messages"].as_array().unwrap().is_empty());

    let cleared: Value = client
        .post(format!("{base}/api/v0/session/{sid}/clear"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(cleared["cleared"].as_u64().unwrap() >= 2, "{cleared:?}");
    let msgs: Value = client
        .get(format!("{base}/api/v0/session/{sid}/messages"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(msgs["messages"].as_array().unwrap().is_empty(), "{msgs:?}");
}

#[tokio::test]
async fn device_meta_roundtrip_history_export_and_restore() {
    let (base, _state) = spawn_app(FakeLlm::new(vec![]), MockInject::default()).await;
    let client = reqwest::Client::new();

    let empty: Value = client
        .get(format!("{base}/api/v0/device-meta"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(empty["meta"].as_object().unwrap().is_empty());

    let saved: Value = client
        .put(format!("{base}/api/v0/device-meta/dot2-d"))
        .json(&json!({ "name": "客厅小达", "room": "客厅" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["state"], "applied");
    assert_eq!(saved["meta"]["name"], "客厅小达");
    assert_eq!(saved["meta"]["room"], "客厅");

    let all: Value = client
        .get(format!("{base}/api/v0/device-meta"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all["meta"]["dot2-d"]["room"], "客厅");

    let bad = client
        .put(format!("{base}/api/v0/device-meta/dot2-d"))
        .json(&json!({ "name": "x".repeat(41), "room": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400, "41-character names are rejected");

    let hist: Value = client
        .get(format!("{base}/api/v0/config/history"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = hist["history"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["scope"] == "device")
        .expect("device meta save lands in history")
        .clone();
    let id = entry["id"].as_i64().unwrap();

    let export: Value = client
        .get(format!("{base}/api/v0/config/export"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(export["devices"]["dot2-d"]["name"], "客厅小达");

    // Clearing both fields removes the label; restore brings it back.
    client
        .put(format!("{base}/api/v0/device-meta/dot2-d"))
        .json(&json!({ "name": "", "room": "" }))
        .send()
        .await
        .unwrap();
    let cleared: Value = client
        .get(format!("{base}/api/v0/device-meta"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(cleared["meta"].as_object().unwrap().is_empty());

    let restored: Value = client
        .post(format!("{base}/api/v0/config/history/{id}/restore"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(restored["state"], "applied", "{restored:?}");
    let after: Value = client
        .get(format!("{base}/api/v0/device-meta"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["meta"]["dot2-d"]["name"], "客厅小达");
}

/// R1j/P0-b: the live 21:53 defect, at the API boundary. With no home backend
/// the tool must produce a NAMED failure (`HOME_UNCONFIGURED`) and no
/// confirmation, and `/mode` must render the service as unconfigured. The
/// scripted model here says the honest sentence; the tool layer is what the
/// assertion pins down (a model can only relay what it is told).
#[tokio::test]
async fn an_unconfigured_home_cannot_confirm_the_light_was_turned_off() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                r#"{"entity":"children_room.light","state":"off"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["还没有接入家庭自动化，我现在开不了灯。".into()],
        },
    ]);
    let (base, _state) = spawn_app(llm, MockInject::default()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/v0/chat"))
        .json(&json!({ "text": "关闭儿童房的灯" }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let events = sse_events(resp).await;
    let result = events
        .iter()
        .find(|e| e["type"] == "tool_result")
        .unwrap_or_else(|| panic!("no tool_result in {events:?}"));
    assert_eq!(result["tool"], "home.set");
    assert_eq!(
        result["status"], "failed",
        "an unconfigured backend is a failure: {result}"
    );
    assert_ne!(result["evidence"], "deviceconfirmed");
    assert_eq!(
        result["summary"]
            .as_str()
            .unwrap_or("")
            .contains("HOME_UNCONFIGURED"),
        true
    );

    let mode: Value = client
        .get(format!("{base}/api/v0/mode"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mode["home"]["backend"], "none");
    assert_eq!(mode["home"]["state"], "unconfigured");
    assert!(!serde_json::to_string(&mode["home"])
        .unwrap()
        .contains("DeviceConfirmed"));

    // The config block is a real management path, and an unknown backend is a
    // named error, not a silently-broken state.
    let bad = client
        .put(format!("{base}/api/v0/config/services/home"))
        .json(&json!({ "backend": "magic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let ok = client
        .put(format!("{base}/api/v0/config/services/home"))
        .json(&json!({ "backend": "home_assistant" }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(ok["backend"], "home_assistant");
    // R2-BM5: the adapter exists, so the reserved value is no longer
    // "disabled"; an incomplete block is honestly unconfigured and says what
    // is missing (here: address, token and whitelist).
    assert_eq!(ok["state"], "unconfigured");
    assert_eq!(ok["token_set"], false);
    assert!(
        ok["detail"].as_str().unwrap().contains("地址")
            || ok["detail"].as_str().unwrap().contains("令牌"),
        "{ok}"
    );

    // ...and a saved config survives export/import (the household's rule).
    let export: Value = client
        .get(format!("{base}/api/v0/config/export"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(export["home"]["backend"], "home_assistant");
}

/// R1j/P1: the console does not have to know pinyin. Typing the Hanzi is
/// enough — the same generator the save path uses answers here, and a word
/// saved without tokens gets them.
#[tokio::test]
async fn reflex_tokens_are_generated_from_hanzi() {
    let llm = FakeLlm::new(vec![]);
    let (base, _state) = spawn_app(llm, MockInject::default()).await;
    let client = reqwest::Client::new();

    let mut url = reqwest::Url::parse(&format!("{base}/api/v0/reflex/tokens")).unwrap();
    url.query_pairs_mut().append_pair("text", "暂停播放");
    let r: Value = client.get(url).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["tokens"], "z àn t íng b ō f àng");
    assert!(r["error"].is_null());

    let mut url = reqwest::Url::parse(&format!("{base}/api/v0/reflex/tokens")).unwrap();
    url.query_pairs_mut().append_pair("text", "he");
    let r: Value = client.get(url).send().await.unwrap().json().await.unwrap();
    assert!(
        r["error"].as_str().unwrap_or("").contains("不是汉字"),
        "a non-Hanzi word is named: {r}"
    );

    // Saving a word with no tokens generates them (a word is typed once).
    let put: Value = client
        .put(format!("{base}/api/v0/reflex"))
        .json(&json!({ "words": [ { "text": "你好高达", "tokens": "", "intent": "wake", "enabled": true } ] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put["words"][0]["tokens"], "n ǐ h ǎo g āo d á");
    assert!(put["compiled"].as_str().unwrap().contains("@你好高达"));
}

/// M6b (S3): the ring stays a 100-event snapshot, the journal keeps the
/// window. The export is the acceptance feed, so it must survive a burst
/// (150 events), offer an exact cursor, and respect the `since` filter.
#[tokio::test]
async fn voice_events_journal_exports_past_the_ring() {
    let dir = std::env::temp_dir().join(format!(
        "hm-events-test-{}-{}",
        std::process::id(),
        hm_core::id::now_ms()
    ));
    let llm = FakeLlm::new(vec![]);
    let (base, state) = spawn_app_at(llm, MockInject::default(), dir.clone()).await;
    let client = reqwest::Client::new();
    let marker = format!("journal-{}", hm_core::id::now_ms());
    // `since` is EXCLUSIVE and the burst may share the millisecond it was
    // taken in; the marker filter is what makes this test independent of
    // other tests writing to the same test data dir.
    let since = 0u64;

    for i in 0..150 {
        state
            .voice
            .log_event("journal_probe", json!({ "marker": marker, "i": i }));
    }

    // The console's view is unchanged: the last 100, newest first.
    let log: Value = client
        .get(format!("{base}/api/v0/voice/log"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(log["events"].as_array().unwrap().len(), 100);

    // The journal drains on its own thread; poll briefly instead of sleeping.
    let mut mine = Vec::new();
    for _ in 0..100 {
        let body: Value = client
            .get(format!(
                "{base}/api/v0/voice/events?since={since}&limit=1000"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        mine = body["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e["marker"] == json!(marker))
            .collect();
        if mine.len() == 150 {
            // Exact cursor: the last returned seq continues with nothing.
            let next = mine.last().unwrap()["seq"].as_u64().unwrap();
            let epoch = mine.last().unwrap()["core_epoch"].as_u64().unwrap();
            let again: Value = client
                .get(format!(
                    "{base}/api/v0/voice/events?since_seq={next}&since_epoch={epoch}&limit=1000"
                ))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let after: Vec<Value> = again["events"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|e| e["marker"] == json!(marker))
                .collect();
            assert!(
                after.is_empty(),
                "the seq cursor must be exclusive: {}",
                after.len()
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(mine.len(), 150, "the whole burst must be exportable");
    // Order is write order, and the sequence is strictly increasing.
    let seqs: Vec<u64> = mine.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seq must increase: {seqs:?}"
    );
    assert_eq!(mine[0]["i"].as_u64(), Some(0));
    assert_eq!(mine[149]["i"].as_u64(), Some(149));

    // The wall-clock cursor is exclusive: a `since` past the burst returns
    // none of it (while other tests' events may still be in there).
    let ahead = hm_core::id::now_ms() + 1_000;
    let none: Value = client
        .get(format!(
            "{base}/api/v0/voice/events?since={ahead}&limit=1000"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let after_ahead = none["events"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e["marker"] == json!(marker))
        .count();
    assert_eq!(after_ahead, 0, "since is exclusive");

    // Files are bounded: the journal dir never exceeds its total budget.
    let mode: Value = client
        .get(format!("{base}/api/v0/mode"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let stats = &mode["voice_journal"];
    assert!(stats["bytes"].as_u64().unwrap() <= stats["total_cap_bytes"].as_u64().unwrap());
    assert!(stats["files"].as_u64().unwrap() >= 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The deploy endpoint takes an optional body: an EMPTY one must mean
/// "deploy the saved table". Before this, a client that declared an empty
/// body (urllib sends Content-Length: 0) got 415 — the call never reached the
/// real precondition check.
#[tokio::test]
async fn reflex_deploy_accepts_an_empty_or_missing_body() {
    let llm = FakeLlm::new(vec![]);
    let (base, _state) = spawn_app(llm, MockInject::default()).await;
    let client = reqwest::Client::new();

    // No audio device online in this app: the honest answer is 409 with the
    // REAL reason — not a media-type error from the extractor.
    let r = client
        .post(format!("{base}/api/v0/reflex/deploy"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], json!("no audio device online"));

    // An explicitly empty body with a JSON content type is the same call.
    let r = client
        .post(format!("{base}/api/v0/reflex/deploy"))
        .header("content-type", "application/json")
        .body("")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);

    // Malformed JSON is a named 400, never silently defaulted.
    let r = client
        .post(format!("{base}/api/v0/reflex/deploy"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn factory_reset_requires_confirmation_and_preserves_secrets_by_default() {
    let (base, state) = spawn_app(FakeLlm::new(vec![]), MockInject::default()).await;
    let client = reqwest::Client::new();

    state
        .config
        .save_device_meta("dot2-d", "客厅小达", "客厅", "test")
        .unwrap();
    state.store.secret_set("test_api_key", "sk-secret").unwrap();

    // Missing or wrong confirmation must be rejected without touching state.
    for payload in [
        json!({}),
        json!({ "confirm": "yes" }),
        json!({ "confirm": "factory_reset" }),
    ] {
        let r = client
            .post(format!("{base}/api/v0/system/factory-reset"))
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "must reject {payload}"
        );
    }
    assert_eq!(
        state
            .config
            .device_meta()
            .unwrap()
            .get("dot2-d")
            .map(|m| m.room.as_str()),
        Some("客厅")
    );
    assert_eq!(
        state.store.secret_get("test_api_key").unwrap().as_deref(),
        Some("sk-secret")
    );

    // Explicit FACTORY_RESET clears non-secret config/meta while preserving secrets.
    let r = client
        .post(format!("{base}/api/v0/system/factory-reset"))
        .json(&json!({ "confirm": "FACTORY_RESET" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::OK);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["state"], json!("reset"));
    assert_eq!(body["secrets_preserved"], json!(true));
    assert!(state.config.device_meta().unwrap().is_empty());
    assert_eq!(
        state.store.secret_get("test_api_key").unwrap().as_deref(),
        Some("sk-secret"),
        "secrets must be preserved unless include_secrets=true"
    );
}

#[tokio::test]
async fn factory_reset_clears_in_memory_disk_and_blocks_shared_memory_resurrection() {
    let isolated_dir = std::env::temp_dir().join(format!(
        "hm-factory-reset-test-{}-{}",
        std::process::id(),
        hm_core::id::now_ms()
    ));
    let (base, state) = spawn_app_at(
        FakeLlm::new(vec![]),
        MockInject::default(),
        isolated_dir.clone(),
    )
    .await;
    let client = reqwest::Client::new();

    // 1. Populate active task, completed task, and cross_turn state.
    let active_task = state.tasks.create("S1").unwrap();
    let completed_task = state.tasks.create("S1").unwrap();
    state
        .tasks
        .finish(&completed_task, hm_core::task::TaskStatus::Completed)
        .unwrap();
    state.tasks.begin_cross_turn(
        "S1",
        "先查天气再开灯",
        Some("开灯".into()),
        Some("已查天气".into()),
        hm_core::task::CrossTurnState::AwaitingContinue,
    );
    assert!(state.tasks.has_pending_cross_turn("S1"));

    // 2. Populate VoicePipeline in-memory settings, sovereignty, and disk dirs.
    let mut voice_cfg = hm_core::voice::VoiceSettings::default();
    voice_cfg.proactive_speech = true;
    state.voice.restore_settings(voice_cfg, "test").unwrap();
    state.voice.sovereignty.claim_or_renew(
        "living",
        "dot2-a",
        hm_core::id::now_ms(),
        9_000,
        60_000,
    );
    assert!(state
        .voice
        .sovereignty
        .owner_for_room("living", "dot2-a", hm_core::id::now_ms())
        .is_some());

    let wake_acks = state.data_dir.join("assets").join("wake-acks");
    let voice_events = state.data_dir.join("voice-events");
    std::fs::create_dir_all(&wake_acks).unwrap();
    std::fs::create_dir_all(&voice_events).unwrap();
    std::fs::write(wake_acks.join("ack.wav"), b"RIFF").unwrap();
    std::fs::write(voice_events.join("voice-events-20261117.jsonl"), b"{}\n").unwrap();

    // 3. Populate shared memory cache and outbox with pre-reset data.
    state
        .config
        .save_shared_memory(
            hm_core::config::SharedMemoryConfig {
                enabled: true,
                nas_base_url: "http://127.0.0.1:9999".into(),
                upload_enabled: true,
            },
            "test",
        )
        .unwrap();
    state
        .store
        .with(|c| {
            c.execute(
                "INSERT INTO shared_profile_cache (id, scope_person, scope_device, summary, updated_ms, fetched_ms)
                 VALUES ('sp-1', '', '', 'pre-reset secret preference', 1, 1)",
                [],
            )?;
            c.execute(
                "INSERT INTO shared_memory_outbox (id, device_id, room, person_id, trace_id, content, created_ms)
                 VALUES ('smo-1', 'dot2-a', 'living', '', '', 'pre-reset obs', 1)",
                [],
            )?;
            Ok(())
        })
        .unwrap();

    // 4. Execute factory reset.
    let (old_epoch, old_revision): (i64, i64) = state
        .store
        .with(|c| {
            Ok(c.query_row(
                "SELECT data_epoch, revision FROM config_head WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    let r = client
        .post(format!("{base}/api/v0/system/factory-reset"))
        .json(&json!({ "confirm": "FACTORY_RESET" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::OK);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["state"], json!("reset"));
    assert_eq!(body["secrets_preserved"], json!(true));
    assert_eq!(body["data_epoch"].as_i64(), Some(old_epoch + 1));
    assert!(body["revision"].as_i64().unwrap_or(0) > old_revision);

    // 5. Verify TaskRegistry, VoicePipeline, config_history, and disk cleanup.
    assert!(active_task.is_cancelled());
    assert_eq!(
        state.tasks.get(&active_task.id).unwrap().status(),
        hm_core::task::TaskStatus::Cancelled
    );
    assert!(state.tasks.get(&completed_task.id).is_none());
    assert!(state.tasks.get_cross_turn("S1").is_none());
    assert!(!state.tasks.has_pending_cross_turn("S1"));

    assert!(state
        .voice
        .sovereignty
        .owner_for_room("living", "dot2-a", hm_core::id::now_ms())
        .is_none());
    assert!(!state.voice.settings_snapshot().proactive_speech);
    assert_eq!(state.voice.mode_snapshot()["devices"], json!([]));
    let history_count: i64 = state
        .store
        .with(|c| Ok(c.query_row("SELECT COUNT(*) FROM config_history", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(
        history_count, 0,
        "factory_reset must not re-insert a voice row into config_history"
    );
    assert!(!wake_acks.exists());
    assert!(!voice_events.exists());

    // 6. Re-enabling shared memory must not re-inject pre-reset prompt data.
    state
        .config
        .save_shared_memory(
            hm_core::config::SharedMemoryConfig {
                enabled: true,
                nas_base_url: "http://127.0.0.1:9999".into(),
                upload_enabled: true,
            },
            "test",
        )
        .unwrap();
    assert!(state
        .shared_memory
        .render_for_prompt_scoped(&hm_core::working_memory::ScopeContext::default())
        .is_none());
    assert!(state.shared_memory.list_outbox(10).unwrap().is_empty());
}

#[tokio::test]
async fn config_export_with_secrets_requires_explicit_confirmation() {
    let (base, state) = spawn_app(FakeLlm::new(vec![]), MockInject::default()).await;
    let client = reqwest::Client::new();

    state
        .store
        .secret_set("llm.api_key", "sk-gated-secret")
        .unwrap();

    // GET with secrets must be rejected with 405 even when the header is present.
    let get_resp = client
        .get(format!("{base}/api/v0/config/export?with_secrets=true"))
        .header("X-HM-Confirm-Secrets", "EXPORT_SECRETS")
        .send()
        .await
        .unwrap();
    assert_eq!(get_resp.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
    let get_body: Value = get_resp.json().await.unwrap();
    assert_eq!(get_body["code"], json!("SECRET_EXPORT_POST_REQUIRED"));
    assert!(!get_body.to_string().contains("sk-gated-secret"));

    for query in [
        "with_secrets=true",
        "include_secrets=true",
        "secrets=1",
        "with_secrets=true&confirm=EXPORT_SECRETS",
    ] {
        let resp = client
            .post(format!("{base}/api/v0/config/export?{query}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::FORBIDDEN,
            "POST '{query}' without confirmation header must be rejected with 403"
        );
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["code"], json!("SECRET_EXPORT_CONFIRMATION_REQUIRED"));
        assert!(!body.to_string().contains("sk-gated-secret"));
    }

    // Confirmed via POST + header: succeeds and includes secret.
    let resp = client
        .post(format!("{base}/api/v0/config/export?with_secrets=true"))
        .header("X-HM-Confirm-Secrets", "EXPORT_SECRETS")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["secrets"]["llm.api_key"], json!("sk-gated-secret"));
}

#[tokio::test]
async fn per_unit_acoustic_and_led_profiles_are_independent_and_not_copied_on_import() {
    let (base, state) = spawn_app(FakeLlm::new(vec![]), MockInject::default()).await;
    let client = reqwest::Client::new();

    // Default read for two units.
    let d_init: Value = client
        .get(format!("{base}/api/v0/devices/dot2-d/profile"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(d_init["acoustic"]["volume_cap"], json!(1.0));
    assert_eq!(d_init["acoustic"]["mix_gain"], json!(1.0));
    assert_eq!(d_init["acoustic"]["vad_threshold"], json!(0.5));
    assert_eq!(d_init["led"]["night_enabled"], json!(false));
    assert_eq!(d_init["led"]["night_start_minute"], json!(1320));
    assert_eq!(d_init["led"]["night_end_minute"], json!(420));
    assert_eq!(d_init["led"]["night_brightness_pct"], json!(10));
    assert_eq!(d_init["led"]["orientation_rotation"], json!(0));
    assert_eq!(d_init["led"]["orientation_reverse"], json!(false));

    // Write distinct profiles for dot2-d and dot2-e.
    let put_d = client
        .put(format!("{base}/api/v0/devices/dot2-d/profile"))
        .json(&json!({
            "acoustic": { "volume_cap": 0.75, "mix_gain": 1.25, "vad_threshold": 0.45 },
            "led": {
                "night_enabled": true,
                "night_start_minute": 1380,
                "night_end_minute": 360,
                "night_brightness_pct": 15,
                "orientation_rotation": 3,
                "orientation_reverse": true
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(put_d.status(), reqwest::StatusCode::OK);

    let put_e = client
        .put(format!("{base}/api/v0/devices/dot2-e/profile"))
        .json(&json!({
            "acoustic": { "volume_cap": 0.60, "mix_gain": 0.85, "vad_threshold": 0.65 },
            "led": {
                "night_enabled": false,
                "night_start_minute": 1320,
                "night_end_minute": 420,
                "night_brightness_pct": 5,
                "orientation_rotation": 6,
                "orientation_reverse": false
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(put_e.status(), reqwest::StatusCode::OK);

    // Read back both and verify independence.
    let d_now: Value = client
        .get(format!("{base}/api/v0/devices/dot2-d/profile"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let e_now: Value = client
        .get(format!("{base}/api/v0/devices/dot2-e/profile"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(d_now["acoustic"]["volume_cap"], json!(0.75));
    assert_eq!(e_now["acoustic"]["volume_cap"], json!(0.60));
    assert_eq!(d_now["led"]["orientation_rotation"], json!(3));
    assert_eq!(e_now["led"]["orientation_rotation"], json!(6));
    assert_eq!(d_now["led"]["orientation_reverse"], json!(true));
    assert_eq!(e_now["led"]["orientation_reverse"], json!(false));

    // Export never includes unit_acoustic_profiles, and fleet/import ignores any injected ones.
    let exported: Value = client
        .get(format!("{base}/api/v0/config/export"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(exported.get("unit_acoustic_profiles").is_none());

    let mut malicious_push = json!({
        "format": "home-mate-config",
        "version": 1,
        "unit_acoustic_profiles": {
            "dot2-d": { "volume_cap": 0.10, "mix_gain": 0.10, "vad_threshold": 0.90 },
            "dot2-e": { "volume_cap": 0.10, "mix_gain": 0.10, "vad_threshold": 0.90 }
        }
    });
    let import_resp: Value = client
        .post(format!("{base}/api/v0/config/import"))
        .header("x-home-mate-source", "dot2-other")
        .json(&malicious_push)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        import_resp["warnings"].as_array().unwrap().iter().any(|w| w
            .as_str()
            .unwrap_or_default()
            .contains("unit_acoustic_profiles")),
        "import must warn and ignore unit_acoustic_profiles: {import_resp}"
    );
    hm_core::fleet::strip_node_local(&mut malicious_push);
    assert!(malicious_push.get("unit_acoustic_profiles").is_none());

    // Profiles on this core remain untouched.
    assert_eq!(
        state
            .config
            .unit_acoustic_profile("dot2-d")
            .unwrap()
            .volume_cap,
        0.75
    );
    assert_eq!(
        state
            .config
            .unit_acoustic_profile("dot2-e")
            .unwrap()
            .volume_cap,
        0.60
    );
}
