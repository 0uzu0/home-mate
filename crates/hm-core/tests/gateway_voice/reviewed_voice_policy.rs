use super::*;
use hm_core::config_delivery::{uuid, ConfigApplyCoordinator};

async fn read(request: reqwest::RequestBuilder) -> Value {
    let response = request.send().await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    body
}

#[tokio::test]
async fn reviewed_voice_policy_reaches_live_turns_and_rolls_back() {
    let llm = FakeLlm::new(
        (0..4)
            .map(|_| Scripted::Text {
                chunks: vec!["回答。".into()],
            })
            .collect(),
    );
    let (base, state, gateway) = spawn_app_with_gateway(llm.clone()).await;
    let client = reqwest::Client::new();
    let pairing = read(client.post(format!("{base}/api/v0/auth/provision"))).await;
    let bearer = pairing["token"].as_str().unwrap();
    let put = |path: &str| client.put(format!("{base}{path}")).bearer_auth(bearer);
    let post = |path: &str| client.post(format!("{base}{path}")).bearer_auth(bearer);
    let get = |path: &str| client.get(format!("{base}{path}")).bearer_auth(bearer);
    let preflight = |mode: Value| {
        post("/api/v1/config/preflight").json(&json!({
            "package":{"format":"home-mate-config","version":1,"mode":mode}
        }))
    };
    let apply = |plan: &Value| {
        post("/api/v1/config/applications").json(&json!({
            "plan_id":plan["plan_id"], "plan_hash":plan["plan_hash"],
            "expected_revision":plan["expected_revision"], "application_id":uuid().unwrap()
        }))
    };
    let readback = |id: &str| get(&format!("/api/v1/config/applications/{id}/readback"));
    let await_state = async |receipt: &Value, expected: &str| {
        let id = receipt["application_id"].as_str().unwrap();
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let proof = read(readback(id)).await;
                assert_eq!(proof["publications"][0]["block"], "mode");
                if proof["publications"][0]["state"] == expected {
                    return proof;
                }
                assert_eq!(
                    proof["publications"][0]["state"], "saved_pending",
                    "{proof}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    };

    read(put("/api/v0/mode").json(&json!({
        "assistant_name":"Existing Speaker", "persona_prompt":"Existing persona note",
        "follow_window_enabled":true, "followup_credits":3, "tail_blind_ms":333,
    })))
    .await;
    read(put("/api/v0/fleet").json(&json!({"self_id":"dot2-d"}))).await;
    read(put("/api/v1/config/enrollment").json(&json!({}))).await;
    let protected = preflight(json!({"tail_blind_ms":400}))
        .send()
        .await
        .unwrap();
    assert_eq!(protected.status(), 400);
    let mut family = read(post("/api/v1/config/exports").json(&json!({
        "format":"v2", "purpose":"family_policy", "blocks":["mode"]
    })))
    .await["package"]
        .clone();
    family.as_object_mut().unwrap().remove("content_hash");
    family["blocks"][0]["operations"] = json!([
        {"action":"set", "path":"mode.system_prompt_override", "value":"not family authority"}
    ]);
    let unauthorized = post("/api/v1/config/preflight")
        .json(&json!({"package":family}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 400);
    let inert = preflight(json!({"proactive_speech":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(inert.status(), 400);
    let malformed = put("/api/v0/mode/dot2-d")
        .json(&json!({"assistant_name":"invalid\0name"}))
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), 400);
    let invalid_budget =
        preflight(json!({"spoken":{"detail_max_chars":400,"backstop_max_chars":220}}))
            .send()
            .await
            .unwrap();
    assert_eq!(invalid_budget.status(), 400);
    let opaque = post("/api/v1/config/preflight").json(&json!({
        "package":{"format":"home-mate-config","version":1,"mode_policy":{"spoken":{"detail_words":["旧用途"]}}}
    })).send().await.unwrap();
    assert_eq!(opaque.status(), 400);
    let invalid = preflight(json!({"assistant_name":"discard", "follow_window_ms":17}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    assert_eq!(
        state.voice.settings_snapshot().assistant_name,
        "Existing Speaker"
    );

    let plan = read(preflight(json!({
        "assistant_name":"Reviewed Speaker", "persona_prompt":"Reviewed persona note",
        "system_prompt_override":null, "follow_window_enabled":true, "followup_credits":0,
        "follow_window_ms":9000, "context_window_ms":180000, "follow_window_tone":false,
        "stop_intents":{"enabled":true,"stop_words":["收住"],"continue_words":["复听"]},
        "spoken":{"max_sentences":3,"max_chars":90,"detail_max_sentences":5,"detail_max_chars":240,
            "continue_hint":"需要时说复听。","resume_lead":"接着上回说。",
            "backstop_max_sentences":36,"backstop_max_chars":1200}
    })))
    .await;
    let receipt = read(apply(&plan)).await;
    assert_eq!(receipt["state"], "saved_pending");
    assert_eq!(
        (read(readback(receipt["application_id"].as_str().unwrap())).await)["publications"][0]
            ["state"],
        "saved_pending"
    );
    assert_eq!(state.voice.settings_snapshot().tail_blind_ms, 333);
    assert_eq!(state.voice.settings_snapshot().followup_credits, Some(3));

    let voice_turn =
        async |seq: u64, name: &str, persona: &str, short: &str, detail: &str, finish: bool| {
            explicit_request(&state, &gateway, seq, "请回答").await;
            let (play, _) = gateway.until_any_voice_play().await;
            gateway.confirm_play(&play).await;
            if finish {
                d1_finish_voice(&gateway, &play).await;
            }
            let seen = llm.seen_requests();
            let prompt = serde_json::to_string(seen.last().unwrap()).unwrap();
            assert!(
                prompt.contains(name)
                    && prompt.contains(persona)
                    && prompt.contains(short)
                    && prompt.contains(detail)
                    && prompt.contains(if name == "Reviewed Speaker" {
                        "用户说「复听」时接着播"
                    } else {
                        "用户说「继续」时接着播"
                    }),
                "{prompt}"
            );
            play
        };
    voice_turn(
        1,
        "Existing Speaker",
        "Existing persona note",
        "最多 2 句话、总长不超过 120 字",
        "最多 6 句话、总长不超过 300 字",
        true,
    )
    .await;
    let _worker =
        ConfigApplyCoordinator::start_with_voice(state.store.clone(), state.voice.clone());
    let proof = await_state(&receipt, "applied").await;
    assert!(
        proof["fields"]
            .as_array()
            .unwrap()
            .iter()
            .all(|field| field["state"] == "core_readback_verified"),
        "{proof}"
    );
    let scoped = proof["fields"].as_array().unwrap();
    for (path, value) in [
        ("mode.spoken.backstop_max_chars", json!(1200)),
        ("mode.spoken.resume_lead", json!("接着上回说。")),
    ] {
        assert!(
            scoped
                .iter()
                .any(|item| item["path"] == path && item["value"] == value),
            "{proof}"
        );
    }
    let active = read(get("/api/v0/mode")).await;
    assert_eq!(active["followup_credits"], 0);
    assert_eq!(active["devices"][0]["session"]["credits"], 0);
    assert_eq!(active["tail_blind_ms"], 333);
    let live_play = voice_turn(
        2,
        "Reviewed Speaker",
        "Reviewed persona note",
        "最多 3 句话、总长不超过 90 字",
        "最多 5 句话、总长不超过 240 字",
        false,
    )
    .await;
    let calls_before = llm.seen_requests().len();
    explicit_request(&state, &gateway, 3, "收住").await;
    let stop = gateway
        .until(|frame| {
            matches!(frame,
        CoreFrame::Stop { task_id, .. } if task_id == &live_play)
        })
        .await;
    assert!(matches!(stop, CoreFrame::Stop { .. }));
    gateway
        .send(&GatewayFrame::StopReceipt {
            task_id: live_play,
            stage: "device_confirmed".into(),
            played_ms: Some(0),
            consumed_ms: None,
            progress_basis: None,
            detail: None,
        })
        .await;
    assert_eq!(
        llm.seen_requests().len(),
        calls_before,
        "recognized stop command stays local"
    );
    explicit_request(&state, &gateway, 4, "复听").await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state
                .voice
                .recent(200)
                .iter()
                .any(|event| event["kind"] == "continue_intent" && event["said"] == "复听")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (reply_play, _) = gateway.until_any_voice_play().await;
    gateway.confirm_play(&reply_play).await;
    d1_finish_voice(&gateway, &reply_play).await;

    for (format, content) in [("v1", "document"), ("v2", "package")] {
        let export =
            read(post("/api/v1/config/exports").json(&json!({"format":format,"blocks":["mode"]})))
                .await;
        if format == "v1" {
            assert!(export["document"].get("mode").is_none());
            assert!(export["document"]["mode_policy"].is_object());
            assert!(export["document"]["mode_policy"]
                .get("tail_blind_ms")
                .is_none());
        } else {
            assert!(export["package"]["blocks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|block| block["operations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|op| op["path"] != "mode.tail_blind_ms")));
        }
        read(post("/api/v1/config/preflight").json(&json!({"package":export[content]}))).await;
    }

    let reversal = read(
        post(&format!(
            "/api/v1/config/applications/{}/rollback-preflight",
            receipt["application_id"].as_str().unwrap()
        ))
        .json(&json!({"expected_revision":receipt["config_revision"]})),
    )
    .await;
    let restored = read(apply(&reversal)).await;
    await_state(&restored, "applied").await;
    assert_eq!(state.voice.settings_snapshot().followup_credits, Some(3));
    assert_eq!(state.voice.settings_snapshot().tail_blind_ms, 333);
    voice_turn(
        5,
        "Existing Speaker",
        "Existing persona note",
        "最多 2 句话、总长不超过 120 字",
        "最多 6 句话、总长不超过 300 字",
        true,
    )
    .await;

    let export =
        read(post("/api/v1/config/exports").json(&json!({"format":"v1","blocks":["mode"]}))).await;
    assert!(export["loss_report"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["domain"] == "mode" && entry["status"] == "partial"));
    let unscoped = post("/api/v0/config/import")
        .json(&json!({"mode_policy":{"tail_blind_ms":100}}))
        .send()
        .await
        .unwrap();
    assert_eq!(unscoped.status(), 400);
    let before = state.voice.settings_snapshot();
    let imported = read(post("/api/v0/config/import").json(&export["document"])).await;
    assert!(imported["applied"]
        .as_array()
        .unwrap()
        .contains(&json!("mode")));
    let after = state.voice.settings_snapshot();
    assert_eq!(after.tail_blind_ms, 333);
    assert_eq!(after.wake_words, before.wake_words);
    assert_eq!(after.duplex_requested, before.duplex_requested);
    assert_eq!(
        serde_json::to_value(after.filler).unwrap(),
        serde_json::to_value(before.filler).unwrap()
    );

    state.store.with(|c| {
        c.execute_batch("CREATE TRIGGER block_voice_history BEFORE INSERT ON config_history WHEN NEW.scope='voice' BEGIN SELECT RAISE(ABORT, 'voice test failure'); END")?;
        Ok(())
    }).unwrap();
    let failed_edit = put("/api/v0/mode/dot2-d")
        .json(&json!({"assistant_name":"direct failure"}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed_edit.status(), 400);
    assert_eq!(
        state.voice.settings_snapshot().assistant_name,
        "Existing Speaker"
    );

    let plan = read(preflight(json!({"assistant_name":"publication failure"}))).await;
    let failed = read(apply(&plan)).await;
    let proof = await_state(&failed, "failed").await;
    assert_eq!(
        proof["publications"][0]["error"],
        "VOICE_PUBLICATION_FAILED"
    );
    assert_eq!(
        state.voice.settings_snapshot().assistant_name,
        "Existing Speaker"
    );
    let saved: hm_core::voice::VoiceSettings =
        serde_json::from_str(&state.store.config_get("voice_mode").unwrap().unwrap()).unwrap();
    assert_eq!(saved.assistant_name, "Existing Speaker");
}

#[tokio::test]
async fn reviewed_voice_policy_preserves_older_settings_across_startup_and_reset() {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    let mut legacy = hm_core::voice::VoiceSettings::default();
    legacy.assistant_name = "Legacy speaker".into();
    legacy.tail_blind_ms = 333;
    legacy.spoken.detail_words = hm_core::voice::legacy_default_detail_words();
    let mut persisted = serde_json::to_value(&legacy).unwrap();
    persisted["future_unit_metadata"] = json!({"trim": -7});
    persisted["spoken"]["future_delivery_tag"] = json!({"format":["legacy","opaque"]});
    store
        .config_set("voice_mode", &persisted.to_string())
        .unwrap();
    let data_dir = std::env::temp_dir().join(format!("hm-voice-policy-{}", uuid().unwrap()));
    let state = AppState::with_overrides(
        store.clone(),
        data_dir.clone(),
        MockInject::default(),
        Some(Arc::new(FakeLlm::default())),
        Some(Arc::new(MockTts::default())),
        None,
        None,
    );
    let upgraded_words = state.voice.settings_snapshot().spoken.detail_words;
    assert_ne!(upgraded_words, legacy.spoken.detail_words);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let api = hm_core::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let client = reqwest::Client::new();
    let pairing = read(client.post(format!("{base}/api/v0/auth/provision"))).await;
    let bearer = pairing["token"].as_str().unwrap();
    let put = |path: &str| client.put(format!("{base}{path}")).bearer_auth(bearer);
    let post = |path: &str| client.post(format!("{base}{path}")).bearer_auth(bearer);
    let get = |path: &str| client.get(format!("{base}{path}")).bearer_auth(bearer);
    read(put("/api/v0/fleet").json(&json!({"self_id":"dot2-d"}))).await;
    read(put("/api/v1/config/enrollment").json(&json!({}))).await;
    let plan = read(post("/api/v1/config/preflight").json(&json!({
        "package":{"format":"home-mate-config","version":1,"mode":{"assistant_name":"New speaker"}}
    })))
    .await;
    let id = uuid().unwrap();
    read(post("/api/v1/config/applications").json(&json!({
        "plan_id":plan["plan_id"], "plan_hash":plan["plan_hash"],
        "expected_revision":plan["expected_revision"], "application_id":id
    })))
    .await;
    let _worker = ConfigApplyCoordinator::start_with_voice(store.clone(), state.voice.clone());
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let proof = read(get(&format!("/api/v1/config/applications/{id}/readback"))).await;
            let publication = &proof["publications"][0];
            if publication["state"] == "applied" {
                break;
            }
            assert_eq!(publication["state"], "saved_pending", "{proof}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let current: Value =
        serde_json::from_str(&store.config_get("voice_mode").unwrap().unwrap()).unwrap();
    assert_eq!(current["spoken"], persisted["spoken"]);
    assert_eq!(
        current["future_unit_metadata"],
        persisted["future_unit_metadata"]
    );
    assert_eq!(current["tail_blind_ms"], persisted["tail_blind_ms"]);
    assert_eq!(
        state.voice.settings_snapshot().spoken.detail_words,
        upgraded_words
    );
    assert_eq!(
        state.voice.settings_snapshot().assistant_name,
        "New speaker"
    );

    read(post("/api/v0/config/import").json(&json!({
        "format":"home-mate-config","version":1,"mode_policy":{"assistant_name":"Imported speaker"}
    })))
    .await;
    let imported: Value =
        serde_json::from_str(&store.config_get("voice_mode").unwrap().unwrap()).unwrap();
    assert_eq!(imported["assistant_name"], "Imported speaker");
    assert_eq!(imported["spoken"], persisted["spoken"]);
    read(post("/api/v0/config/import").json(&json!({
        "format":"home-mate-config","version":1,"mode_policy":{"spoken":{"max_chars":96}}
    })))
    .await;
    let scoped: Value =
        serde_json::from_str(&store.config_get("voice_mode").unwrap().unwrap()).unwrap();
    let mut expected = persisted["spoken"].clone();
    expected["max_chars"] = json!(96);
    assert_eq!(scoped["spoken"], expected);
    assert_eq!(state.voice.settings_snapshot().spoken.max_chars, 96);

    assert_eq!(
        imported["future_unit_metadata"],
        persisted["future_unit_metadata"]
    );
    assert_eq!(imported["tail_blind_ms"], persisted["tail_blind_ms"]);

    read(post("/api/v0/system/factory-reset").json(&json!({"confirm":"FACTORY_RESET"}))).await;
    state
        .voice
        .set_prompts(Some("After reset".into()), None, None, "fixture")
        .unwrap();
    let defaults = hm_core::voice::VoiceSettings::default();
    let live = state.voice.settings_snapshot();
    assert_eq!(live.assistant_name, "After reset");
    assert_eq!(live.tail_blind_ms, defaults.tail_blind_ms);
    let saved: Value =
        serde_json::from_str(&store.config_get("voice_mode").unwrap().unwrap()).unwrap();
    assert_eq!(saved["tail_blind_ms"], defaults.tail_blind_ms);
    assert!(saved.get("future_unit_metadata").is_none());
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn reviewed_retreat_policy_reaches_live_asr_and_restores_the_previous_vocabulary() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["现在按普通请求回答。".into()],
    }]);
    let (base, state, gateway) = spawn_mic_app(llm.clone(), "你好高达，辞别试验").await;
    let client = reqwest::Client::new();
    let pairing = read(client.post(format!("{base}/api/v0/auth/provision"))).await;
    let token = pairing["token"].as_str().unwrap();
    let put = |path: &str| client.put(format!("{base}{path}")).bearer_auth(token);
    let post = |path: &str| client.post(format!("{base}{path}")).bearer_auth(token);
    let get = |path: &str| client.get(format!("{base}{path}")).bearer_auth(token);
    let commit = |plan: &Value| {
        post("/api/v1/config/applications").json(&json!({
            "plan_id":plan["plan_id"], "plan_hash":plan["plan_hash"],
            "expected_revision":plan["expected_revision"], "application_id":uuid().unwrap(),
        }))
    };
    let await_applied = async |receipt: &Value| {
        let id = receipt["application_id"].as_str().unwrap();
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let proof = read(get(&format!("/api/v1/config/applications/{id}/readback"))).await;
                if proof["publication"]["state"] == "applied" {
                    return proof;
                }
                assert_eq!(proof["publication"]["state"], "saved_pending", "{proof}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    };
    read(put("/api/v0/fleet").json(&json!({"self_id":"dot2-d"}))).await;
    read(put("/api/v1/config/enrollment").json(&json!({}))).await;
    let baseline = read(get("/api/v0/mode")).await;
    let plan = read(post("/api/v1/config/preflight").json(&json!({
        "package":{"format":"home-mate-config","version":1,"mode":{
            "retreat_intent":true, "retreat_words":["辞别试验"]
        }}
    })))
    .await;
    let receipt = read(commit(&plan)).await;
    assert_eq!(receipt["state"], "saved_pending");
    assert_eq!(
        read(get("/api/v0/mode")).await["retreat_words"],
        baseline["retreat_words"]
    );
    let _worker =
        ConfigApplyCoordinator::start_with_voice(state.store.clone(), state.voice.clone());
    let proof = await_applied(&receipt).await;
    assert!(proof["fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|field| field["path"] == "mode.retreat_words"
            && field["state"] == "core_readback_verified"
            && field["value"] == json!(["辞别试验"])));
    gateway
        .until(|frame| matches!(frame, CoreFrame::MicSession { action, .. } if action == "start"))
        .await;
    let feed = async |start| {
        for seq in start..start + 10 {
            gateway.send(&audio_chunk(seq, &loud_block())).await;
        }
        for seq in start + 10..start + 20 {
            gateway.send(&audio_chunk(seq, &quiet_block())).await;
        }
    };
    feed(1).await;
    let ack = gateway.until(|frame| matches!(
        frame, CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
            if content_ref.starts_with("ack:") && content_ref.contains("好嘞")
    )).await;
    let ack_id = match ack {
        CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } => task_id,
        _ => unreachable!(),
    };
    gateway.confirm_play(&ack_id).await;
    d1_finish_voice(&gateway, &ack_id).await;
    assert!(state
        .voice
        .recent(200)
        .iter()
        .any(|event| event["kind"] == "session_retreat"));
    assert!(llm.seen_requests().is_empty());

    let rollback = read(
        post(&format!(
            "/api/v1/config/applications/{}/rollback-preflight",
            receipt["application_id"].as_str().unwrap()
        ))
        .json(&json!({"expected_revision":receipt["config_revision"]})),
    )
    .await;
    let restored = read(commit(&rollback)).await;
    let proof = await_applied(&restored).await;
    assert!(proof["fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|field| field["path"] == "mode.retreat_words"
            && field["state"] == "core_readback_verified"
            && field["value"] == baseline["retreat_words"]));
    feed(21).await;
    let (play, meta) = gateway.until_play("voice").await;
    assert!(
        meta["text"]
            .as_str()
            .is_some_and(|text| text.contains("普通请求")),
        "{meta}"
    );
    gateway.confirm_play(&play).await;
    d1_finish_voice(&gateway, &play).await;
    assert_eq!(llm.seen_requests().len(), 1);
}

#[tokio::test]
async fn reviewed_mode_cues_preflight_apply_readback_rollback_and_synthesize() {
    let dir = std::env::temp_dir().join(format!("hm-m2-cues-{}", uuid().unwrap()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        dir.clone(),
        MockInject::default(),
        Some(Arc::new(FakeLlm::new(vec![]))),
        Some(Arc::new(MockTts::default())),
        None,
        None,
    );
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::new();
    let pairing = read(client.post(format!("{base}/api/v0/auth/provision"))).await;
    let bearer = pairing["token"].as_str().unwrap();
    let put = |path: &str| client.put(format!("{base}{path}")).bearer_auth(bearer);
    let post = |path: &str| client.post(format!("{base}{path}")).bearer_auth(bearer);
    let get = |path: &str| client.get(format!("{base}{path}")).bearer_auth(bearer);
    let preflight = |mode: Value| {
        post("/api/v1/config/preflight").json(&json!({
            "package":{"format":"home-mate-config","version":1,"mode":mode}
        }))
    };
    let commit = |plan: &Value| {
        post("/api/v1/config/applications").json(&json!({
            "plan_id":plan["plan_id"], "plan_hash":plan["plan_hash"],
            "expected_revision":plan["expected_revision"], "application_id":uuid().unwrap()
        }))
    };
    let await_applied = async |receipt: &Value| {
        let id = receipt["application_id"].as_str().unwrap();
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let proof = read(get(&format!("/api/v1/config/applications/{id}/readback"))).await;
                if proof["publications"][0]["state"] == "applied" {
                    return proof;
                }
                assert_eq!(
                    proof["publications"][0]["state"], "saved_pending",
                    "{proof}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    };

    read(put("/api/v0/fleet").json(&json!({"self_id":"dot2-d"}))).await;
    read(put("/api/v1/config/enrollment").json(&json!({}))).await;

    for invalid in [
        json!({"truncation_notice": ""}),
        json!({"retreat_ack": "超过八个字的结束提示"}),
        json!({"window_close_ack": "bad/ack"}),
        json!({"wake_ack": {"phrases": []}}),
        json!({"wake_ack": {"phrases": ["超过六个字的唤醒"]}}),
        json!({"filler": {"delay_ms": 100}}),
        json!({"wake_ack": {"resynth": true}}),
    ] {
        let resp = preflight(invalid.clone()).send().await.unwrap();
        assert_eq!(resp.status(), 400, "expected 400 for {invalid}");
    }

    let baseline = state.voice.settings_snapshot();
    assert_eq!(state.voice.cue_readiness()["ready"], false);

    let plan = read(preflight(json!({
        "wake_ack": {"enabled": true, "phrases": ["在呢", "请说"]},
        "retreat_ack": "回见",
        "window_close_ack": "再会",
        "truncation_notice": "这一段先播到这里。",
        "filler": {
            "mode": "interrupt",
            "delay_ms": 400,
            "tool_delay_ms": 800,
            "thinking": ["我想一想。"],
            "search": ["我查一查。"],
            "device": ["我看一看。"],
            "story": ["我构思一下。"]
        }
    })))
    .await;
    let receipt = read(commit(&plan)).await;
    assert_eq!(receipt["state"], "saved_pending");

    let _worker =
        ConfigApplyCoordinator::start_with_voice(state.store.clone(), state.voice.clone());
    let proof = await_applied(&receipt).await;
    assert!(
        proof["fields"]
            .as_array()
            .unwrap()
            .iter()
            .all(|field| field["state"] == "core_readback_verified"),
        "{proof}"
    );
    assert!(proof["cue_readiness"].is_object(), "{proof}");

    // Background cue synthesis completes without blocking the apply transaction.
    let readiness = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let r = state.voice.cue_readiness();
            if r["ready"] == true && r["last_synthesis"]["synthesized"].as_u64().unwrap_or(0) > 0 {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        readiness["ready_phrases"]
            .as_array()
            .unwrap()
            .contains(&json!("请说")),
        "{readiness}"
    );
    let readback_after_synth = read(get(&format!(
        "/api/v1/config/applications/{}/readback",
        receipt["application_id"].as_str().unwrap()
    )))
    .await;
    assert_eq!(readback_after_synth["cue_readiness"]["ready"], true);
    let snap = read(get("/api/v0/mode")).await;
    assert_eq!(snap["cue_readiness"]["ready"], true);

    // Rollback restores baseline cue phrases and re-triggers synthesis.
    let rollback = read(
        post(&format!(
            "/api/v1/config/applications/{}/rollback-preflight",
            receipt["application_id"].as_str().unwrap()
        ))
        .json(&json!({"expected_revision": receipt["config_revision"]})),
    )
    .await;
    let restored = read(commit(&rollback)).await;
    let rollback_proof = await_applied(&restored).await;
    assert!(
        rollback_proof["fields"]
            .as_array()
            .unwrap()
            .iter()
            .all(|field| field["state"] == "core_readback_verified"),
        "{rollback_proof}"
    );
    let after_rollback = state.voice.settings_snapshot();
    assert_eq!(after_rollback.wake_ack.phrases, baseline.wake_ack.phrases);
    assert_eq!(after_rollback.retreat_ack, baseline.retreat_ack);
    assert_eq!(after_rollback.window_close_ack, baseline.window_close_ack);
    assert_eq!(after_rollback.truncation_notice, baseline.truncation_notice);
    assert_eq!(after_rollback.filler.thinking, baseline.filler.thinking);

    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let r = state.voice.cue_readiness();
            if r["ready"] == true
                && r["ready_phrases"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(baseline.retreat_ack))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
