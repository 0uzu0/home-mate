//! Actual HTTP ingress, production LLM HTTP adapter and advertised tool dispatch.
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use hm_core::{
    api::{router, AppState},
    store::Store,
    tool::mocks::MockInject,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
struct Provider {
    requests: Arc<Mutex<Vec<Value>>>,
    gate: Arc<Mutex<Option<tokio::sync::watch::Receiver<bool>>>>,
    arrived: Arc<tokio::sync::Notify>,
}
async fn provider(State(state): State<Provider>, Json(body): Json<Value>) -> impl IntoResponse {
    state.requests.lock().unwrap().push(body.clone());
    state.arrived.notify_one();
    let mut gate = { state.gate.lock().unwrap().clone() };
    if let Some(gate) = gate.as_mut() {
        while !*gate.borrow() {
            gate.changed().await.unwrap();
        }
    }
    let messages = body["messages"].as_array().unwrap();
    let user = messages.iter().rev().find(|m| m["role"] == "user").unwrap()["content"]
        .as_str()
        .unwrap();
    let command: Value = serde_json::from_str(user).unwrap_or(json!({}));
    let tool = command["tool"].as_str();
    let mut chunks = Vec::new();
    if let Some(tool) = tool.filter(|_| messages.last().unwrap()["role"] != "tool") {
        let wire = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| {
                t["function"]["description"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with(&format!("{tool}:"))
            })
            .unwrap()["function"]["name"]
            .as_str()
            .unwrap();
        chunks.push(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_scope","type":"function","function":{"name":wire,"arguments":command["args"].to_string()}}]},"finish_reason":null}]}));
        chunks.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}));
    } else {
        let system = messages[0]["content"].as_str().unwrap_or("");
        let answer = if system.contains("记忆提取器") {
            json!({"candidates":[{"text":"EXTRACTION_DEVICE_FACT","kind":"fact","reason":"explicit preference"}]}).to_string()
        } else if system.contains("记忆审核器") {
            json!({"decisions":[{"index":0,"ok":true,"reason":"eligible fact"}]}).to_string()
        } else {
            "scope checked".into()
        };
        chunks
            .push(json!({"choices":[{"index":0,"delta":{"content":answer},"finish_reason":null}]}));
        chunks.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
    }
    let mut data = chunks
        .into_iter()
        .map(|c| format!("data: {c}\n\n"))
        .collect::<String>();
    data.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], data)
}

#[tokio::test]
async fn revocation_while_real_provider_waits_suppresses_late_dispatch_and_output() {
    let f = Fixture::new().await;
    let binding = f.binding("alice", "B", false).await;
    let (release, gate) = tokio::sync::watch::channel(false);
    *f.provider.gate.lock().unwrap() = Some(gate);
    let response=f.client.post(format!("{}/api/v0/chat",f.base)).bearer_auth(&f.token)
        .json(&json!({"scope_binding_id":binding["binding_id"],"text":json!({"tool":"workmem.set","args":{"key_or_topic":"late","content":"LATE_FORBIDDEN"}}).to_string()})).send().await.unwrap();
    let read = tokio::spawn(async move { response.text().await.unwrap() });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        f.provider.arrived.notified(),
    )
    .await
    .unwrap();
    let revoke = f
        .client
        .delete(format!(
            "{}/api/v1/chat/scopes/{}",
            f.base,
            binding["binding_id"].as_str().unwrap()
        ))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status(), 200);
    release.send(true).unwrap();
    let raw = read.await.unwrap();
    assert!(raw.contains("error"), "{raw}");
    assert!(
        !raw.contains("LATE_FORBIDDEN") && !raw.contains("tool_result"),
        "{raw}"
    );
    assert!(f
        .state
        .working_memory
        .get("alice", "B", "late")
        .unwrap()
        .is_none());
    assert_eq!(f.provider.requests.lock().unwrap().len(), 1);
    assert!(f
        .state
        .tasks
        .list(10)
        .unwrap()
        .iter()
        .all(|task| task["status"] != "running"));
}
async fn listen(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}
struct Fixture {
    base: String,
    token: String,
    state: Arc<AppState>,
    provider: Provider,
    client: reqwest::Client,
}

