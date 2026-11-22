//! Household memory and conversation clearing (2026-11-13).
//!
//! Two user rules are pinned here:
//! 1. Memory is short, user-facing and console-owned: add / edit / pin / delete
//!    / clear all work through the management API, and a memory reaches the
//!    prompt (as background, not as a capability).
//! 2. A conversation is not a source of truth about configuration. Clearing it
//!    removes its messages, and an availability change (a service becoming
//!    usable) clears the DEVICE conversations so the assistant cannot replay
//!    its own older "it is not connected" claims.
use futures::StreamExt;
use hm_core::api::{router, AppState};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::tool::mocks::MockInject;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

fn paired_client(token: &str) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

async fn spawn_app(llm: FakeLlm) -> (String, Arc<AppState>, String) {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let state = AppState::with_overrides(
        store,
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm)),
        None,
        None,
        None,
    );
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let token = state.auth.provision("memory-fixture").unwrap();
    (format!("http://{addr}"), state, token)
}

async fn chat(base: &str, token: &str, session: Option<&str>, text: &str) -> Vec<Value> {
    chat_scoped(base, token, session, text, None, None).await
}

async fn chat_scoped(
    base: &str,
    token: &str,
    session: Option<&str>,
    text: &str,
    person_id: Option<&str>,
    device_id: Option<&str>,
) -> Vec<Value> {
    let mut body = json!({ "text": text });
    if let Some(sid) = session {
        body["session_id"] = json!(sid);
    }
    if person_id.is_some() || device_id.is_some() {
        assert!(session.is_none(), "scope selection creates its own session");
        let selected = select_scope(base, token, person_id, device_id).await;
        body["scope_binding_id"] = selected["binding_id"].clone();
    }
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v0/chat"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
    let mut bytes = resp.bytes_stream();
    let mut buf = String::new();
    let mut events = Vec::new();
    while let Some(chunk) = bytes.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        while let Some(pos) = buf.find('\n') {
            let line: String = buf.drain(..=pos).collect();
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

// Seed a physical conversation through the trusted Agent entry, never by
// claiming a reserved device session from the management text HTTP route.
async fn physical_chat(state: &AppState, device: &str, text: &str) {
    let session = state.sessions.device_session_id(device).unwrap();
    let events: Vec<_> = hm_core::agent::run_turn_with_scope(
        state.agent_env().unwrap(),
        Some(session),
        text.into(),
        "voice".into(),
        hm_core::working_memory::ScopeContext {
            person_id: None,
            device_id: Some(device.into()),
            room: None,
        },
    )
    .collect()
    .await;
    assert!(events.iter().any(|event| matches!(event,
        hm_core::agent::AgentEvent::TurnEnd { status, .. } if status == "completed")));
    assert!(!events
        .iter()
        .any(|event| matches!(event, hm_core::agent::AgentEvent::Error { .. })));
}

async fn select_scope(
    base: &str,
    token: &str,
    person: Option<&str>,
    device: Option<&str>,
) -> Value {
    let response = reqwest::Client::new()
        .post(format!("{base}/api/v1/chat/scopes"))
        .bearer_auth(token)
        .json(&json!({"person_id": person, "device_id": device, "household_write": false}))
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
    response.json().await.unwrap()
}

#[tokio::test]
async fn memory_api_is_crud_and_reaches_the_prompt_as_background() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["好。".into()],
    }]);
    let (base, state, _token) = spawn_app(llm.clone()).await;
    let c = paired_client(&_token);

    // Empty store: no block, just the list.
    let list: Value = c
        .get(format!("{base}/api/v0/memories"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["count"], 0, "{list}");
    assert_eq!(list["max"], 200);

    let added: Value = c
        .post(format!("{base}/api/v0/memories"))
        .json(&json!({ "text": "  奶奶周日来吃饭  ", "kind": "event", "pinned": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = added["item"]["id"].as_str().unwrap().to_string();
    assert_eq!(added["item"]["text"], "奶奶周日来吃饭");

    // Bounds are enforced with a 400, not silently truncated.
    let bad = c
        .post(format!("{base}/api/v0/memories"))
        .json(&json!({ "text": "长".repeat(201) }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);

    let edited: Value = c
        .put(format!("{base}/api/v0/memories/{id}"))
        .json(&json!({ "text": "奶奶每周日来吃饭", "pinned": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(edited["item"]["text"], "奶奶每周日来吃饭");

    // The prompt receives it as background next to the capability list.
    let _ = chat(&base, &_token, None, "随便说点什么").await;
    let reqs = llm.seen_requests();
    let system = reqs
        .last()
        .unwrap()
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.clone())
        .unwrap();
    assert!(system.contains("关于这个家"), "{system}");
    assert!(system.contains("奶奶每周日来吃饭"), "{system}");
    assert!(
        system.contains("不要把它当作配置或工具状态的来源"),
        "memory is background, never a capability source: {system}"
    );

    let del: Value = c
        .delete(format!("{base}/api/v0/memories/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(del["state"], "deleted");
    let cleared: Value = c
        .post(format!("{base}/api/v0/memories/clear"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["state"], "cleared");
    assert!(state.memory.list().unwrap().is_empty());
}

#[tokio::test]
async fn the_remember_tool_preserves_management_origin() {
    let llm = FakeLlm::new(vec![]);
    let (base, state, _token) = spawn_app(llm).await;
    let selected = select_scope(&base, &_token, None, Some("dot2-d")).await;
    let principal = state.execution.principal(&_token).unwrap();
    let (authority, _) = state
        .execution
        .resolve(selected["binding_id"].as_str().unwrap(), &principal, None)
        .unwrap();
    let ctx = hm_core::tool::ToolCtx::new(
        "T1",
        "C1",
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .with_authority(authority);
    let r = state
        .registry
        .call(
            "memory.remember",
            json!({ "text": "我不喝冰的", "kind": "preference" }),
            &ctx,
        )
        .await;
    assert_eq!(r.status, hm_core::tool::ToolStatus::Completed);
    let items = state.memory.list().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].source, "console");
    assert_eq!(items[0].kind, "preference");
    // An empty note is a named failure the model can relay.
    let bad = state
        .registry
        .call("memory.remember", json!({ "text": "  " }), &ctx)
        .await;
    assert_eq!(bad.status, hm_core::tool::ToolStatus::Failed);
    assert_eq!(bad.error.unwrap().code, "MEMORY_REJECTED");
}

#[tokio::test]
async fn clearing_a_session_removes_its_messages() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["好。".into()],
    }]);
    let (base, state, _token) = spawn_app(llm).await;
    physical_chat(&state, "dot2-d", "随便说点什么").await;
    assert!(!state.sessions.messages("dev:dot2-d").unwrap().is_empty());
    let cleared: Value = paired_client(&_token)
        .post(format!("{base}/api/v0/session/dev:dot2-d/clear"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(cleared["cleared"].as_u64().unwrap() >= 1, "{cleared}");
    assert!(state.sessions.messages("dev:dot2-d").unwrap().is_empty());
}

/// The real-HA failure this exists for: after the household configured Home
/// Assistant, the assistant kept replaying "家庭灯光后端还没有接入" from its
/// own older messages. A service becoming usable clears the DEVICE
/// conversation; console/text sessions are untouched.
#[tokio::test]
async fn a_service_becoming_usable_clears_the_device_conversation() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["好。".into()],
        },
        Scripted::Text {
            chunks: vec!["好。".into()],
        },
    ]);
    let (base, state, _token) = spawn_app(llm).await;

    state.sessions.ensure("text:user").unwrap();
    physical_chat(&state, "dot2-d", "打不开灯吗").await;
    let _ = chat(&base, &_token, Some("text:user"), "打不开灯吗").await;
    assert!(!state.sessions.messages("dev:dot2-d").unwrap().is_empty());
    assert!(!state.sessions.messages("text:user").unwrap().is_empty());

    // Configure home through the same API the console uses. This makes
    // home.set usable, so the ready-tool set changes.
    let resp = paired_client(&_token)
        .put(format!("{base}/api/v0/config/services/home"))
        .json(&json!({
            "backend": "home_assistant",
            "base_url": "http://127.0.0.1:9",
            "token": "test-token",
            "entities": [{ "name": "儿童房主灯", "entity": "light.children" }]
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{:?}", resp.text().await);

    assert!(
        state.sessions.messages("dev:dot2-d").unwrap().is_empty(),
        "the device conversation must not keep pre-configuration claims"
    );
    assert!(
        !state.sessions.messages("text:user").unwrap().is_empty(),
        "console/text sessions are the user's workspace: untouched"
    );
}

/// Codex review (2026-11-13): the ready-tool set alone misses an entity
/// DIRECTORY change — adding a light keeps `home.set` ready — so the old
/// conversation would survive a whitelist edit and keep replaying "没有这盏灯".
#[tokio::test]
async fn a_whitelist_change_also_clears_the_device_conversation() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["好。".into()],
    }]);
    let (base, state, _token) = spawn_app(llm).await;
    let c = paired_client(&_token);
    let put = |entities: Value| {
        let c = c.clone();
        let base = base.clone();
        async move {
            c.put(format!("{base}/api/v0/config/services/home"))
                .json(&json!({
                    "backend": "home_assistant",
                    "base_url": "http://127.0.0.1:9",
                    "token": "test-token",
                    "entities": entities
                }))
                .send()
                .await
                .unwrap()
        }
    };
    // First save: unconfigured -> ready (the tool set changes).
    let r = put(json!([{ "name": "儿童房主灯", "entity": "light.a" }])).await;
    assert!(r.status().is_success(), "{:?}", r.text().await);
    physical_chat(&state, "dot2-d", "打开儿童房主灯").await;
    assert!(!state.sessions.messages("dev:dot2-d").unwrap().is_empty());

    // Second save: the tool set is the SAME, only the directory grew.
    let r = put(json!([
        { "name": "儿童房主灯", "entity": "light.a" },
        { "name": "儿童房灯组", "entity": "light.b" }
    ]))
    .await;
    assert!(r.status().is_success(), "{:?}", r.text().await);
    assert!(
        state.sessions.messages("dev:dot2-d").unwrap().is_empty(),
        "an entity-directory change must also invalidate the conversation"
    );
}

// --- M7-1 (2026-11-13): auto-extracted memory candidates ---------------------

const EXTRACT_ONE: &str = r#"{"candidates":[
  {"text":"奶奶周日来吃饭","kind":"event","reason":"家庭事件"},
  {"text":"当前音量是 60","kind":"fact","reason":"运行状态"}
]}"#;

