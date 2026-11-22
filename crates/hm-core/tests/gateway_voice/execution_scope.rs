use super::*;

/// Real connected Gateway wire ingress and Core voice consumer; no acoustic proof.
#[tokio::test]
async fn connected_voice_binds_device_only_for_prompt_and_all_memory_writers() {
    let llm=FakeLlm::new(vec![
        Scripted::Calls { calls:vec![
            ("workmem.set".into(),json!({"key_or_topic":"voice-scope","content":"DEVICE_ONLY_WORK"}).to_string()),
            ("user_profile.propose".into(),json!({"key":"tea","content":"DEVICE_ONLY_PROFILE","rationale":"explicit request"}).to_string()),
            ("memory.remember".into(),json!({"text":"DEVICE_ONLY_NOTE"}).to_string()),
            ("workmem.get".into(),json!({"scope_person":"alice"}).to_string()),
        ]},
        Scripted::Text { chunks:vec!["已记录。".into()] },
    ]);
    let (_base, state, gw) =
        spawn_mic_app_stream_tts(llm.clone(), "unused", Arc::new(RecordingTts::default())).await;
    state
        .working_memory
        .set("", "", "HOUSEHOLD_KEY", "HOUSEHOLD_VISIBLE")
        .unwrap();
    state
        .working_memory
        .set("", "dot2-d", "LOCAL_KEY", "LOCAL_VISIBLE")
        .unwrap();
    state
        .working_memory
        .set("alice", "", "PERSON_KEY", "ALICE_PRIVATE")
        .unwrap();
    state
        .working_memory
        .set("", "dot2-b", "FOREIGN_KEY", "OTHER_DEVICE_PRIVATE")
        .unwrap();
    explicit_request(&state, &gw, 1, "记住这个偏好").await;
    wait_until_sync("voice scoped writers", || {
        state
            .working_memory
            .get("", "dot2-d", "voice-scope")
            .unwrap()
            .is_some()
            && state.user_md.list(Some("active")).unwrap().len() == 1
            && state.memory.list().unwrap().len() == 1
    })
    .await;
    let work = state
        .working_memory
        .get("", "dot2-d", "voice-scope")
        .unwrap()
        .unwrap();
    assert_eq!(work.content, "DEVICE_ONLY_WORK");
    assert!(state
        .working_memory
        .get("", "", "voice-scope")
        .unwrap()
        .is_none());
    let user = state.user_md.list(Some("active")).unwrap().pop().unwrap();
    assert_eq!(
        (&*user.person_id, &*user.scope_device, &*user.source_device),
        ("", "dot2-d", "dot2-d")
    );
    let note = state.memory.list().unwrap().pop().unwrap();
    assert_eq!((&*note.scope_person, &*note.scope_device), ("", "dot2-d"));
    wait_until_sync("voice tool results reached model", || {
        llm.seen_requests().len() >= 2
    })
    .await;
    let requests = llm.seen_requests();
    let first = requests[0][0].content.as_deref().unwrap();
    assert!(first.contains("HOUSEHOLD_VISIBLE") && first.contains("LOCAL_VISIBLE"));
    assert!(!first.contains("ALICE_PRIVATE") && !first.contains("OTHER_DEVICE_PRIVATE"));
    let results = requests[1]
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!results.contains("ALICE_PRIVATE") && !results.contains("OTHER_DEVICE_PRIVATE"));
    assert!(
        results.contains("DEVICE_ONLY_WORK")
            && results.contains("DEVICE_ONLY_PROFILE")
            && results.contains("DEVICE_ONLY_NOTE")
    );
}

#[tokio::test]
async fn current_conversation_readback_and_clear_preserve_legacy_archive() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["新的会话。".into()],
    }]);
    let (base, state, gw) =
        spawn_mic_app_stream_tts(llm, "unused", Arc::new(RecordingTts::default())).await;
    state.sessions.ensure("dev:dot2-d").unwrap();
    state
        .sessions
        .append("dev:dot2-d", "user", "LEGACY_ARCHIVE")
        .unwrap();
    let client = reqwest::Client::new();
    let path = format!("{base}/api/v0/devices/dot2-d/conversation");
    let before: Value = client
        .get(&path)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(before["session_id"].is_null());
    assert!(before["messages"].as_array().unwrap().is_empty());
    explicit_request(&state, &gw, 1, "新的问题").await;
    wait_until_sync("new physical history", || {
        state
            .sessions
            .current_device_session_id("dot2-d")
            .unwrap()
            .is_some_and(|id| !state.sessions.messages(&id).unwrap().is_empty())
    })
    .await;
    let current: Value = client
        .get(&path)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = current["session_id"].as_str().unwrap();
    assert_ne!(id, "dev:dot2-d");
    assert!(!current.to_string().contains("LEGACY_ARCHIVE"));
    let wrong = client
        .delete(&path)
        .json(&json!({"expected_session_id":"dev:dot2-d"}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 409);
    assert_eq!(state.sessions.messages("dev:dot2-d").unwrap().len(), 1);
    let cleared: Value = client
        .delete(&path)
        .json(&json!({"expected_session_id":id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(cleared["cleared"].as_u64().unwrap() > 0);
    assert_eq!(state.sessions.messages("dev:dot2-d").unwrap().len(), 1);
}

#[tokio::test]
async fn selected_text_visibility_cannot_close_a_physical_session_but_voice_can() {
    let call = || Scripted::Calls {
        calls: vec![("session.close".into(), json!({}).to_string())],
    };
    let llm = FakeLlm::new(vec![
        call(),
        Scripted::Text {
            chunks: vec!["当前文本请求无设备会话权限。".into()],
        },
        call(),
    ]);
    let (base, state, gw) =
        spawn_mic_app_stream_tts(llm, "unused", Arc::new(RecordingTts::default())).await;
    state.voice.test_activate_session("dot2-d");
    let token = state.auth.provision("scope-regression").unwrap();
    let client = reqwest::Client::new();
    let selected: Value = client
        .post(format!("{base}/api/v1/chat/scopes"))
        .bearer_auth(&token)
        .json(&json!({"device_id":"dot2-d"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let response = client
        .post(format!("{base}/api/v0/chat"))
        .bearer_auth(&token)
        .json(&json!({"scope_binding_id":selected["binding_id"],"text":"处理当前会话"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let raw = response.text().await.unwrap();
    assert!(
        raw.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["type"] == "tool_result"
                && event["tool"] == "session.close"
                && event["status"] == "failed"),
        "{raw}"
    );
    assert!(!state
        .voice
        .recent(100)
        .iter()
        .any(|event| event["kind"] == "session_closed"));
    explicit_request(&state, &gw, 1, "处理当前会话").await;
    wait_until_sync("physical model session close", || {
        state
            .voice
            .recent(100)
            .iter()
            .any(|event| event["kind"] == "session_closed")
    })
    .await;
    assert_eq!(
        state.voice.mode_snapshot()["devices"][0]["session"]["state"],
        "idle"
    );
}