#[tokio::test]
async fn actual_http_full_selected_scope_matrix_matches_exact_seeded_visibility() {
    let f = Fixture::new().await;
    let rows = [
        ("", "", "FACT_HOUSEHOLD"),
        ("alice", "", "FACT_ALICE_ONLY"),
        ("bob", "", "FACT_BOB_ONLY"),
        ("", "B", "FACT_DEVICE_B"),
        ("", "D", "FACT_DEVICE_D"),
        ("alice", "B", "FACT_ALICE_B"),
        ("alice", "D", "FACT_ALICE_D"),
        ("bob", "B", "FACT_BOB_B"),
        ("bob", "D", "FACT_BOB_D"),
    ];
    for (person, device, content) in rows {
        f.state
            .working_memory
            .set(person, device, "same-key", content)
            .unwrap();
    }
    for (person, device) in [
        ("alice", "B"),
        ("alice", "D"),
        ("bob", "B"),
        ("bob", "D"),
        ("alice", ""),
        ("", "B"),
        ("", ""),
    ] {
        let binding = f.binding(person, device, false).await;
        let before = f.provider.requests.lock().unwrap().len();
        f.chat(
            Some(&binding),
            Some(("workmem.get", json!({"key_or_topic":"same-key"}))),
        )
        .await;
        let expected = rows
            .iter()
            .filter(|(p, d, _)| (p.is_empty() || *p == person) && (d.is_empty() || *d == device))
            .map(|(_, _, content)| content.to_string())
            .collect::<std::collections::BTreeSet<_>>();
        let observed = f.tool_result()["result"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["content"].as_str().unwrap().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(observed, expected, "{person}/{device}");
        let prompt = f.provider.requests.lock().unwrap()[before]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        for (_, _, content) in rows {
            assert_eq!(
                prompt.contains(content),
                expected.contains(content),
                "{person}/{device}: {content}"
            );
        }
    }
}

#[tokio::test]
async fn actual_cancel_and_epoch_boundaries_block_all_late_memory_writers() {
    for boundary in ["cancel", "reset"] {
        for (tool, args) in [
            (
                "workmem.set",
                json!({"key_or_topic":"late","content":"NO_LATE_WORK"}),
            ),
            (
                "user_profile.propose",
                json!({"key":"late","content":"NO_LATE_PROFILE","rationale":"explicit fact"}),
            ),
            ("memory.remember", json!({"text":"NO_LATE_NOTE"})),
        ] {
            let f = Fixture::new().await;
            let binding = f.binding("alice", "B", false).await;
            let (release, gate) = tokio::sync::watch::channel(false);
            *f.provider.gate.lock().unwrap() = Some(gate);
            let response=f.client.post(format!("{}/api/v0/chat",f.base)).bearer_auth(&f.token)
                .json(&json!({"scope_binding_id":binding["binding_id"],"text":json!({"tool":tool,"args":args}).to_string()})).send().await.unwrap();
            let read = tokio::spawn(async move { response.text().await.unwrap() });
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                f.provider.arrived.notified(),
            )
            .await
            .unwrap();
            let response = if boundary == "cancel" {
                let task = f.state.tasks.list(1).unwrap().pop().unwrap();
                f.client
                    .post(format!("{}/api/v0/session/cancel", f.base))
                    .bearer_auth(&f.token)
                    .json(&json!({"task_id":task["id"]}))
                    .send()
                    .await
                    .unwrap()
            } else {
                f.client
                    .post(format!("{}/api/v0/system/factory-reset", f.base))
                    .bearer_auth(&f.token)
                    .json(&json!({"confirm":"FACTORY_RESET"}))
                    .send()
                    .await
                    .unwrap()
            };
            assert_eq!(response.status(), 200, "{boundary}/{tool}");
            release.send(true).unwrap();
            let raw = read.await.unwrap();
            assert!(!raw.contains("NO_LATE_"), "{boundary}/{tool}: {raw}");
            assert!(f
                .state
                .working_memory
                .get("alice", "B", "late")
                .unwrap()
                .is_none());
            assert!(f.state.user_md.list(None).unwrap().is_empty());
            assert!(f.state.memory.list().unwrap().is_empty());
            assert_eq!(f.provider.requests.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn production_extractor_preserves_device_scope_and_reset_fences_awaited_result() {
    for reset in [false, true] {
        let f = Fixture::new().await;
        let session = f.state.sessions.device_session_id("B").unwrap();
        f.state
            .sessions
            .append(&session, "user", "Please remember my tea preference")
            .unwrap();
        let (release, gate) = tokio::sync::watch::channel(false);
        *f.provider.gate.lock().unwrap() = Some(gate);
        f.state.voice.start_memory_extraction("B", "b01_scope_test");
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            f.provider.arrived.notified(),
        )
        .await
        .unwrap();
        if reset {
            let response = f
                .client
                .post(format!("{}/api/v0/system/factory-reset", f.base))
                .bearer_auth(&f.token)
                .json(&json!({"confirm":"FACTORY_RESET"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
        }
        release.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if f.state
                    .voice
                    .recent(100)
                    .iter()
                    .any(|event| event["kind"] == "memory_extract")
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let notes = f.state.memory.list().unwrap();
        if reset {
            assert!(notes.is_empty());
            assert!(f.state.memory.candidates(None).unwrap().is_empty());
            assert_eq!(f.provider.requests.lock().unwrap().len(), 1);
        } else {
            assert_eq!(notes.len(), 1);
            assert_eq!(
                (&*notes[0].scope_person, &*notes[0].scope_device),
                ("", "B")
            );
            assert_eq!(f.provider.requests.lock().unwrap().len(), 2);
        }
    }
}
impl Fixture {
    async fn new() -> Self {
        let provider_state = Provider::default();
        let llm = listen(
            Router::new()
                .route("/v1/chat/completions", post(provider))
                .with_state(provider_state.clone()),
        )
        .await;
        let state = AppState::with_overrides(
            Arc::new(Store::open_in_memory().unwrap()),
            PathBuf::from("/tmp/luna-b01-test"),
            MockInject::default(),
            None,
            None,
            None,
            None,
        );
        let base = listen(router(state.clone())).await;
        let token = state.auth.provision("scope-test").unwrap();
        let fixture = Self {
            base,
            token,
            state,
            provider: provider_state,
            client: reqwest::Client::new(),
        };
        let response=fixture.client.put(format!("{}/api/v0/config/services/llm",fixture.base)).bearer_auth(&fixture.token)
            .json(&json!({"provider":"deepseek","base_url":format!("{llm}/v1"),"model":"scope-fixture","api_key_env":""})).send().await.unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        fixture
    }
    async fn binding(&self, person: &str, device: &str, household: bool) -> Value {
        let response = self
            .client
            .post(format!("{}/api/v1/chat/scopes", self.base))
            .bearer_auth(&self.token)
            .json(&json!({"person_id":person,"device_id":device,"household_write":household}))
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
    async fn chat(&self, binding: Option<&Value>, tool: Option<(&str, Value)>) -> Vec<Value> {
        let text = tool
            .map(|(name, args)| json!({"tool":name,"args":args}).to_string())
            .unwrap_or_else(|| "hello scope".into());
        let mut body = json!({"text":text});
        if let Some(binding) = binding {
            body["scope_binding_id"] = binding["binding_id"].clone();
        }
        let response = self
            .client
            .post(format!("{}/api/v0/chat", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        let raw = response.text().await.unwrap();
        let events = raw
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str(data.trim()).ok())
            .collect::<Vec<Value>>();
        assert!(events.iter().any(|e| e["type"] == "turn_end"), "{events:?}");
        events
    }
    fn tool_result(&self) -> Value {
        let requests = self.provider.requests.lock().unwrap();
        let wire = requests.last().unwrap()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        serde_json::from_str(wire).unwrap()
    }
}

#[tokio::test]
async fn actual_prompt_and_tools_share_conjunctive_scope_and_exact_write_grant() {
    let f = Fixture::new().await;
    for (person, device, content) in [
        ("", "", "HOUSEHOLD"),
        ("alice", "", "ALICE"),
        ("bob", "", "BOB_SECRET"),
        ("", "B", "DEVICE_B"),
        ("", "D", "D_SECRET"),
        ("alice", "B", "ALICE_B"),
        ("alice", "D", "ALICE_D_SECRET"),
    ] {
        f.state
            .working_memory
            .set(person, device, "same_key", content)
            .unwrap();
    }
    f.state.store.with(|c| {
        for (id,person,device,summary) in [("shared-household","","","SHARED_HOUSEHOLD"),("shared-alice-b","alice","B","SHARED_ALICE_B"),("shared-bob","bob","","SHARED_BOB_PRIVATE"),("shared-d","","D","SHARED_D_PRIVATE")] {
            c.execute("INSERT INTO shared_profile_cache(id,scope_person,scope_device,summary,updated_ms,fetched_ms) VALUES (?1,?2,?3,?4,1,1)",rusqlite::params![id,person,device,summary])?;
        }
        Ok(())
    }).unwrap();
    let alice = f.binding("alice", "B", false).await;
    f.chat(Some(&alice), Some(("workmem.get", json!({})))).await;
    let requests = f.provider.requests.lock().unwrap().clone();
    let first = requests[0]["messages"][0]["content"].as_str().unwrap();
    for visible in ["HOUSEHOLD", "ALICE", "DEVICE_B", "ALICE_B"] {
        assert!(first.contains(visible), "missing {visible}");
    }
    for hidden in ["BOB_SECRET", "D_SECRET", "ALICE_D_SECRET"] {
        assert!(!first.contains(hidden), "leaked {hidden}");
    }
    assert!(first.contains("SHARED_HOUSEHOLD") && first.contains("SHARED_ALICE_B"));
    assert!(!first.contains("SHARED_BOB_PRIVATE") && !first.contains("SHARED_D_PRIVATE"));
    let get_result = f.tool_result();
    assert_eq!(get_result["result"]["items"].as_array().unwrap().len(), 4);
    let result = get_result.to_string();
    for visible in ["HOUSEHOLD", "ALICE", "DEVICE_B", "ALICE_B"] {
        assert!(result.contains(visible), "tool omitted {visible}");
    }
    for hidden in ["BOB_SECRET", "D_SECRET", "ALICE_D_SECRET"] {
        assert!(!result.contains(hidden), "tool leaked {hidden}");
    }
    f.chat(
        Some(&alice),
        Some((
            "workmem.set",
            json!({"key_or_topic":"default","content":"SAFE_DEFAULT"}),
        )),
    )
    .await;
    assert!(f
        .state
        .working_memory
        .get("alice", "B", "default")
        .unwrap()
        .is_some());
    f.chat(
        Some(&alice),
        Some((
            "workmem.set",
            json!({"key_or_topic":"default","content":"UPDATED_DEFAULT"}),
        )),
    )
    .await;
    assert_eq!(
        f.state
            .working_memory
            .get("alice", "B", "default")
            .unwrap()
            .unwrap()
            .content,
        "UPDATED_DEFAULT"
    );
    f.chat(
        Some(&alice),
        Some(("workmem.delete", json!({"key_or_topic":"default"}))),
    )
    .await;
    assert_eq!(f.tool_result()["result"]["deleted"], true);
    assert!(f
        .state
        .working_memory
        .get("alice", "B", "default")
        .unwrap()
        .is_none());
    f.chat(Some(&alice),Some(("workmem.set",json!({"scope_person":"bob","scope_device":"D","key_or_topic":"same_key","content":"FORGED"})))).await;
    assert_eq!(
        f.state
            .working_memory
            .get("bob", "", "same_key")
            .unwrap()
            .unwrap()
            .content,
        "BOB_SECRET"
    );
    assert!(f
        .state
        .working_memory
        .get("bob", "D", "same_key")
        .unwrap()
        .is_none());
    f.chat(
        Some(&alice),
        Some((
            "workmem.delete",
            json!({"scope_person":"bob","scope_device":"","key_or_topic":"same_key"}),
        )),
    )
    .await;
    let mut hidden = f.tool_result();
    hidden["result"]
        .as_object_mut()
        .unwrap()
        .remove("key_or_topic");
    f.chat(
        Some(&alice),
        Some((
            "workmem.delete",
            json!({"scope_person":"bob","scope_device":"","key_or_topic":"absent"}),
        )),
    )
    .await;
    let mut absent = f.tool_result();
    absent["result"]
        .as_object_mut()
        .unwrap()
        .remove("key_or_topic");
    assert_eq!(hidden, absent);
    assert_eq!(absent["result"]["deleted"], false);
    f.chat(Some(&alice),Some(("user_profile.propose",json!({"key":"tea","content":"PERSON_DEVICE_PROFILE","rationale":"explicit preference"})))).await;
    f.chat(Some(&alice),Some(("user_profile.propose",json!({"person_id":"bob","key":"forged","content":"FORGED_PROFILE","rationale":"model-selected person"})))).await;
    assert_eq!(f.state.user_md.list(Some("active")).unwrap().len(), 1);
    let entry = f.state.user_md.list(Some("active")).unwrap().pop().unwrap();
    assert_eq!((&*entry.person_id, &*entry.scope_device), ("alice", "B"));
    f.chat(
        Some(&alice),
        Some(("memory.remember", json!({"text":"PERSON_DEVICE_NOTE"}))),
    )
    .await;
    let note = f
        .state
        .memory
        .list()
        .unwrap()
        .into_iter()
        .find(|m| m.text == "PERSON_DEVICE_NOTE")
        .unwrap();
    assert_eq!((&*note.scope_person, &*note.scope_device), ("alice", "B"));
    let before = f.provider.requests.lock().unwrap().len();
    f.chat(Some(&alice), None).await;
    let next = f.provider.requests.lock().unwrap()[before]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(next.contains("PERSON_DEVICE_PROFILE") && next.contains("PERSON_DEVICE_NOTE"));
    f.chat(
        Some(&alice),
        Some((
            "workmem.get",
            json!({"scope_person":"bob","key_or_topic":"same_key"}),
        )),
    )
    .await;
    let hidden_read = f.tool_result();
    f.chat(
        Some(&alice),
        Some((
            "workmem.get",
            json!({"scope_person":"bob","key_or_topic":"absent"}),
        )),
    )
    .await;
    assert_eq!(hidden_read, f.tool_result());
    let bob = f.binding("bob", "B", false).await;
    let before = f.provider.requests.lock().unwrap().len();
    f.chat(Some(&bob), None).await;
    let prompt = f.provider.requests.lock().unwrap()[before]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!prompt.contains("PERSON_DEVICE_PROFILE") && !prompt.contains("PERSON_DEVICE_NOTE"));
    let household = f.binding("", "", true).await;
    f.chat(
        Some(&household),
        Some((
            "workmem.set",
            json!({"key_or_topic":"shared","content":"EXPLICIT_HOUSEHOLD"}),
        )),
    )
    .await;
    assert!(f
        .state
        .working_memory
        .get("", "", "shared")
        .unwrap()
        .is_some());
    f.chat(
        Some(&household),
        Some(("memory.remember", json!({"text":"EXPLICIT_HOUSEHOLD_NOTE"}))),
    )
    .await;
    f.chat(Some(&household),Some(("user_profile.propose",json!({"key":"household","content":"EXPLICIT_HOUSEHOLD_PROFILE","rationale":"explicit household grant"})))).await;
    let before = f.provider.requests.lock().unwrap().len();
    f.chat(None, None).await;
    let prompt = f.provider.requests.lock().unwrap()[before]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        prompt.contains("EXPLICIT_HOUSEHOLD_NOTE")
            && prompt.contains("EXPLICIT_HOUSEHOLD_PROFILE")
            && prompt.contains("EXPLICIT_HOUSEHOLD")
    );
    f.chat(
        None,
        Some((
            "workmem.set",
            json!({"key_or_topic":"forbidden","content":"NO_WRITE"}),
        )),
    )
    .await;
    assert!(f
        .state
        .working_memory
        .get("", "", "forbidden")
        .unwrap()
        .is_none());
    let descriptors = f
        .state
        .registry
        .capabilities()
        .into_iter()
        .flat_map(|service| service.tools)
        .collect::<Vec<_>>();
    assert_eq!(
        descriptors
            .iter()
            .find(|d| d.name == "workmem.delete")
            .unwrap()
            .side_effect,
        hm_core::tool::SideEffect::Mutate
    );
}

#[tokio::test]
async fn real_http_rejects_forged_authority_wrong_session_revoke_and_stale_epoch() {
    let f = Fixture::new().await;
    let unauthorized = f
        .client
        .post(format!("{}/api/v1/chat/scopes", f.base))
        .json(&json!({"person_id":"alice"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    let forged = f
        .client
        .post(format!("{}/api/v0/chat", f.base))
        .bearer_auth(&f.token)
        .json(&json!({"text":"hello","person_id":"alice"}))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 400);
    let a = f.binding("alice", "B", false).await;
    let b = f.binding("bob", "D", false).await;
    let mismatch=f.client.post(format!("{}/api/v0/chat",f.base)).bearer_auth(&f.token).json(&json!({"text":"hello","scope_binding_id":a["binding_id"],"session_id":b["session_id"]})).send().await.unwrap();
    assert_eq!(mismatch.status(), 409);
    f.state.sessions.ensure("dev:B").unwrap();
    let reserved = f
        .client
        .post(format!("{}/api/v0/chat", f.base))
        .bearer_auth(&f.token)
        .json(&json!({"text":"hello","session_id":"dev:B"}))
        .send()
        .await
        .unwrap();
    assert_eq!(reserved.status(), 409);
    let claimed = f
        .state
        .store
        .with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM execution_sessions WHERE session_id='dev:B'",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .unwrap();
    assert_eq!(claimed, 0);
    assert_eq!(f.state.sessions.device_session_id("B").unwrap(), "dev:B");
    f.state.sessions.ensure("legacy-unclassified").unwrap();
    f.state
        .sessions
        .append("legacy-unclassified", "user", "ARCHIVE_PRIVATE")
        .unwrap();
    let legacy = f
        .client
        .post(format!("{}/api/v0/chat", f.base))
        .bearer_auth(&f.token)
        .json(&json!({"text":"hello","session_id":"legacy-unclassified"}))
        .send()
        .await
        .unwrap();
    assert_eq!(legacy.status(), 409);
    assert_eq!(
        legacy.json::<Value>().await.unwrap()["error"],
        "SESSION_SCOPE_UNVERIFIED"
    );
    assert_eq!(
        f.state
            .sessions
            .messages("legacy-unclassified")
            .unwrap()
            .len(),
        1
    );
    let unknown = f
        .client
        .post(format!("{}/api/v0/chat", f.base))
        .bearer_auth(&f.token)
        .json(&json!({"text":"hello","session_id":"unknown"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 200);
    assert!(unknown.text().await.unwrap().contains("turn_end"));
    let id = a["binding_id"].as_str().unwrap();
    assert!(f
        .client
        .get(format!("{}/api/v1/chat/scopes/{id}", f.base))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(f
        .client
        .delete(format!("{}/api/v1/chat/scopes/{id}", f.base))
        .bearer_auth(&f.token)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let revoked = f
        .client
        .post(format!("{}/api/v0/chat", f.base))
        .bearer_auth(&f.token)
        .json(&json!({"text":"hello","scope_binding_id":id}))
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), 409);
    f.state.store.factory_reset(false).unwrap();
    let stale = f
        .client
        .post(format!("{}/api/v0/chat", f.base))
        .bearer_auth(&f.token)
        .json(&json!({"text":"hello","scope_binding_id":b["binding_id"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 401); // reset clears the bearer store as well as scope data
    assert_eq!(f.provider.requests.lock().unwrap().len(), 1);
}