/// The extractor runs only when a model service is CONFIGURED (it is two
/// real calls). These tests drive a FakeLlm through the override seam, so the
/// configuration entry exists but never reaches the network.
fn configure_fake_llm(state: &AppState) {
    hm_core::config::ConfigService::new(state.store.clone())
        .save_service(
            "llm",
            hm_core::config::ServiceConfig {
                provider: "fake".into(),
                model: "fake".into(),
                ..Default::default()
            },
            None,
            "test",
        )
        .unwrap();
}

/// The extractor may PROPOSE; only the background confirmation writes. This
/// test watches the window between the two calls: the candidate is already
/// pending, the memory store is still empty. Then the confirmer accepts and
/// the note is written exactly once.
#[tokio::test]
async fn auto_memory_is_written_only_after_the_background_confirmation() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec![EXTRACT_ONE.into()],
        },
        // The delay creates an observable "pending but not written" window.
        Scripted::Delayed {
            delay_ms: 400,
            chunks: vec![r#"{"decisions":[{"index":0,"ok":true,"reason":"真实家庭事件"}]}"#.into()],
        },
    ]);
    let (_base, state, _token) = spawn_app(llm).await;
    configure_fake_llm(&state);
    state.voice.set_memory_extraction(true);
    let physical_session = state.sessions.device_session_id("dot2-d").unwrap();
    state
        .sessions
        .append(&physical_session, "user", "我奶奶周日来吃饭")
        .unwrap();
    state
        .sessions
        .append(&physical_session, "assistant", "好的，我记住了。")
        .unwrap();

    state.voice.start_memory_extraction("dot2-d", "test");

    // Between the calls: one pending candidate (the operational one was
    // refused by the deterministic gate), and NO memory yet.
    let mut saw_pending = false;
    for _ in 0..100 {
        let pending = state.memory.candidates(Some("pending")).unwrap();
        if !pending.is_empty() {
            saw_pending = true;
            assert_eq!(
                pending.len(),
                1,
                "the operational candidate must not be proposed: {pending:?}"
            );
            assert_eq!(pending[0].text, "奶奶周日来吃饭");
            assert_eq!(pending[0].session_id, physical_session);
            assert_eq!(pending[0].device, "dot2-d");
            assert!(
                state.memory.list().unwrap().is_empty(),
                "a pending candidate is NOT a memory"
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        saw_pending,
        "the extractor never proposed anything: {:?}",
        state.voice.recent(30)
    );

    // After the confirmation: confirmed, and the exact text is in memory with
    // source=auto (one row, not duplicated).
    let mut written = None;
    for _ in 0..150 {
        let list = state.memory.list().unwrap();
        if !list.is_empty() {
            written = Some(list);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let list = written.expect("the confirmed candidate must become a memory");
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0].text, "奶奶周日来吃饭");
    assert_eq!(
        list[0].source, "auto",
        "a background note is not a console note"
    );
    assert!(state.memory.candidates(Some("confirmed")).unwrap().len() == 1);
}

/// The confirmer can refuse; then NOTHING is written and the candidate is
/// kept as `rejected` with the reason (audit, not silence).
#[tokio::test]
async fn a_refused_candidate_never_becomes_a_memory() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec![
                r#"{"candidates":[{"text":"他好像喜欢蓝色","kind":"preference","reason":"猜测"}]}"#
                    .into(),
            ],
        },
        Scripted::Text {
            chunks: vec![
                r#"{"decisions":[{"index":0,"ok":false,"reason":"推测，不是事实"}]}"#.into(),
            ],
        },
    ]);
    let (_base, state, _token) = spawn_app(llm).await;
    configure_fake_llm(&state);
    state.voice.set_memory_extraction(true);
    let physical_session = state.sessions.device_session_id("dot2-d").unwrap();
    state
        .sessions
        .append(&physical_session, "user", "也许他喜欢蓝色吧")
        .unwrap();

    state.voice.start_memory_extraction("dot2-d", "test");
    let mut rejected = Vec::new();
    for _ in 0..150 {
        rejected = state.memory.candidates(Some("rejected")).unwrap();
        if !rejected.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert!(
        rejected[0].reason.contains("推测"),
        "the reason is kept: {rejected:?}"
    );
    assert!(
        state.memory.list().unwrap().is_empty(),
        "nothing may be written"
    );
    let ev = state
        .voice
        .recent(200)
        .into_iter()
        .find(|e| e["kind"] == "memory_extract")
        .expect("the extraction result is recorded");
    assert_eq!(ev["written"], json!(0));
    assert_eq!(ev["proposed"], json!(1));
}

