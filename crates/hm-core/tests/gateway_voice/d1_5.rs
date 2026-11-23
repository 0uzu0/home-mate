//! D1.5 exercises the real management, synthesis and gateway playback paths.
use super::*;
use hm_core::speech::TtsClient;

async fn app() -> (String, Arc<AppState>, Arc<FakeGateway>, PathBuf) {
    let dir = std::env::temp_dir().join(hm_core::id::next("hm-d1-5"));
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        dir.clone(),
        MockInject::default(),
        Some(Arc::new(FakeLlm::new(vec![]))),
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
    (base, state, gw, dir)
}

async fn put(base: &str, patch: Value) {
    reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&patch)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
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

async fn asset(dir: &std::path::Path, phrase: &str) -> PathBuf {
    let cache = dir.join("assets/wake-acks");
    for _ in 0..100 {
        if let Ok(raw) = std::fs::read_to_string(cache.join("manifest.json")) {
            if let Ok(m) = serde_json::from_str::<Value>(&raw) {
                if let Some(file) = m["files"][phrase].as_str() {
                    let path = cache.join(file);
                    if path.is_file() {
                        let expected = MockTts::default().synthesize(phrase).await.unwrap().wav;
                        assert_eq!(std::fs::read(&path).unwrap(), expected);
                        return path;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no synthesized WAV for {phrase}");
}

async fn ack_play(gw: &FakeGateway, bound_secs: u64) -> (String, String, String) {
    use base64::Engine;
    let (task_id, content_ref, pcm) =
        tokio::time::timeout(Duration::from_secs(bound_secs), async {
            let mut active: Option<(String, String, Vec<u8>)> = None;
            loop {
                match gw.recv().await {
                    CoreFrame::PlayBegin {
                        task_id,
                        content_ref,
                        ..
                    } if content_ref.starts_with("ack:") => {
                        active = Some((task_id, content_ref, Vec::new()));
                    }
                    CoreFrame::PlayChunk {
                        task_id, audio_b64, ..
                    } => {
                        if let Some((ref id, _, ref mut buf)) = active {
                            if id == &task_id {
                                buf.extend_from_slice(
                                    &base64::engine::general_purpose::STANDARD
                                        .decode(audio_b64)
                                        .unwrap(),
                                );
                            }
                        }
                    }
                    CoreFrame::PlayEnd { task_id } => {
                        if let Some((id, cref, buf)) = active.take() {
                            if id == task_id {
                                return (id, cref, buf);
                            }
                        }
                    }
                    CoreFrame::Play {
                        task_id,
                        content_ref,
                        audio_b64: Some(wav),
                        ..
                    } if content_ref.starts_with("ack:") => {
                        let raw = base64::engine::general_purpose::STANDARD
                            .decode(wav)
                            .unwrap();
                        return (task_id, content_ref, raw);
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("ack must reach the gateway");
    (
        task_id,
        content_ref,
        base64::engine::general_purpose::STANDARD.encode(pcm),
    )
}

#[tokio::test]
async fn retreat_and_window_close_play_distinct_phrases_and_wavs() {
    use base64::Engine;
    let (base, state, gw, dir) = app().await;
    put(
        &base,
        json!({"retreat_ack": "遵命", "window_close_ack": "回头见啦",
        "follow_window_tone": true, "follow_window_ms": 5000}),
    )
    .await;
    // Let the existing automatic save synthesis finish; never synthesize manually.
    asset(&dir, "遵命").await;
    state.voice.test_activate_session("dot2-d");
    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 1,
        keyword: "停止播放".into(),
        fed_ms: None,
        self_heard: false,
        rms_dbfs: None,
        lag_ms: None,
        bundle_id: None,
        kws_generation: None,
        kws_tier: None,
        wake_dur_ms: None,
        max_gap_ms: None,
        l2_passed: None,
    })
    .await;
    let (retreat_id, retreat_ref, retreat_wav) = ack_play(&gw, 2).await;
    assert_eq!(retreat_ref, "ack:遵命");
    gw.confirm_play(&retreat_id).await;
    d1_finish_voice(&gw, &retreat_id).await;
    explicit_request(&state, &gw, 2, "现在几点").await;
    let (answer, _) = gw.until_play("voice").await;
    d1_finish_voice(&gw, &answer).await;
    let (close_id, close_ref, close_wav) = ack_play(&gw, 8).await;
    assert_eq!(close_ref, "ack:回头见啦");
    assert_ne!(
        retreat_wav, close_wav,
        "different length phrases produce distinct mock WAVs"
    );
    for (phrase, encoded) in [("遵命", retreat_wav), ("回头见啦", close_wav)] {
        let actual = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let expected_wav = MockTts::default().synthesize(phrase).await.unwrap().wav;
        assert_eq!(actual, expected_wav[44..]);
    }
    gw.confirm_play(&close_id).await;
    d1_finish_voice(&gw, &close_id).await;
    assert_eq!(state.voice.mode_snapshot()["retreat_ack"], "遵命");
}

#[tokio::test]
async fn independent_saves_readback_synthesis_history_restore_and_backup_import() {
    let (base, state, _gw, dir) = app().await;
    assert_eq!(get(&base, "mode").await["window_close_ack"], "拜拜啦");
    put(&base, json!({"retreat_ack": "遵命"})).await;
    let first_retreat = asset(&dir, "遵命").await;
    assert_eq!(get(&base, "mode").await["window_close_ack"], "拜拜啦");
    put(&base, json!({"window_close_ack": " 回头见啦 "})).await;
    let first_close = asset(&dir, "回头见啦").await;
    let mode = get(&base, "mode").await;
    assert_eq!(mode["retreat_ack"], "遵命");
    assert_eq!(mode["window_close_ack"], "回头见啦");
    let backup = get(&base, "config/export").await;
    assert_eq!(backup["mode"]["retreat_ack"], "遵命");
    assert_eq!(backup["mode"]["window_close_ack"], "回头见啦");
    let stored: Value =
        serde_json::from_str(&state.store.config_get("voice_mode").unwrap().unwrap()).unwrap();
    assert_eq!(stored["window_close_ack"], "回头见啦");
    let rows = state.config.history(100).unwrap();
    let row = rows
        .iter()
        .find(|r| r["scope"] == "voice" && r["payload"]["window_close_ack"] == "回头见啦")
        .unwrap();
    let restore_url = format!("{base}/api/v0/config/history/{}/restore", row["id"]);
    put(&base, json!({"retreat_ack": "收到啦"})).await;
    asset(&dir, "收到啦").await;
    assert_eq!(get(&base, "mode").await["window_close_ack"], "回头见啦");
    put(&base, json!({"window_close_ack": "下回聊"})).await;
    asset(&dir, "下回聊").await;
    assert_eq!(get(&base, "mode").await["retreat_ack"], "收到啦");
    std::fs::remove_file(first_close).unwrap();
    std::fs::remove_file(first_retreat).unwrap();
    reqwest::Client::new()
        .post(&restore_url)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let restored_close = asset(&dir, "回头见啦").await;
    let restored_retreat = asset(&dir, "遵命").await;
    assert_eq!(get(&base, "mode").await["retreat_ack"], "遵命");
    assert_eq!(get(&base, "mode").await["window_close_ack"], "回头见啦");
    put(&base, json!({"window_close_ack": "再会啦"})).await;
    asset(&dir, "再会啦").await;
    std::fs::remove_file(restored_close).unwrap();
    std::fs::remove_file(restored_retreat).unwrap();
    reqwest::Client::new()
        .post(format!("{base}/api/v0/config/import"))
        .json(&json!({"format": "home-mate-config", "version": 1, "mode": backup["mode"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    asset(&dir, "回头见啦").await;
    asset(&dir, "遵命").await;
    assert_eq!(get(&base, "mode").await["retreat_ack"], "遵命");
    assert_eq!(get(&base, "mode").await["window_close_ack"], "回头见啦");
}

#[tokio::test]
async fn legacy_backup_fleet_and_history_without_field_use_default() {
    let (base, state, _gw, dir) = app().await;
    let mut old = serde_json::to_value(state.voice.settings_snapshot()).unwrap();
    old.as_object_mut().unwrap().remove("window_close_ack");
    old["retreat_ack"] = json!("遵命");
    for source in [None, Some("old-peer")] {
        put(&base, json!({"window_close_ack": "下回聊"})).await;
        asset(&dir, "下回聊").await;
        let mut req = reqwest::Client::new().post(format!("{base}/api/v0/config/import"));
        if let Some(source) = source {
            req = req.header("X-Home-Mate-Source", source);
        }
        req.json(&json!({"format": "home-mate-config", "version": 1, "mode": old}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let mode = get(&base, "mode").await;
        assert_eq!(mode["window_close_ack"], "拜拜啦", "source={source:?}");
        assert_eq!(mode["retreat_ack"], "遵命");
        asset(&dir, "拜拜啦").await;
    }
    hm_core::config::append_history(&state.store, "test", "voice", "legacy voice", &old).unwrap();
    let row = state
        .config
        .history(100)
        .unwrap()
        .into_iter()
        .find(|r| r["summary"] == "legacy voice")
        .unwrap();
    put(&base, json!({"window_close_ack": "再会啦"})).await;
    asset(&dir, "再会啦").await;
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
    assert_eq!(get(&base, "mode").await["window_close_ack"], "拜拜啦");
}

#[tokio::test]
async fn invalid_phrases_reject_mixed_put_import_fleet_and_restore_without_writes() {
    let (base, state, _gw, _dir) = app().await;
    let before = serde_json::to_value(state.voice.settings_snapshot()).unwrap();
    for field in ["window_close_ack", "retreat_ack"] {
        for bad in ["", "   ", "一二三四五六七八九", "a\"b", "a/b"] {
            let mut snapshot = before.clone();
            snapshot[field] = json!(bad);
            snapshot["follow_window_tone"] = json!(true);
            hm_core::config::append_history(
                &state.store,
                "test",
                "voice",
                "invalid ack",
                &snapshot,
            )
            .unwrap();
            let row = state
                .config
                .history(100)
                .unwrap()
                .into_iter()
                .filter(|r| r["summary"] == "invalid ack")
                .max_by_key(|r| r["id"].as_i64().unwrap())
                .unwrap();
            let history_before = state.config.history(100).unwrap().len();
            let persisted_before = state.store.config_get("voice_mode").unwrap();
            let mut patch = json!({"asr_wake_fallback": false, "follow_window_tone": true,
                "spoken": before["spoken"], "retreat_ack": "收到", "window_close_ack": "再会"});
            patch["spoken"]["max_chars"] = json!(90);
            patch[field] = json!(bad);
            let response = reqwest::Client::new()
                .put(format!("{base}/api/v0/mode/dot2-d"))
                .json(&patch)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 400, "PUT {field}={bad:?}");
            for source in [None, Some("old-peer")] {
                let mut req = reqwest::Client::new().post(format!("{base}/api/v0/config/import"));
                if let Some(source) = source {
                    req = req.header("X-Home-Mate-Source", source);
                }
                let response = req
                    .json(&json!({"mode": snapshot, "devices": {
                    "dot2-d": {"name": "must not write", "room": "must not write"}}}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 400, "import {source:?} {field}={bad:?}");
            }
            let response = reqwest::Client::new()
                .post(format!(
                    "{base}/api/v0/config/history/{}/restore",
                    row["id"]
                ))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 400, "restore {field}={bad:?}");
            assert_eq!(
                serde_json::to_value(state.voice.settings_snapshot()).unwrap(),
                before
            );
            assert_eq!(
                state.store.config_get("voice_mode").unwrap(),
                persisted_before
            );
            assert_eq!(state.store.config_get("device_meta").unwrap(), None);
            assert_eq!(state.config.history(100).unwrap().len(), history_before);
        }
    }
    // Both Unicode length boundaries are accepted, independently.
    put(
        &base,
        json!({"window_close_ack": "好", "retreat_ack": "一二三四五六七八"}),
    )
    .await;
    assert_eq!(get(&base, "mode").await["window_close_ack"], "好");
    put(
        &base,
        json!({"window_close_ack": "一二三四五六七八", "retreat_ack": "好"}),
    )
    .await;
    assert_eq!(
        get(&base, "mode").await["window_close_ack"],
        "一二三四五六七八"
    );
}
