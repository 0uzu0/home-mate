//! Integration tests for:
//! 1. `X-Internal-Token` on `SharedMemoryService` sync (sent on wire, never echoed).
//! 2. `W4b` (`#83`) Versioned `Agent.md` hot-reload, rollback, and model-write prohibition.
//! 3. `W4b` (`#83`) Auditable `USER.md` provenance (`user_profile.propose`), scoped
//!    prompt injection, and Console revert.

use axum::extract::State as AxumState;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use hm_core::api::{router, AppState};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::tool::mocks::MockInject;
use hm_core::tool::SideEffect;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
    let token = state.auth.provision("persona-fixture").unwrap();
    (format!("http://{addr}"), state, token)
}

async fn chat_scoped(
    base: &str,
    token: &str,
    text: &str,
    person_id: Option<&str>,
    device_id: Option<&str>,
) -> String {
    let c = reqwest::Client::new();
    let mut body = json!({ "text": text });
    if person_id.is_some() || device_id.is_some() {
        let response = c
            .post(format!("{base}/api/v1/chat/scopes"))
            .bearer_auth(token)
            .json(
                &json!({"person_id": person_id, "device_id": device_id, "household_write": false}),
            )
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        let selected: Value = response.json().await.unwrap();
        body["scope_binding_id"] = selected["binding_id"].clone();
    }
    let resp = c
        .post(format!("{base}/api/v0/chat"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let mut stream = resp.bytes_stream();
    let mut raw = String::new();
    while let Some(chunk) = stream.next().await {
        raw.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }
    raw
}

#[derive(Default)]
struct MockNasState {
    obs_tokens: Vec<Option<String>>,
    profile_tokens: Vec<Option<String>>,
    obs_count: usize,
}

async fn mock_obs(
    AxumState(st): AxumState<Arc<Mutex<MockNasState>>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    let tok = headers
        .get("x-internal-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut guard = st.lock().unwrap();
    guard.obs_tokens.push(tok);
    guard.obs_count += body["observations"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    Json(json!({ "accepted": guard.obs_count }))
}

async fn mock_profiles(
    AxumState(st): AxumState<Arc<Mutex<MockNasState>>>,
    headers: HeaderMap,
) -> Json<Value> {
    let tok = headers
        .get("x-internal-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    st.lock().unwrap().profile_tokens.push(tok);
    Json(json!({
        "profiles": [{
            "id": "p-nas-1",
            "scope_person": "",
            "scope_device": "",
            "summary": "Household prefers quiet mornings",
            "updated_ms": 12345
        }]
    }))
}

#[tokio::test]
async fn shared_memory_x_internal_token_sent_and_never_echoed() {
    let nas_state = Arc::new(Mutex::new(MockNasState::default()));
    let nas_app = Router::new()
        .route("/api/v1/observations", post(mock_obs))
        .route("/api/v1/profiles", get(mock_profiles))
        .with_state(nas_state.clone());
    let nas_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nas_addr = nas_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(nas_listener, nas_app).await.unwrap() });
    let nas_url = format!("http://{nas_addr}");

    let llm = FakeLlm::new(vec![]);
    let (base, state, _token) = spawn_app(llm).await;
    let c = paired_client(&_token);

    let secret_tok = "super-secret-internal-token-99";
    let put_resp = c
        .put(format!("{base}/api/v0/shared-memory/config"))
        .json(&json!({
            "enabled": true,
            "nas_base_url": nas_url,
            "upload_enabled": true,
            "token": secret_tok
        }))
        .send()
        .await
        .unwrap();
    assert!(put_resp.status().is_success());
    let put_text = put_resp.text().await.unwrap();
    assert!(
        !put_text.contains(secret_tok),
        "token must never be echoed in PUT response: {put_text}"
    );
    let put_json: Value = serde_json::from_str(&put_text).unwrap();
    assert_eq!(put_json["status"]["token_set"], json!(true));

    let status_text = c
        .get(format!("{base}/api/v0/shared-memory/status"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !status_text.contains(secret_tok),
        "token must never be echoed in status response: {status_text}"
    );
    let status_json: Value = serde_json::from_str(&status_text).unwrap();
    assert_eq!(status_json["token_set"], json!(true));

    state
        .shared_memory
        .enqueue_observation("dot2-a", "living", "alice", "T-tok", "test obs")
        .unwrap();
    let outcome = state.shared_memory.sync_once().await;
    assert_eq!(
        outcome,
        hm_core::shared_memory::SyncOutcome::Synced {
            uploaded: 1,
            cached: 1
        }
    );

    let guard = nas_state.lock().unwrap();
    assert_eq!(guard.obs_tokens, vec![Some(secret_tok.to_string())]);
    assert!(guard
        .profile_tokens
        .iter()
        .any(|t| t.as_deref() == Some(secret_tok)));
}

#[tokio::test]
async fn agent_md_hot_reload_rollback_and_model_cannot_modify() {
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
    let (base, state, _token) = spawn_app(llm.clone()).await;
    let c = paired_client(&_token);

    // Safety check: no registered tool may modify Agent.md.
    for svc in state.registry.ready_capabilities() {
        for t in &svc.tools {
            assert!(
                !t.name.contains("agent_md"),
                "LLM tool {} must not exist for Agent.md",
                t.name
            );
        }
    }

    // Save v1 Agent.md
    let r1: Value = c
        .put(format!("{base}/api/v0/agent-md"))
        .json(&json!({
            "content": "Persona v1: speak like a calm butler.",
            "note": "initial butler persona"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r1["state"], json!("applied"));
    assert_eq!(r1["version"], json!(1));
    let v1_id = r1["revisions"][0]["id"].as_i64().unwrap();

    // Turn 1 sees v1 without restarting Core.
    let _ = chat_scoped(&base, &_token, "hi", None, None).await;

    // Save v2 Agent.md
    let r2: Value = c
        .put(format!("{base}/api/v0/agent-md"))
        .json(&json!({
            "content": "Persona v2: ultra concise replies.",
            "note": "concise mode"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r2["version"], json!(2));

    // Turn 2 hot-reloads v2.
    let _ = chat_scoped(&base, &_token, "hi again", None, None).await;

    // Rollback to v1 by revision id.
    let rb: Value = c
        .post(format!("{base}/api/v0/agent-md/rollback"))
        .json(&json!({ "id": v1_id }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rb["state"], json!("rolled_back"));
    assert_eq!(rb["version"], json!(3));
    assert_eq!(
        rb["content"],
        json!("Persona v1: speak like a calm butler.")
    );

    // Turn 3 hot-reloads rolled-back v1 content.
    let _ = chat_scoped(&base, &_token, "hi third", None, None).await;

    let reqs = llm.seen_requests();
    let sys1 = reqs[0]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap();
    let sys2 = reqs[1]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap();
    let sys3 = reqs[2]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap();

    assert!(sys1.contains("Persona v1: speak like a calm butler."));
    assert!(sys2.contains("Persona v2: ultra concise replies."));
    assert!(!sys2.contains("Persona v1: speak like a calm butler."));
    assert!(sys3.contains("Persona v1: speak like a calm butler."));
}

#[tokio::test]
async fn user_md_provenance_tool_propose_scoped_and_revert() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "user_profile.propose".into(),
                json!({
                    "key": "tea_preference",
                    "content": "Prefers jasmine tea in the afternoon",
                    "rationale": "Alice explicitly mentioned this",
                    "person_id": "alice"
                })
                .to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["Noted.".into()],
        },
        Scripted::Text {
            chunks: vec!["anon ok".into()],
        },
        Scripted::Text {
            chunks: vec!["alice ok".into()],
        },
        Scripted::Text {
            chunks: vec!["after revert".into()],
        },
    ]);
    let (base, state, _token) = spawn_app(llm.clone()).await;
    let c = paired_client(&_token);

    // Verify user_profile.propose is registered with SideEffect::Mutate.
    let umd_tool = state
        .registry
        .ready_capabilities()
        .into_iter()
        .flat_map(|s| s.tools)
        .find(|t| t.name == "user_profile.propose")
        .expect("user_profile.propose must be registered");
    assert_eq!(umd_tool.side_effect, SideEffect::Mutate);

    // Turn 1: model proposes USER.md entry from dot2-living.
    let raw1 = chat_scoped(
        &base,
        &_token,
        "Remember I like jasmine tea",
        Some("alice"),
        Some("dot2-living"),
    )
    .await;

    let list_json: Value = c
        .get(format!("{base}/api/v0/user-md"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = list_json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "raw1={raw1}");
    let e = &entries[0];
    let entry_id = e["id"].as_str().unwrap().to_string();
    assert_eq!(e["key"], json!("tea_preference"));
    assert_eq!(e["person_id"], json!("alice"));
    // Choosing device visibility does not claim a physical device origin.
    assert_eq!(e["source_device"], json!("console"));
    assert_eq!(e["author"], json!("model"));
    assert_eq!(e["status"], json!("active"));
    assert_eq!(e["rationale"], json!("Alice explicitly mentioned this"));
    assert!(!e["trace_id"].as_str().unwrap().is_empty());
    assert!(e["turn_ts"].as_u64().unwrap() > 0);

    // Turn 2: anonymous caller must NOT see alice's private USER.md entry.
    let _ = chat_scoped(&base, &_token, "hello", None, Some("dot2-living")).await;
    // Turn 3: alice MUST see her USER.md entry.
    let _ = chat_scoped(&base, &_token, "hello", Some("alice"), Some("dot2-living")).await;

    // Console reverts the entry.
    let rev_resp: Value = c
        .post(format!("{base}/api/v0/user-md/{entry_id}/revert"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rev_resp["state"], json!("reverted"));
    assert_eq!(rev_resp["entry"]["status"], json!("reverted"));

    // Turn 4: alice no longer sees reverted entry in system prompt.
    let _ = chat_scoped(
        &base,
        &_token,
        "hello again",
        Some("alice"),
        Some("dot2-living"),
    )
    .await;

    let reqs = llm.seen_requests();
    // reqs[0] = turn1 before tool, reqs[1] = turn1 after tool, reqs[2] = anon, reqs[3] = alice, reqs[4] = alice after revert
    let sys_anon = reqs[2]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap();
    let sys_alice = reqs[3]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap();
    let sys_reverted = reqs[4]
        .iter()
        .find(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap();

    assert!(!sys_anon.contains("Prefers jasmine tea in the afternoon"));
    assert!(sys_alice.contains("Prefers jasmine tea in the afternoon"));
    assert!(!sys_reverted.contains("Prefers jasmine tea in the afternoon"));
}