/// The console endpoints are the human override: a pending candidate can be
/// adopted, ignored or deleted without ever touching the model again.
#[tokio::test]
async fn the_console_can_adopt_or_ignore_a_pending_candidate() {
    let llm = FakeLlm::new(vec![]);
    let (base, state, _token) = spawn_app(llm).await;
    let c = state
        .memory
        .add_candidate(
            "主卧的窗帘叫主卧窗帘",
            "alias",
            "dot2-d",
            "dev:dot2-d",
            "test",
            "",
        )
        .unwrap();
    let c2 = state
        .memory
        .add_candidate(
            "我不喝冰的",
            "preference",
            "dot2-d",
            "dev:dot2-d",
            "test",
            "",
        )
        .unwrap();
    let list: Value = paired_client(&_token)
        .get(format!("{base}/api/v0/memories/candidates?status=pending"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["count"], json!(2));

    let adopted: Value = paired_client(&_token)
        .post(format!(
            "{base}/api/v0/memories/candidates/{}/confirm",
            c.id
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(adopted["state"], json!("applied"));
    assert_eq!(adopted["item"]["text"], json!("主卧的窗帘叫主卧窗帘"));

    let ignored: Value = paired_client(&_token)
        .post(format!(
            "{base}/api/v0/memories/candidates/{}/reject",
            c2.id
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ignored["state"], json!("rejected"));
    assert_eq!(
        state.memory.list().unwrap().len(),
        1,
        "only the adopted one is a memory"
    );

    let gone: Value = paired_client(&_token)
        .delete(format!("{base}/api/v0/memories/candidates/{}", c2.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(gone["state"], json!("deleted"));
}

#[tokio::test]
async fn working_memory_persists_across_sessions_and_is_recalled_in_new_session() {
    let work_detail = format!(
        "仓库在 /Users/dev/home-mate-b，当前做到 Phase 7 M6 长期工作记忆，已完成 SQLite v7 迁移与单元测试。{}",
        "补充上下文细节。".repeat(20)
    );
    assert!(work_detail.chars().count() > 200);

    let llm = FakeLlm::new(vec![
        // Session 1: assistant writes working memory via tool call.
        Scripted::Calls {
            calls: vec![(
                "workmem.set".into(),
                json!({
                    "key_or_topic": "home_mate_m6",
                    "content": work_detail,
                    "scope_person": "alice",
                    "scope_device": "dot2-a"
                })
                .to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["已记下你的工作进度。".into()],
        },
        // Session 2 (brand new session): assistant recalls from injected working memory.
        Scripted::Text {
            chunks: vec![
                "你的项目在 /Users/dev/home-mate-b，上次做到 Phase 7 M6 长期工作记忆。".into(),
            ],
        },
    ]);
    let (base, state, _token) = spawn_app(llm.clone()).await;

    // Session 1
    let ev1 = chat_scoped(
        &base,
        &_token,
        None,
        "记住我手头的项目进度",
        Some("alice"),
        Some("dot2-a"),
    )
    .await;
    let sid1 = ev1
        .iter()
        .find_map(|e| {
            (e["type"] == "session")
                .then(|| e["session_id"].as_str().map(str::to_string))
                .flatten()
        })
        .expect("session 1 id");
    assert!(
        state.memory.list().unwrap().is_empty(),
        "working memory must not pollute 200-item household notes"
    );
    let wm_items = state
        .working_memory
        .list(Some("alice"), Some("dot2-a"))
        .unwrap();
    assert_eq!(wm_items.len(), 1);
    assert_eq!(wm_items[0].key_or_topic, "home_mate_m6");

    // Session 2: brand new session (session_id = None), explicit scope for alice + dot2-a.
    let ev2 = chat_scoped(
        &base,
        &_token,
        None,
        "上次那个项目在哪、做到哪了？",
        Some("alice"),
        Some("dot2-a"),
    )
    .await;
    let sid2 = ev2
        .iter()
        .find_map(|e| {
            (e["type"] == "session")
                .then(|| e["session_id"].as_str().map(str::to_string))
                .flatten()
        })
        .expect("session 2 id");
    assert_ne!(sid1, sid2, "must be a brand-new session");

    let reqs = llm.seen_requests();
    let last_system = reqs
        .last()
        .unwrap()
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.clone())
        .unwrap();
    assert!(
        last_system.contains("工作记忆")
            && last_system.contains("home_mate_m6")
            && last_system.contains("/Users/dev/home-mate-b"),
        "new session system prompt must inject working memory: {last_system}"
    );
    let answer = ev2
        .iter()
        .find_map(|e| {
            (e["type"] == "answer")
                .then(|| e["text"].as_str().map(str::to_string))
                .flatten()
        })
        .expect("session 2 answer");
    assert!(answer.contains("/Users/dev/home-mate-b"), "{answer}");
}

#[tokio::test]
async fn working_memory_api_crud_and_household_note_isolation() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["好的。".into()],
    }]);
    let (base, state, _token) = spawn_app(llm.clone()).await;
    let c = paired_client(&_token);

    // PUT working memory via HTTP API (> 200 chars, scoped per-person & per-device).
    let long_ctx = "工作上下文：".to_string() + &"详情".repeat(150);
    let put_resp: Value = c
        .put(format!("{base}/api/v0/working-memory"))
        .json(&json!({
            "scope_person": "bob",
            "scope_device": "dot2-b",
            "key_or_topic": "deploy_plan",
            "content": long_ctx
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put_resp["state"], json!("applied"));
    let id_b = put_resp["item"]["id"].as_str().unwrap().to_string();

    c.put(format!("{base}/api/v0/working-memory"))
        .json(&json!({
            "scope_person": "",
            "scope_device": "dot2-a",
            "key_or_topic": "audio_calib",
            "content": "dot2-a 正在校准麦克风阵列"
        }))
        .send()
        .await
        .unwrap();

    // GET filtered by scope_device.
    let list_a: Value = c
        .get(format!("{base}/api/v0/working-memory?scope_device=dot2-a"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list_a["count"], json!(1));
    assert_eq!(list_a["items"][0]["key_or_topic"], json!("audio_calib"));

    // Household notes remain strictly isolated: operational state is still rejected.
    for op in ["当前音量是 60%", "正在播放《夜曲》", "天气服务已配置好"] {
        assert!(
            state
                .memory
                .add_candidate(op, "fact", "dot2-a", "dev:dot2-a", "test", "")
                .is_err(),
            "operational text '{op}' must never enter household notes"
        );
    }
    assert!(state.memory.list().unwrap().is_empty());

    // Explicitly selected device scope sees its memory, not dot2-b's.
    let _ = chat_scoped(&base, &_token, None, "继续", None, Some("dot2-a")).await;
    let reqs = llm.seen_requests();
    let system = reqs
        .last()
        .unwrap()
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.clone())
        .unwrap();
    assert!(system.contains("dot2-a 正在校准麦克风阵列"), "{system}");
    assert!(
        !system.contains("deploy_plan"),
        "dot2-b working memory must not leak to dot2-a: {system}"
    );

    // DELETE by id and by key, plus clear.
    let del_id: Value = c
        .delete(format!("{base}/api/v0/working-memory/{id_b}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(del_id["state"], json!("deleted"));

    let del_key: Value = c
        .delete(format!(
            "{base}/api/v0/working-memory?scope_device=dot2-a&key_or_topic=audio_calib"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(del_key["state"], json!("deleted"));

    let cleared: Value = c
        .post(format!("{base}/api/v0/working-memory/clear"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["state"], json!("cleared"));
}

#[tokio::test]
async fn workmem_set_is_mutate_and_records_provenance() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "workmem.set".into(),
                json!({
                    "key_or_topic": "prov_test",
                    "content": "provenance check",
                    "scope_person": "alice",
                    "scope_device": "dot2-a"
                })
                .to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["已记录。".into()],
        },
    ]);
    let (base, state, _token) = spawn_app(llm).await;

    use hm_core::tool::Tool;
    let desc =
        hm_core::tool::memory::WorkingMemorySetTool::new(state.working_memory.clone()).descriptor();
    assert_eq!(desc.name, "workmem.set");
    assert_eq!(desc.side_effect, hm_core::tool::SideEffect::Mutate);

    let _ = chat_scoped(
        &base,
        &_token,
        None,
        "记一下",
        Some("alice"),
        Some("dot2-a"),
    )
    .await;
    let item = state
        .working_memory
        .get("alice", "dot2-a", "prov_test")
        .unwrap()
        .expect("saved item");
    // Selected visibility is not physical device ingress provenance.
    assert_eq!(item.written_by_device, "");
    assert!(!item.source_trace_id.is_empty());
}

#[tokio::test]
async fn working_memory_pin_api_and_privacy_isolation_in_chat() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["匿名回复。".into()],
        },
        Scripted::Text {
            chunks: vec!["Bob回复。".into()],
        },
        Scripted::Text {
            chunks: vec!["Alice回复。".into()],
        },
    ]);
    let (base, state, _token) = spawn_app(llm.clone()).await;
    let c = paired_client(&_token);

    state
        .working_memory
        .set("", "", "global_k", "global_secret_val")
        .unwrap();
    let alice_item = state
        .working_memory
        .set("alice", "", "alice_k", "alice_private_val")
        .unwrap();
    state
        .working_memory
        .set("", "dot2-a", "dev_k", "dot2_a_only_val")
        .unwrap();

    // Pin via API.
    let pin_resp: Value = c
        .post(format!(
            "{base}/api/v0/working-memory/{}/pin",
            alice_item.id
        ))
        .json(&json!({ "pinned": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pin_resp["state"], json!("applied"));
    assert_eq!(pin_resp["item"]["pinned"], json!(true));

    // Unscoped chat: must see global only, NEVER alice or dot2-a.
    let _ = chat(&base, &_token, None, "你好").await;
    // Bob chat: must see global only, NEVER alice.
    let _ = chat_scoped(&base, &_token, None, "你好", Some("bob"), None).await;
    // Alice on dot2-a: sees all three.
    let _ = chat_scoped(&base, &_token, None, "你好", Some("alice"), Some("dot2-a")).await;

    let reqs = llm.seen_requests();
    let sys_anon = reqs[0]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap();
    let sys_bob = reqs[1]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap();
    let sys_alice = reqs[2]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap();

    assert!(sys_anon.contains("global_secret_val"));
    assert!(!sys_anon.contains("alice_private_val"));
    assert!(!sys_anon.contains("dot2_a_only_val"));

    assert!(sys_bob.contains("global_secret_val"));
    assert!(!sys_bob.contains("alice_private_val"));

    assert!(sys_alice.contains("global_secret_val"));
    assert!(sys_alice.contains("alice_private_val"));
    assert!(sys_alice.contains("dot2_a_only_val"));
}

#[tokio::test]
async fn shared_memory_circuit_breaker_and_cache_injection() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["ok1".into()],
        },
        Scripted::Text {
            chunks: vec!["ok2".into()],
        },
    ]);
    let (base, state, _token) = spawn_app(llm.clone()).await;
    let c = paired_client(&_token);

    // Point NAS to an unreachable local port (closed socket -> immediate ECONNREFUSED, <= 250ms).
    let cfg_resp: Value = c
        .put(format!("{base}/api/v0/shared-memory/config"))
        .json(&json!({
            "enabled": true,
            "nas_base_url": "http://127.0.0.1:1",
            "upload_enabled": true
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cfg_resp["state"], json!("applied"));

    state
        .shared_memory
        .enqueue_observation("dot2-a", "living-room", "alice", "T1", "obs 1")
        .unwrap();

    let outcome = state.shared_memory.sync_once().await;
    assert!(
        matches!(
            outcome,
            hm_core::shared_memory::SyncOutcome::Failed(_)
                | hm_core::shared_memory::SyncOutcome::CircuitOpen
        ),
        "expected Failed or CircuitOpen (if background sync tripped first), got {outcome:?}"
    );
    assert!(state.shared_memory.is_circuit_open());

    // Second sync_once immediately short-circuits with CircuitOpen.
    let outcome2 = state.shared_memory.sync_once().await;
    assert_eq!(outcome2, hm_core::shared_memory::SyncOutcome::CircuitOpen);

    // Seed local shared_profile_cache: live turn injects scoped summary with 0 ms blocking.
    state
        .shared_memory
        .upsert_profile_cache("p-global", "", "", "家庭偏好：周末早起", 100)
        .unwrap();
    state
        .shared_memory
        .upsert_profile_cache("p-alice", "alice", "", "Alice 正在准备 Rust 重构", 200)
        .unwrap();

    let started = std::time::Instant::now();
    let _ = chat(&base, &_token, None, "早上好").await;
    let _ = chat_scoped(&base, &_token, None, "继续", Some("alice"), Some("dot2-a")).await;
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    let reqs = llm.seen_requests();
    let sys_anon = reqs[0]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap();
    let sys_alice = reqs[1]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap();

    assert!(sys_anon.contains("家庭偏好：周末早起"));
    assert!(!sys_anon.contains("Alice 正在准备 Rust 重构"));
    assert!(sys_alice.contains("Alice 正在准备 Rust 重构"));

    // Status & clear-outbox endpoints.
    let status: Value = c
        .get(format!("{base}/api/v0/shared-memory/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["backlog_count"], json!(1));
    assert_eq!(status["circuit_open"], json!(true));

    let cleared: Value = c
        .post(format!("{base}/api/v0/shared-memory/clear-outbox"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["deleted"], json!(1));
}

#[tokio::test]
async fn shared_memory_held_sync_across_reset_never_repopulates_cache() {
    use axum::{extract::State, routing::get, Json, Router};
    use tokio::sync::{mpsc, oneshot};

    let (call_tx, mut calls) = mpsc::unbounded_channel::<oneshot::Sender<Value>>();
    let nas_app = Router::new()
        .route(
            "/api/v1/profiles",
            get(
                |State(tx): State<mpsc::UnboundedSender<oneshot::Sender<Value>>>| async move {
                    let (release, wait) = oneshot::channel();
                    let _ = tx.send(release);
                    Json(wait.await.unwrap_or_else(|_| json!({ "profiles": [] })))
                },
            ),
        )
        .with_state(call_tx);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nas_base = format!("http://{}", listener.local_addr().unwrap());
    let nas_task = tokio::spawn(async move {
        let _ = axum::serve(listener, nas_app).await;
    });

    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["Synthetic reply.".into()],
    }]);
    let (base, state, token) = spawn_app(llm.clone()).await;
    let c = paired_client(&token);

    // PUT /api/v0/shared-memory/config automatically spawns a background sync_once().
    let cfg_resp = c
        .put(format!("{base}/api/v0/shared-memory/config"))
        .json(&json!({
            "enabled": true,
            "nas_base_url": nas_base,
            "upload_enabled": false
        }))
        .send()
        .await
        .unwrap();
    assert!(cfg_resp.status().is_success());

    // Hold the auto-triggered /api/v1/profiles request across factory-reset.
    let release = tokio::time::timeout(std::time::Duration::from_secs(2), calls.recv())
        .await
        .unwrap()
        .unwrap();

    let reset_resp = c
        .post(format!("{base}/api/v0/system/factory-reset"))
        .json(&json!({ "confirm": "FACTORY_RESET", "include_secrets": false }))
        .send()
        .await
        .unwrap();
    assert!(reset_resp.status().is_success());
    assert_eq!(state.shared_memory.cache_count().unwrap(), 0);

    // Release the stale pre-reset response well within the 250ms HTTP budget.
    let _ = release.send(json!({
        "profiles": [{
            "id": "same-profile",
            "scope_person": "",
            "scope_device": "",
            "summary": "OLD_RESET_MARKER",
            "updated_ms": 1
        }]
    }));
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;

    assert_eq!(state.shared_memory.cache_count().unwrap(), 0);
    // factory-reset clears auth tokens too; provision a fresh one for post-reset chat.
    let new_tok: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/auth/provision"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fresh_token = new_tok["token"].as_str().unwrap().to_string();
    let _ = chat(&base, &fresh_token, None, "Synthetic post-reset question").await;
    let reqs = llm.seen_requests();
    assert!(!reqs.is_empty());
    let leaked = serde_json::to_string(&reqs[0])
        .unwrap()
        .contains("OLD_RESET_MARKER");
    assert!(
        !leaked,
        "stale pre-reset NAS profile leaked into post-reset LLM prompt"
    );
    nas_task.abort();
}

#[tokio::test]
async fn b03_runtime_producers_enqueue_and_respect_upload_enabled_policy() {
    use hm_core::shared_memory::{ObservationInput, OUTBOX_MAX_BYTES, OUTBOX_MAX_ENTRIES};

    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "workmem.set".into(),
                json!({"key_or_topic": "proj", "content": "off-upload work"}).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["Saved locally.".into()],
        },
        Scripted::Calls {
            calls: vec![(
                "workmem.set".into(),
                json!({"key_or_topic": "proj", "content": "on-upload work"}).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["Saved and queued.".into()],
        },
        Scripted::Calls {
            calls: vec![(
                "user_profile.propose".into(),
                json!({"key": "tea", "content": "prefers oolong", "rationale": "user stated"})
                    .to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["Profile proposed.".into()],
        },
    ]);
    let (base, state, token) = spawn_app(llm).await;
    let c = paired_client(&token);

    // 1. When upload_enabled == false, producers save locally but do NOT accumulate outbox rows.
    let cfg_off: Value = c
        .put(format!("{base}/api/v0/shared-memory/config"))
        .json(&json!({
            "enabled": true,
            "nas_base_url": "http://127.0.0.1:1",
            "upload_enabled": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cfg_off["state"], json!("applied"));

    let _ = chat_scoped(
        &base,
        &token,
        None,
        "remember proj",
        Some("alice"),
        Some("dot2-a"),
    )
    .await;
    assert_eq!(state.shared_memory.backlog_count().unwrap(), 0);
    assert_eq!(state.working_memory.list(None, None).unwrap().len(), 1);
    assert_eq!(
        state
            .shared_memory
            .enqueue_observation("dot2-a", "living", "alice", "T0", "should skip")
            .unwrap(),
        None
    );
    assert_eq!(state.shared_memory.backlog_count().unwrap(), 0);

    // 2. Enable upload_enabled == true -> workmem.set & user_profile.propose enqueue with provenance + scope.
    let _ = c
        .put(format!("{base}/api/v0/shared-memory/config"))
        .json(&json!({
            "enabled": true,
            "nas_base_url": "http://127.0.0.1:1",
            "upload_enabled": true
        }))
        .send()
        .await
        .unwrap();

    let _ = chat_scoped(
        &base,
        &token,
        None,
        "remember proj 2",
        Some("alice"),
        Some("dot2-a"),
    )
    .await;
    let _ = chat_scoped(
        &base,
        &token,
        None,
        "propose tea",
        Some("alice"),
        Some("dot2-a"),
    )
    .await;

    let outbox = state.shared_memory.list_outbox(50).unwrap();
    assert_eq!(outbox.len(), 2);
    assert_eq!(outbox[0].domain, "workmem");
    assert_eq!(outbox[0].source_device, "console");
    assert_eq!(outbox[0].scope_device, "dot2-a");
    assert_eq!(outbox[0].scope_person, "alice");
    assert_eq!(outbox[0].event_kind, "explicit_save");

    assert_eq!(outbox[1].domain, "user_md");
    assert_eq!(outbox[1].source_device, "console");
    assert_eq!(outbox[1].scope_device, "dot2-a");
    assert_eq!(outbox[1].scope_person, "alice");

    // 3. data_epoch mismatch is rejected cleanly.
    let err = state
        .shared_memory
        .enqueue_observation_scoped(ObservationInput {
            source_device: "dot2-a",
            scope_device: "",
            scope_person: "",
            room: "",
            trace_id: "T-bad-epoch",
            content: "stale epoch",
            domain: "notes",
            record_id: "",
            event_kind: "explicit_save",
            dimension: "general",
            expected_epoch: Some(999_999),
        })
        .unwrap_err();
    assert!(err.contains("data_epoch"), "unexpected error: {err}");

    // 4. Outbox bounds (bytes / entries) prevent unbounded growth.
    state.shared_memory.clear_outbox().unwrap();
    let big = "x".repeat(4000);
    let max_by_bytes = OUTBOX_MAX_BYTES / big.len();
    for i in 0..max_by_bytes.min(OUTBOX_MAX_ENTRIES) {
        state
            .shared_memory
            .enqueue_observation("dot2-a", "", "", &format!("T-{i}"), &big)
            .unwrap();
    }
    let full_err = state
        .shared_memory
        .enqueue_observation("dot2-a", "", "", "T-overflow", &big)
        .unwrap_err();
    assert!(
        full_err.contains("capacity"),
        "unexpected error: {full_err}"
    );
}

#[tokio::test]
async fn b04_source_device_vs_scope_device_and_scope_person_isolation() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["r1".into()],
        },
        Scripted::Text {
            chunks: vec!["r2".into()],
        },
        Scripted::Text {
            chunks: vec!["r3".into()],
        },
    ]);
    let (base, state, token) = spawn_app(llm.clone()).await;

    // All 4 profiles originated from `dot2-a` (`source_device = "dot2-a"`),
    // but have distinct visibility scopes (`scope_person`, `scope_device`).
    state
        .shared_memory
        .upsert_profile_cache_with_source("p-shared", "", "", "dot2-a", "HOUSEHOLD_CROSS_ROOM", 100)
        .unwrap();
    state
        .shared_memory
        .upsert_profile_cache_with_source(
            "p-dev-a",
            "",
            "dot2-a",
            "dot2-a",
            "DEVICE_A_PRIVATE",
            200,
        )
        .unwrap();
    state
        .shared_memory
        .upsert_profile_cache_with_source("p-alice", "alice", "", "dot2-a", "ALICE_CROSS_ROOM", 300)
        .unwrap();
    state
        .shared_memory
        .upsert_profile_cache_with_source(
            "p-bob-b",
            "bob",
            "dot2-b",
            "dot2-a",
            "BOB_DEVICE_B_PRIVATE",
            400,
        )
        .unwrap();

    // 1. Alice on dot2-b: sees shared + Alice cross-room; isolated from dot2-a and Bob-on-dot2-b.
    let _ = chat_scoped(&base, &token, None, "hi", Some("alice"), Some("dot2-b")).await;
    // 2. Anonymous on dot2-a: sees shared + dot2-a private; isolated from Alice and Bob.
    let _ = chat_scoped(&base, &token, None, "hi", None, Some("dot2-a")).await;
    // 3. Bob on dot2-b: sees shared + Bob-on-dot2-b; isolated from dot2-a and Alice.
    let _ = chat_scoped(&base, &token, None, "hi", Some("bob"), Some("dot2-b")).await;

    let reqs = llm.seen_requests();
    let sys = |idx: usize| -> &str {
        reqs[idx]
            .iter()
            .find(|m| m.role == "system")
            .and_then(|m| m.content.as_deref())
            .unwrap()
    };

    assert!(sys(0).contains("HOUSEHOLD_CROSS_ROOM"));
    assert!(sys(0).contains("ALICE_CROSS_ROOM"));
    assert!(!sys(0).contains("DEVICE_A_PRIVATE"));
    assert!(!sys(0).contains("BOB_DEVICE_B_PRIVATE"));

    assert!(sys(1).contains("HOUSEHOLD_CROSS_ROOM"));
    assert!(sys(1).contains("DEVICE_A_PRIVATE"));
    assert!(!sys(1).contains("ALICE_CROSS_ROOM"));
    assert!(!sys(1).contains("BOB_DEVICE_B_PRIVATE"));

    assert!(sys(2).contains("HOUSEHOLD_CROSS_ROOM"));
    assert!(sys(2).contains("BOB_DEVICE_B_PRIVATE"));
    assert!(!sys(2).contains("DEVICE_A_PRIVATE"));
    assert!(!sys(2).contains("ALICE_CROSS_ROOM"));
}

#[tokio::test]
async fn b05_v2_sync_reconciles_deletions_and_scope_narrowing_without_wiping_on_http_error() {
    use axum::{
        extract::State,
        http::StatusCode,
        routing::{get, post},
        Json, Router,
    };
    use std::sync::Mutex;

    struct V2Nas {
        source_epoch: String,
        status_code: StatusCode,
        profiles: Vec<Value>,
        v2_posts: Vec<Value>,
    }

    let nas_state = Arc::new(Mutex::new(V2Nas {
        source_epoch: "11111111-1111-4111-8111-111111111111".into(),
        status_code: StatusCode::OK,
        profiles: vec![
            json!({
                "id": "p-keep",
                "summary": "KEEP_HOUSEHOLD_PROFILE",
                "visibility": { "person_id": null, "device_id": null },
                "dimension": "general",
                "version": 1,
                "row_revision": 1,
                "updated_ms": 100
            }),
            json!({
                "id": "p-delete",
                "summary": "DELETED_ON_NAS_PROFILE",
                "visibility": { "person_id": null, "device_id": null },
                "dimension": "general",
                "version": 1,
                "row_revision": 2,
                "updated_ms": 200
            }),
            json!({
                "id": "p-narrow",
                "summary": "NARROWED_TO_OTHER_DEVICE",
                "visibility": { "person_id": null, "device_id": null },
                "dimension": "general",
                "version": 1,
                "row_revision": 3,
                "updated_ms": 300
            }),
        ],
        v2_posts: Vec::new(),
    }));

    let nas_app = Router::new()
        .route(
            "/api/v2/status",
            get(|State(st): State<Arc<Mutex<V2Nas>>>| async move {
                let g = st.lock().unwrap();
                (
                    g.status_code,
                    Json(json!({ "source_epoch": g.source_epoch, "state": "v2_active" })),
                )
            }),
        )
        .route(
            "/api/v2/observations",
            post(
                |State(st): State<Arc<Mutex<V2Nas>>>, Json(body): Json<Value>| async move {
                    assert_eq!(body["protocol_version"], json!(2));
                    assert!(body["batch_id"].as_str().is_some());
                    assert!(body["observations"].is_array());
                    let mut g = st.lock().unwrap();
                    g.v2_posts.push(body);
                    (
                        StatusCode::OK,
                        Json(json!({ "protocol_version": 2, "accepted": 1, "deduplicated": 0 })),
                    )
                },
            ),
        )
        .route(
            "/api/v2/profiles",
            get(|State(st): State<Arc<Mutex<V2Nas>>>| async move {
                let g = st.lock().unwrap();
                (
                    g.status_code,
                    Json(json!({
                        "source_epoch": g.source_epoch,
                        "revision": 3,
                        "items": g.profiles,
                        "next_cursor": null
                    })),
                )
            }),
        )
        .with_state(nas_state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nas_base = format!("http://{}", listener.local_addr().unwrap());
    let nas_task = tokio::spawn(async move {
        let _ = axum::serve(listener, nas_app).await;
    });

    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["turn 1".into()],
        },
        Scripted::Text {
            chunks: vec!["turn 2".into()],
        },
    ]);
    let (base, state, token) = spawn_app(llm.clone()).await;

    // Enroll this core as `dot2-a` so scope narrowing to `dot2-b` is pruned locally too.
    state
        .store
        .with(|c| {
            c.execute("UPDATE config_head SET node_id='dot2-a' WHERE id=1", [])?;
            Ok(())
        })
        .unwrap();

    state
        .shared_memory
        .save_config(
            hm_core::config::SharedMemoryConfig {
                enabled: true,
                nas_base_url: nas_base.clone(),
                upload_enabled: true,
            },
            "test",
        )
        .unwrap();
    state
        .shared_memory
        .enqueue_observation("dot2-a", "living", "", "T-v2", "v2 observation")
        .unwrap();

    let outcome1 = state.shared_memory.sync_once().await;
    assert_eq!(
        outcome1,
        hm_core::shared_memory::SyncOutcome::Synced {
            uploaded: 1,
            cached: 3,
        }
    );
    assert_eq!(state.shared_memory.backlog_count().unwrap(), 0);
    assert_eq!(state.shared_memory.cache_count().unwrap(), 3);
    assert!(!nas_state.lock().unwrap().v2_posts.is_empty());

    let _ = chat(&base, &token, None, "hello").await;
    let sys1 = llm.seen_requests()[0]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap()
        .to_string();
    assert!(sys1.contains("KEEP_HOUSEHOLD_PROFILE"));
    assert!(sys1.contains("DELETED_ON_NAS_PROFILE"));
    assert!(sys1.contains("NARROWED_TO_OTHER_DEVICE"));

    // Simulate HTTP 502 from NAS: cache MUST NOT be wiped on failed HTTP!
    {
        let mut g = nas_state.lock().unwrap();
        g.status_code = StatusCode::BAD_GATEWAY;
    }
    let outcome_err = state.shared_memory.sync_once().await;
    assert!(matches!(
        outcome_err,
        hm_core::shared_memory::SyncOutcome::Failed(_)
    ));
    assert_eq!(
        state.shared_memory.cache_count().unwrap(),
        3,
        "failed HTTP must never wipe shared_profile_cache"
    );

    // Now delete `p-delete` on NAS, narrow `p-narrow` to `dot2-b`, and recover HTTP 200.
    {
        let mut g = nas_state.lock().unwrap();
        g.status_code = StatusCode::OK;
        g.profiles = vec![
            json!({
                "id": "p-keep",
                "summary": "KEEP_HOUSEHOLD_PROFILE",
                "visibility": { "person_id": null, "device_id": null },
                "dimension": "general",
                "version": 2,
                "row_revision": 4,
                "updated_ms": 400
            }),
            json!({
                "id": "p-narrow",
                "summary": "NARROWED_TO_OTHER_DEVICE",
                "visibility": { "person_id": null, "device_id": "dot2-b" },
                "dimension": "general",
                "version": 2,
                "row_revision": 5,
                "updated_ms": 500
            }),
        ];
    }
    // Reset circuit breaker via config save.
    state
        .shared_memory
        .save_config(
            hm_core::config::SharedMemoryConfig {
                enabled: true,
                nas_base_url: nas_base,
                upload_enabled: true,
            },
            "test",
        )
        .unwrap();

    let outcome2 = state.shared_memory.sync_once().await;
    assert_eq!(
        outcome2,
        hm_core::shared_memory::SyncOutcome::Synced {
            uploaded: 0,
            cached: 1,
        }
    );
    assert_eq!(state.shared_memory.cache_count().unwrap(), 1);

    let _ = chat(&base, &token, None, "hello again").await;
    let sys2 = llm.seen_requests()[1]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap()
        .to_string();
    assert!(sys2.contains("KEEP_HOUSEHOLD_PROFILE"));
    assert!(!sys2.contains("DELETED_ON_NAS_PROFILE"));
    assert!(!sys2.contains("NARROWED_TO_OTHER_DEVICE"));

    nas_task.abort();
}
