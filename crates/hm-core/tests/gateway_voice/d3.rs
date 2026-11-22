//! D3 verifies the real HTTP/config and model-tool execution boundaries.
//! FakeLlm controls tool selection; these tests do not claim semantic-model quality.
use super::*;

async fn app(llm: FakeLlm) -> (String, Arc<AppState>, Arc<FakeGateway>) {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        std::env::temp_dir().join(hm_core::id::next("hm-d3")),
        MockInject::default(),
        Some(Arc::new(llm)),
        Some(Arc::new(MockTts::default())),
        None,
        None,
    );
    let mut settings = state.voice.settings_snapshot();
    settings.filler = no_filler();
    // These suites are about the WINDOW (its close phrase, its credits): the
    // product default since 2026-11-15 is "an answer ends the conversation",
    // so the window is asked for explicitly here.
    settings.follow_window_enabled = true;
    state.voice.restore_settings(settings, "test").unwrap();
    enable_follow_window(&state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], port).into());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let gw = connect_fake_gateway(port).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = hm_core::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, state, gw)
}

async fn put(base: &str, patch: Value) -> reqwest::Response {
    reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&patch)
        .send()
        .await
        .unwrap()
}
async fn get(base: &str, path: &str) -> Value {
    reqwest::get(format!("{base}/api/v0/{path}"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}
async fn text(state: &AppState, gw: &FakeGateway, seq: u64, text: &str) {
    gw.send_raw_json(json!({"type":"utterance","device":"dot2-d","ts":hm_core::id::now_ms(),
        "seq":seq,"source":"text","text":text,"origin":{"kind":"leased",
        "lease":state.voice.capture_lease("dot2-d"),"origin":{"kind":"text","input_id":hm_core::id::next("d3")}}})).await;
}
fn session(state: &AppState) -> Value {
    state.voice.mode_snapshot()["devices"][0]["session"].clone()
}

#[tokio::test]
async fn close_api_checks_generation_closes_and_returns_the_new_authority() {
    let (base, state, gw) = app(FakeLlm::new(vec![])).await;
    explicit_request(&state, &gw, 1, "现在几点").await;
    let (play, _) = gw.until_play("voice").await;
    let before = session(&state);
    let generation = before["generation"]
        .as_u64()
        .expect("session generation must be exposed");
    assert_eq!(before["phase"], "active");
    assert!(
        before["tail_deadline_ms"].is_null(),
        "an in-flight first answer has no completed-answer tail"
    );
    for key in [
        "opened_by",
        "credits",
        "tail_deadline_ms",
        "close_reason",
        "last_denied",
    ] {
        assert!(before.get(key).is_some(), "missing {key}: {before}");
    }
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/api/v0/session/close"))
        .json(&json!({"device":"dot2-d","generation":generation + 100}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(session(&state)["generation"], generation);
    let started = tokio::time::Instant::now();
    let response: Value = client
        .post(format!("{base}/api/v0/session/close"))
        .json(&json!({"device":"dot2-d","generation":generation}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let closed = response["session"].clone();
    assert!(closed["generation"].as_u64().unwrap() > generation);
    assert_eq!(closed["phase"], "closed");
    assert!(closed["tail_deadline_ms"].is_null());
    assert_eq!(closed["credits"], 0);
    assert_eq!(closed["close_reason"], "explicit_exit");
    let mode = get(&base, "mode").await;
    assert_eq!(mode["devices"][0]["session"], closed);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "new authority must be visible within one second"
    );
    assert!(hm_core::session::SessionStore::new(state.store.clone())
        .history("dev:dot2-d", 100)
        .unwrap()
        .is_empty());
    assert!(stops_within(&gw, Duration::from_millis(100))
        .await
        .contains(&play));
    let repeated: Value = client
        .post(format!("{base}/api/v0/session/close"))
        .json(&json!({"device":"dot2-d"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        repeated["session"], closed,
        "an already closed session is idempotent"
    );
    assert_eq!(
        client
            .post(format!("{base}/api/v0/session/close"))
            .json(&json!({"device":"missing"}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn zero_credits_accepts_a_first_request_then_requires_another_wake() {
    let (base, state, gw) = app(FakeLlm::new(vec![])).await;
    let response = put(&base, json!({"followup_credits":0})).await;
    assert_eq!(response.status(), 200, "zero is a valid followup budget");
    explicit_request(&state, &gw, 1, "现在几点").await;
    let (play, meta) = gw.until_play("voice").await;
    assert!(
        meta["text"].as_str().unwrap().contains('点'),
        "first request must run"
    );
    d1_finish_voice(&gw, &play).await;
    // PlayFinished is asynchronous; freeze the next input only after the
    // owner has revoked the prior capture lease for this single request.
    tokio::time::timeout(Duration::from_secs(1), async {
        while session(&state)["phase"] != "closed" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("zero credits close after the answer finishes");

    text(&state, &gw, 2, "现在几点").await;
    d0_wait_note(&state, "input_admission", |e| e["decision"] == "deny").await;
    assert_eq!(
        state
            .voice
            .recent(100)
            .iter()
            .filter(|e| e["kind"] == "clock_answered_locally")
            .count(),
        1
    );
    assert_eq!(session(&state)["credits"], 0);
    assert_eq!(session(&state)["last_denied"]["hint"], "请重新唤醒后再提问");
    explicit_request(&state, &gw, 3, "现在几点").await;
    let (next, _) = gw.until_play("voice").await;
    assert_ne!(
        next, play,
        "another explicit wake admits the next first question"
    );
}

#[tokio::test]
async fn prompts_roundtrip_history_backup_restore_and_legacy_defaults() {
    let (base, state, _gw) = app(FakeLlm::new(vec![])).await;
    let persona = "叫我老板，回答简洁。";
    let override_text = "自定义系统骨架";
    put(
        &base,
        json!({"persona_prompt":persona,"system_prompt_override":override_text}),
    )
    .await
    .error_for_status()
    .unwrap();
    let mode = get(&base, "mode").await;
    assert_eq!(mode["persona_prompt"], persona);
    assert_eq!(mode["system_prompt_override"], override_text);
    let row = state
        .config
        .history(100)
        .unwrap()
        .into_iter()
        .find(|r| r["scope"] == "voice" && r["payload"]["persona_prompt"] == persona)
        .unwrap();
    assert_eq!(row["payload"]["system_prompt_override"], override_text);
    let backup = get(&base, "config/export").await;
    assert_eq!(backup["mode"]["persona_prompt"], persona);
    assert_eq!(backup["mode"]["system_prompt_override"], override_text);
    put(
        &base,
        json!({"persona_prompt":"","system_prompt_override":null}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(get(&base, "mode").await["persona_prompt"], "");
    assert!(get(&base, "mode").await["system_prompt_override"].is_null());
    reqwest::Client::new()
        .post(format!(
            "{base}/api/v0/config/history/{}/restore",
            row["id"]
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(get(&base, "mode").await["persona_prompt"], persona);
    put(
        &base,
        json!({"persona_prompt":"临时个性","system_prompt_override":""}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert!(get(&base, "mode").await["system_prompt_override"].is_null());
    reqwest::Client::new()
        .post(format!("{base}/api/v0/config/import"))
        .json(&json!({"mode":backup["mode"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        get(&base, "mode").await["system_prompt_override"],
        override_text
    );
    let mut legacy = backup["mode"].clone();
    legacy.as_object_mut().unwrap().remove("persona_prompt");
    legacy
        .as_object_mut()
        .unwrap()
        .remove("system_prompt_override");
    legacy["followup_credits"] = json!(0);
    reqwest::Client::new()
        .post(format!("{base}/api/v0/config/import"))
        .json(&json!({"mode":legacy}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mode = get(&base, "mode").await;
    assert_eq!(mode["persona_prompt"], "");
    assert!(mode["system_prompt_override"].is_null());
    assert_eq!(mode["followup_credits"], 0);
}

#[tokio::test]
async fn invalid_prompt_lengths_reject_all_configuration_entry_points_without_partial_writes() {
    let (base, state, _gw) = app(FakeLlm::new(vec![])).await;
    let before = serde_json::to_value(state.voice.settings_snapshot()).unwrap();
    for field in ["persona_prompt", "system_prompt_override"] {
        let mut invalid = before.clone();
        invalid[field] = json!("字".repeat(4001));
        hm_core::config::append_history(&state.store, "test", "voice", "invalid prompt", &invalid)
            .unwrap();
        let row = state
            .config
            .history(100)
            .unwrap()
            .into_iter()
            .filter(|r| r["summary"] == "invalid prompt")
            .max_by_key(|r| r["id"].as_i64().unwrap())
            .unwrap();
        let history_count = state.config.history(100).unwrap().len();
        let persisted = state.store.config_get("voice_mode").unwrap();
        let mut patch = json!({"asr_wake_fallback":false,"followup_credits":0});
        patch[field] = invalid[field].clone();
        assert_eq!(put(&base, patch).await.status(), 400, "invalid PUT {field}");
        for source in [None, Some("d3-peer")] {
            let mut request = reqwest::Client::new().post(format!("{base}/api/v0/config/import"));
            if let Some(source) = source {
                request = request.header("X-Home-Mate-Source", source);
            }
            assert_eq!(request.json(&json!({"mode":invalid,"devices":{"dot2-d":{"name":"must not apply","room":""}}}))
                .send().await.unwrap().status(),400);
        }
        assert_eq!(
            reqwest::Client::new()
                .post(format!(
                    "{base}/api/v0/config/history/{}/restore",
                    row["id"]
                ))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            serde_json::to_value(state.voice.settings_snapshot()).unwrap(),
            before
        );
        assert_eq!(state.store.config_get("voice_mode").unwrap(), persisted);
        assert_eq!(state.config.history(100).unwrap().len(), history_count);
        assert_eq!(state.store.config_get("device_meta").unwrap(), None);
    }
    put(
        &base,
        json!({"persona_prompt":"字".repeat(4000),"system_prompt_override":"字".repeat(4000)}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        get(&base, "mode").await["persona_prompt"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        4000
    );
}

#[tokio::test]
async fn persona_applies_to_the_next_turn_and_override_keeps_structural_contracts() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["第一轮".into()],
        },
        Scripted::Text {
            chunks: vec!["第二轮".into()],
        },
        Scripted::Text {
            chunks: vec!["第三轮".into()],
        },
    ]);
    let seen = llm.clone();
    let (base, state, gw) = app(llm).await;
    put(&base, json!({"persona_prompt":"叫我老板"}))
        .await
        .error_for_status()
        .unwrap();
    explicit_request(&state, &gw, 1, "介绍一下自己").await;
    let (play, _) = gw.until_play("voice").await;
    d1_finish_voice(&gw, &play).await;
    let first = seen.seen_requests()[0][0].content.clone().unwrap();
    assert!(first.contains("叫我老板"));
    assert!(first.find("叫我老板").unwrap() < first.find("可用能力").unwrap());
    put(
        &base,
        json!({"persona_prompt":"","system_prompt_override":"自定义骨架标记"}),
    )
    .await
    .error_for_status()
    .unwrap();
    explicit_request(&state, &gw, 2, "再介绍一次").await;
    let (play, _) = gw.until_play("voice").await;
    d1_finish_voice(&gw, &play).await;
    let second = seen.seen_requests()[1][0].content.clone().unwrap();
    assert!(second.contains("自定义骨架标记"));
    assert!(!second.contains("叫我老板"));
    for clause in [
        "可用能力",
        "session.close",
        "这是语音对话",
        "系统会直接播报动作事实",
        "关闭会话这个词什么意思",
    ] {
        assert!(
            second.contains(clause),
            "override must preserve {clause}: {second}"
        );
    }
    assert!(second.find("自定义骨架标记").unwrap() < second.find("可用能力").unwrap());
    put(&base, json!({"system_prompt_override":null}))
        .await
        .error_for_status()
        .unwrap();
    explicit_request(&state, &gw, 3, "再说一次").await;
    let (_play, _) = gw.until_play("voice").await;
    let third = seen.seen_requests()[2][0].content.clone().unwrap();
    assert!(!third.contains("自定义骨架标记"));
    assert!(third.contains("行为准则"));
}

#[tokio::test]
async fn model_session_close_really_revokes_and_discards_prose() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("session.close".into(), "{}".into())],
        },
        Scripted::Text {
            chunks: vec!["已经退下的模型散文不应保存或播放".into()],
        },
    ]);
    let seen = llm.clone();
    let (_base, state, gw) = app(llm).await;
    explicit_request(&state, &gw, 1, "行了别说了").await;
    let event = d0_wait_note(&state, "session_close_tool", |_| true).await;
    assert_eq!(event["confirmed"], true);
    d0_wait_note(&state, "session_retreat", |e| e["via"] == "model").await;
    assert_eq!(session(&state)["phase"], "closed");
    assert_eq!(session(&state)["credits"], 0);
    let (receipt_play, meta) = gw.until_play("voice").await;
    assert_eq!(
        meta["text"], "会话已结束。好嘞",
        "the core owns the spoken action fact"
    );
    d1_finish_voice(&gw, &receipt_play).await;
    assert_eq!(
        session(&state)["phase"],
        "closed",
        "the close receipt cannot reopen the session"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        seen.seen_requests().len(),
        1,
        "successful exit must not ask the model for exit prose"
    );
    let history = hm_core::session::SessionStore::new(state.store.clone())
        .history("dev:dot2-d", 100)
        .unwrap();
    assert!(!history
        .iter()
        .any(|m| m.content.as_deref().is_some_and(|c| c.contains("模型散文"))));
    let receipt = history
        .iter()
        .find(|m| m.role == "tool")
        .and_then(|m| m.content.as_deref())
        .expect("closed turn keeps the core tool receipt");
    let receipt: Value = serde_json::from_str(receipt).unwrap();
    assert_eq!(receipt["closed"], true);
    assert_eq!(receipt["confirmed"], true);
    assert_eq!(receipt["scope"], "device");
    let traces = state.traces.list(10).unwrap();
    assert!(!serde_json::to_string(&traces).unwrap().contains("模型散文"));
}

#[tokio::test]
async fn model_confirmation_is_two_distinct_turns_and_questions_do_not_close() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("session.close".into(), r#"{"confirm":true}"#.into())],
        },
        Scripted::Text {
            chunks: vec!["散文不应替代确认句".into()],
        },
        Scripted::Calls {
            calls: vec![("session.close".into(), r#"{"confirm":true}"#.into())],
        },
    ]);
    let (_base, state, gw) = app(llm).await;
    explicit_request(&state, &gw, 1, "你是不是该歇会儿了").await;
    let (play, meta) = gw.until_play("voice").await;
    assert!(
        meta["text"].as_str().unwrap().contains("结束"),
        "core owns the confirmation question: {meta}"
    );
    assert_eq!(session(&state)["phase"], "active");
    assert!(!state
        .voice
        .recent(100)
        .iter()
        .any(|e| e["kind"] == "session_retreat"));
    d1_finish_voice(&gw, &play).await;
    text(&state, &gw, 2, "确认结束对话").await;
    d0_wait_note(&state, "session_close_tool", |e| e["confirmed"] == true).await;
    d0_wait_note(&state, "session_retreat", |e| e["via"] == "model").await;
    assert_eq!(session(&state)["phase"], "closed");

    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["这是指结束当前对话的说法。".into()],
    }]);
    let seen = llm.clone();
    let (_base, state, gw) = app(llm).await;
    explicit_request(&state, &gw, 1, "关闭会话这个词什么意思").await;
    let (question_play, meta) = gw.until_play("voice").await;
    assert_eq!(meta["text"], "这是指结束当前对话的说法。");
    assert!(!state
        .voice
        .recent(100)
        .iter()
        .any(|e| e["kind"] == "session_close_tool"));
    assert_eq!(session(&state)["phase"], "active");
    let prompt = seen.seen_requests()[0][0].content.clone().unwrap();
    assert!(prompt.contains("关闭会话这个词什么意思"));
    assert!(prompt.contains("session.close"));
    d1_finish_voice(&gw, &question_play).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while session(&state)["phase"] != "tail" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("only the complete answer creates the tail phase");
    let deadline = session(&state)["tail_deadline_ms"].as_u64().unwrap();
    let now = hm_core::id::now_ms();
    assert!(
        (8_000..=9_000).contains(&deadline.saturating_sub(now)),
        "a complete valid answer must expose its full configured tail deadline"
    );
}

#[tokio::test]
async fn model_cannot_confirm_its_own_close_in_one_tool_batch() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![
                ("session.close".into(), r#"{"confirm":true}"#.into()),
                ("session.close".into(), r#"{"confirm":true}"#.into()),
            ],
        },
        Scripted::Text {
            chunks: vec!["模型的确认散文".into()],
        },
    ]);
    let (_base, state, gw) = app(llm).await;
    explicit_request(&state, &gw, 1, "是不是该歇会儿了").await;
    let (_play, meta) = gw.until_play("voice").await;
    assert!(
        meta["text"].as_str().unwrap().contains("结束"),
        "core must ask the person: {meta}"
    );
    assert_eq!(session(&state)["phase"], "active");
    assert!(
        !state
            .voice
            .recent(100)
            .iter()
            .any(|e| e["kind"] == "session_retreat"),
        "a second call in the same model turn is not a second human turn"
    );
}

#[tokio::test]
async fn pending_close_confirmation_cannot_cross_a_new_activation() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("session.close".into(), r#"{"confirm":true}"#.into())],
        },
        Scripted::Text {
            chunks: vec!["第一轮模型散文".into()],
        },
        Scripted::Calls {
            calls: vec![("session.close".into(), r#"{"confirm":true}"#.into())],
        },
        Scripted::Text {
            chunks: vec!["新会话模型散文".into()],
        },
    ]);
    let (_base, state, gw) = app(llm).await;
    explicit_request(&state, &gw, 1, "是不是该歇会儿了").await;
    let (play, meta) = gw.until_play("voice").await;
    assert!(meta["text"].as_str().unwrap().contains("结束"));
    d1_finish_voice(&gw, &play).await;
    let old_generation = session(&state)["generation"].as_u64().unwrap();
    explicit_request(&state, &gw, 2, "想一下是不是该歇会儿了").await;
    let (_play, meta) = gw.until_play("voice").await;
    assert!(
        meta["text"].as_str().unwrap().contains("结束"),
        "new generation must ask again: {meta}"
    );
    let current = session(&state);
    assert!(current["generation"].as_u64().unwrap() > old_generation);
    assert_eq!(current["phase"], "active");
    assert!(!state
        .voice
        .recent(100)
        .iter()
        .any(|e| e["kind"] == "session_retreat"));
}

#[tokio::test]
async fn an_intervening_question_expires_pending_close_confirmation() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("session.close".into(), r#"{"confirm":true}"#.into())],
        },
        Scripted::Text {
            chunks: vec!["第一轮模型散文".into()],
        },
        Scripted::Text {
            chunks: vec!["彩虹来自水滴对光的折射和反射。".into()],
        },
        Scripted::Calls {
            calls: vec![("session.close".into(), r#"{"confirm":true}"#.into())],
        },
        Scripted::Text {
            chunks: vec!["末轮模型散文".into()],
        },
    ]);
    let (_base, state, gw) = app(llm).await;
    explicit_request(&state, &gw, 1, "是不是该歇会儿了").await;
    let (first, meta) = gw.until_play("voice").await;
    assert!(meta["text"].as_str().unwrap().contains("结束"));
    d1_finish_voice(&gw, &first).await;
    text(&state, &gw, 2, "解释一下彩虹怎么形成").await;
    let (second, meta) = gw.until_play("voice").await;
    assert_eq!(meta["text"], "彩虹来自水滴对光的折射和反射。");
    d1_finish_voice(&gw, &second).await;
    d0_wait_note(&state, "play_finished", |e| e["play_id"] == second).await;
    text(&state, &gw, 3, "想想看该不该结束对话").await;
    let (_third, meta) = gw.until_play("voice").await;
    assert!(
        meta["text"].as_str().unwrap().contains("结束"),
        "an old pending question cannot authorize this later request: {meta}"
    );
    assert_eq!(session(&state)["phase"], "active");
    assert!(!state
        .voice
        .recent(100)
        .iter()
        .any(|e| e["kind"] == "session_retreat"));
}

#[tokio::test]
async fn headless_prompt_api_edits_clears_records_history_and_rejects_invalid_values() {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        std::env::temp_dir().join(hm_core::id::next("hm-d3-headless")),
        MockInject::default(),
        Some(Arc::new(FakeLlm::new(vec![]))),
        Some(Arc::new(MockTts::default())),
        None,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = hm_core::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    let url = format!("{base}/api/v0/prompts");
    assert!(
        get(&base, "mode").await["devices"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the endpoint must not need a connected audio device"
    );
    let response: Value = client.put(&url)
        .json(&json!({"persona_prompt":"  叫我老板  ","system_prompt_override":"  无设备的系统骨架  "}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(response["persona_prompt"], "叫我老板");
    assert_eq!(response["system_prompt_override"], "无设备的系统骨架");
    let saved = state
        .config
        .history(100)
        .unwrap()
        .into_iter()
        .find(|r| {
            r["scope"] == "voice"
                && r["actor"] == "console"
                && r["payload"]["persona_prompt"] == "叫我老板"
        })
        .unwrap();
    assert_eq!(
        saved["payload"]["system_prompt_override"],
        "无设备的系统骨架"
    );
    client
        .put(&url)
        .json(&json!({"persona_prompt":""}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mode = get(&base, "mode").await;
    assert_eq!(mode["persona_prompt"], "");
    assert_eq!(
        mode["system_prompt_override"], "无设备的系统骨架",
        "omitting override preserves it"
    );
    client
        .put(&url)
        .json(&json!({"system_prompt_override":null}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(get(&base, "mode").await["system_prompt_override"].is_null());
    client
        .post(format!(
            "{base}/api/v0/config/history/{}/restore",
            saved["id"]
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(get(&base, "mode").await["persona_prompt"], "叫我老板");
    assert_eq!(
        get(&base, "mode").await["system_prompt_override"],
        "无设备的系统骨架"
    );
    // A full snapshot with a blank override must normalize just like a patch.
    let mut snapshot = serde_json::to_value(state.voice.settings_snapshot()).unwrap();
    snapshot["system_prompt_override"] = json!("   ");
    client
        .post(format!("{base}/api/v0/config/import"))
        .json(&json!({"mode":snapshot}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(get(&base, "mode").await["system_prompt_override"].is_null());
    let persisted: Value =
        serde_json::from_str(&state.store.config_get("voice_mode").unwrap().unwrap()).unwrap();
    assert!(persisted["system_prompt_override"].is_null());
    assert_eq!(persisted["persona_prompt"], "叫我老板");
    let before = serde_json::to_value(state.voice.settings_snapshot()).unwrap();
    let history_len = state.config.history(100).unwrap().len();
    let before_persisted = state.store.config_get("voice_mode").unwrap();
    for field in ["persona_prompt", "system_prompt_override"] {
        let mut invalid = json!({"persona_prompt":"must not apply"});
        invalid[field] = json!("字".repeat(4001));
        let response = client.put(&url).json(&invalid).send().await.unwrap();
        assert_eq!(
            response.status(),
            400,
            "invalid {field} must fail before any change"
        );
        assert_eq!(
            serde_json::to_value(state.voice.settings_snapshot()).unwrap(),
            before
        );
        assert_eq!(
            state.store.config_get("voice_mode").unwrap(),
            before_persisted
        );
        assert_eq!(state.config.history(100).unwrap().len(), history_len);
    }
    assert!(get(&base, "mode").await["devices"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn zero_credits_closed_session_rejects_a_real_pcm_asr_followup_with_a_rewake_hint() {
    let llm = FakeLlm::new(vec![]);
    let seen = llm.clone();
    let asr = CountingAsr::new("解释一下彩虹怎么形成");
    let (base, state, gw) = spawn_mic_app_asr(llm, asr.clone()).await;
    put(
        &base,
        json!({"followup_credits":0,"asr_wake_fallback":true,"tail_blind_ms":0}),
    )
    .await
    .error_for_status()
    .unwrap();
    explicit_request(&state, &gw, 1, "现在几点").await;
    let (play, meta) = gw.until_play("voice").await;
    assert!(meta["text"].as_str().unwrap().contains('点'));
    d1_finish_voice(&gw, &play).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while session(&state)["phase"] != "closed" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("zero credits must close after the first complete answer");
    let closed = session(&state);
    assert_eq!(closed["close_reason"], "single_request");
    assert_eq!(closed["credits"], 0);
    assert!(closed["last_denied"].is_null());
    let lease = state.voice.capture_lease("dot2-d");
    let input = gw.capture_input();
    // Freeze a fresh post-close capture grant at the audio source. Send actual
    // PCM through TCP, VAD and ASR; no injected transcript or legacy fallback.
    for block in 0..18 {
        let samples = if block < 10 {
            loud_block()
        } else {
            quiet_block()
        };
        let mut frame = input.pcm(100 + block, block * 1280, &samples);
        frame["origin"] = json!({"kind":"leased","lease":lease,"origin":frame["origin"]});
        gw.send_raw_json(frame).await;
    }
    let transcript = d0_wait_note(&state, "mic_transcript_source", |e| {
        e["ticket"]["spans"][0]["provenance"]["origin"]["origin"]["input_id"] == input.input_id
    })
    .await;
    assert_eq!(asr.calls(), 1, "a real audio segment must have reached ASR");
    let spans = transcript["ticket"]["spans"].as_array().unwrap();
    assert!(!spans.is_empty());
    assert!(
        spans.iter().all(|s| s.get("control_only").is_none()),
        "SourceSpan omits its false control_only flag; this must exercise ordinary closed-session rejection"
    );
    d0_wait_note(&state, "mic_ignored_unwoken", |e| {
        e["text"] == "解释一下彩虹怎么形成"
    })
    .await;
    assert!(
        !state
            .voice
            .recent(100)
            .iter()
            .any(|e| e["kind"] == "mic_control_only_rejected"),
        "the complete ordinary ASR admission path must reach its final closed-session rejection"
    );
    let denied = session(&state);
    assert_eq!(denied["generation"], closed["generation"]);
    assert_eq!(denied["phase"], "closed");
    assert_eq!(denied["credits"], 0);
    assert_eq!(denied["last_denied"]["reason"], "session_closed");
    assert_eq!(denied["last_denied"]["hint"], "请重新唤醒后再提问");
    assert!(
        denied["last_denied"]["at_ms"].as_u64().unwrap() >= closed["since_ms"].as_u64().unwrap()
    );
    assert_eq!(
        state
            .voice
            .recent(100)
            .iter()
            .filter(|e| e["kind"] == "request_started")
            .count(),
        1
    );
    assert!(
        seen.seen_requests().is_empty(),
        "denied naked speech cannot start another model turn"
    );
}
