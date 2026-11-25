use super::*;

#[tokio::test]
async fn d0_delayed_kws_keeps_old_capture_despite_new_timestamp_seq_and_connection() {
    let (_base, state, gw) = spawn_mic_app_stream_asr(
        FakeLlm::new(vec![]),
        Arc::new(FakeAsr {
            reply: String::new(),
        }),
    )
    .await;
    let input = gw.capture_input();
    let captured = input.keyword(1280, 2560);
    let old_generation = gw.generation.load(std::sync::atomic::Ordering::SeqCst);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], port).into());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let new_gw = connect_gateway_audio_in_caps(port, vec!["input_origin_v1"]).await;
    let new_generation = new_gw.generation.load(std::sync::atomic::Ordering::SeqCst);
    assert!(new_generation > old_generation);
    new_gw.send_captured(captured.clone(), 9999999, 900).await;
    let note = d0_wait_note(&state, "keyword_source", |v| {
        v["provenance"]["origin"] == captured["origin"]
    })
    .await;
    assert_eq!(
        note["provenance"]["transport"]["generation"],
        new_generation
    );
    assert_eq!(note["ts"], 9999999);
    assert_eq!(note["provenance"]["origin"]["capture_epoch"], input.epoch);
    // D0 deliberately does not assert rejection or zero reopening.
}

#[tokio::test]
async fn d0_kws_and_delayed_asr_share_source_but_same_text_other_input_stays_distinct() {
    let (tx, mut started) = mpsc::unbounded_channel();
    let (_base, state, gw) =
        spawn_mic_app_stream_asr(FakeLlm::new(vec![]), Arc::new(D0GatedAsr { started: tx })).await;
    let input = gw.capture_input();
    let kws = input.keyword(0, 1280);
    gw.send_captured(kws.clone(), hm_core::id::now_ms(), 1)
        .await;
    d0_send_segment(&gw, &input, 2).await;
    let release = tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .unwrap()
        .unwrap();
    let closed = d0_wait_note(&state, "mic_segment_closed", |v| {
        v["ticket"]["spans"][0]["provenance"]["origin"]["input_id"] == input.input_id
    })
    .await;
    let mut other = gw.capture_input();
    other.epoch = input.epoch.clone(); // Distinct input in the same capture stream, not a restart.
    gw.send_captured(other.keyword(0, 1280), hm_core::id::now_ms(), 100)
        .await;
    // Same text, adjacent arrival, distinct source input; no text/time association.
    let other_kws = d0_wait_note(&state, "keyword_source", |v| {
        v["provenance"]["origin"]["input_id"] == other.input_id
    })
    .await;
    release.send(Ok("普通环境句子".into())).unwrap();
    let returned = d0_wait_note(&state, "mic_transcript_source", |v| {
        v["ticket"] == closed["ticket"]
    })
    .await;
    assert_eq!(returned["outcome"], "ok");
    let spans = returned["ticket"]["spans"].as_array().unwrap();
    assert_eq!(spans[0]["provenance"]["origin"], kws["origin"]);
    assert_ne!(
        spans[0]["provenance"]["origin"]["input_id"],
        other_kws["provenance"]["origin"]["input_id"]
    );
    assert_eq!(spans.len(), 14); // original VAD half-hangover trim
    assert_eq!(
        spans
            .iter()
            .map(|s| s["samples"].as_u64().unwrap())
            .sum::<u64>(),
        14 * 1280
    );
    d0_send_segment(&gw, &other, 101).await;
    let other_release = tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .unwrap()
        .unwrap();
    other_release.send(Ok("普通环境句子".into())).unwrap();
    let other_return = d0_wait_note(&state, "mic_transcript_source", |v| {
        v["ticket"]["spans"][0]["provenance"]["origin"]["input_id"] == other.input_id
    })
    .await;
    assert_ne!(other_return["ticket"], returned["ticket"]);
    assert!(other_return["ticket"]["spans"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["provenance"]["origin"]["input_id"] == other.input_id));
}

#[tokio::test]
async fn d0_asr_failure_empty_and_reconnect_preserve_original_work_tickets() {
    let (tx, mut started) = mpsc::unbounded_channel();
    let (_base, state, gw) =
        spawn_mic_app_stream_asr(FakeLlm::new(vec![]), Arc::new(D0GatedAsr { started: tx })).await;
    let input = gw.capture_input();
    d0_send_segment(&gw, &input, 1).await;
    let old_release = tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .unwrap()
        .unwrap();
    let closed = d0_wait_note(&state, "mic_segment_closed", |v| {
        v["ticket"]["spans"][0]["provenance"]["origin"]["input_id"] == input.input_id
    })
    .await;
    old_release
        .send(Err(hm_core::speech::SpeechError::Protocol(
            "injected failure".into(),
        )))
        .unwrap();
    let failed = d0_wait_note(&state, "mic_transcript_source", |v| {
        v["outcome"] == "failed"
    })
    .await;
    let mut pending = gw.capture_input();
    pending.epoch = input.epoch.clone();
    d0_send_segment(&gw, &pending, 30).await;
    let cancelled = tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .unwrap()
        .unwrap();
    // DeviceStatus takes the real offline/reset path and aborts pending ASR.
    gw.send(&GatewayFrame::DeviceStatus {
        northbound: None,
        device: "dot2-d".into(),
        online: false,
        reason: "test-reset".into(),
        descriptor: None,
    })
    .await;
    d0_wait_note(&state, "device_status", |v| v["online"] == false).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], port).into());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let next_gw = connect_gateway_audio_in(port).await;
    let next_input = next_gw.capture_input();
    d0_send_segment(&next_gw, &next_input, 1).await;
    let next_release = tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .unwrap()
        .unwrap();
    let next_closed = d0_wait_note(&state, "mic_segment_closed", |v| {
        v["ticket"]["spans"][0]["provenance"]["origin"]["input_id"] == next_input.input_id
    })
    .await;
    next_release.send(Ok(String::new())).unwrap();
    assert!(
        cancelled.is_closed(),
        "reconnect must cancel rather than retain the old recognition worker"
    );
    assert!(cancelled
        .send(Err(hm_core::speech::SpeechError::Protocol(
            "late cancelled failure".into()
        )))
        .is_err());
    let empty = d0_wait_note(&state, "mic_transcript_source", |v| v["outcome"] == "empty").await;
    assert_eq!(failed["ticket"], closed["ticket"]);
    assert_eq!(empty["ticket"], next_closed["ticket"]);
    assert_ne!(
        failed["ticket"]["spans"][0]["provenance"]["transport"]["generation"],
        empty["ticket"]["spans"][0]["provenance"]["transport"]["generation"]
    );
    assert!(empty["ticket"]["spans"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["provenance"]["origin"]["input_id"] == next_input.input_id));
}

#[tokio::test]
async fn d0_legacy_audio_is_unproven_through_asr_and_text_has_no_audio_range() {
    let (_base, state, gw) = spawn_mic_app_stream_asr(
        FakeLlm::new(vec![]),
        Arc::new(FakeAsr {
            reply: String::new(),
        }),
    )
    .await;
    for seq in 1..=18 {
        gw.send(&audio_chunk(
            seq,
            &if seq <= 10 {
                loud_block()
            } else {
                quiet_block()
            },
        ))
        .await;
    }
    let returned = d0_wait_note(&state, "mic_transcript_source", |v| v["outcome"] == "empty").await;
    assert!(returned["ticket"]["spans"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["provenance"]["origin"]["kind"] == "legacy"));
    gw.send_raw_json(
        json!({"type":"utterance","device":"dot2-d","ts":123,"seq":19,"source":"text","text":"退下",
        "origin":{"kind":"text","input_id":"console-original-1"}}),
    )
    .await;
    let text = d0_wait_note(&state, "utterance", |v| {
        v["provenance"]["origin"]["input_id"] == "console-original-1"
    })
    .await;
    assert_eq!(
        text["provenance"]["origin"],
        json!({"kind":"text","input_id":"console-original-1"})
    );
}

/// This test uses only pre-D0 public types and raw wire JSON, so the exact
/// same assertion can run against the entry snapshot before implementation.
#[tokio::test]
async fn d0_raw_wire_origin_reaches_voice_without_being_discarded() {
    let (_base, state, gw) = spawn_mic_app_stream_asr(
        FakeLlm::new(vec![]),
        Arc::new(FakeAsr {
            reply: String::new(),
        }),
    )
    .await;
    let origin = json!({"kind":"text","input_id":"immutable-wire-input-1"});
    gw.send_raw_json(
        json!({"type":"utterance","device":"dot2-d","ts":123,"seq":1,
        "source":"text","text":"退下","origin":origin}),
    )
    .await;
    let note = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(note) = state
                .voice
                .recent(100)
                .into_iter()
                .find(|v| v["kind"] == "utterance")
            {
                break note;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        note["provenance"]["origin"], origin,
        "the raw source identity must survive hub -> voice"
    );
}

#[tokio::test]
async fn d0_open_segment_keeps_preroll_source_after_capture_context_switch_and_asr_delay() {
    let (tx, mut started) = mpsc::unbounded_channel();
    let (_base, state, gw) =
        spawn_mic_app_stream_asr(FakeLlm::new(vec![]), Arc::new(D0GatedAsr { started: tx })).await;
    let old = gw.capture_input();
    // Two quiet blocks, then three loud: the source-bearing onset includes
    // real preroll (the existing ring keeps the three most recent blocks).
    for block in 0..5 {
        let pcm = if block < 2 {
            quiet_block()
        } else {
            loud_block()
        };
        gw.send_raw_json(old.pcm(block + 1, block * 1280, &pcm))
            .await;
    }
    let opened = d0_wait_note(&state, "mic_speech_started", |_| true).await;
    let mut next = gw.capture_input();
    next.epoch = old.epoch.clone(); // Input metadata may change without restarting capture.
    for block in 0..8 {
        gw.send_raw_json(next.pcm(block + 6, block * 1280, &quiet_block()))
            .await;
    }
    let release = tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .unwrap()
        .unwrap();
    let closed = d0_wait_note(&state, "mic_segment_closed", |_| true).await;
    let mut third = gw.capture_input();
    third.epoch = old.epoch.clone();
    gw.send_captured(third.keyword(0, 1280), hm_core::id::now_ms(), 100)
        .await;
    d0_wait_note(&state, "keyword_source", |v| {
        v["provenance"]["origin"]["input_id"] == third.input_id
    })
    .await;
    release.send(Ok("环境语音".into())).unwrap();
    let returned = d0_wait_note(&state, "mic_transcript_source", |v| {
        v["ticket"] == closed["ticket"]
    })
    .await;
    let spans = returned["ticket"]["spans"].as_array().unwrap();
    assert_eq!(
        &spans[..3],
        opened["provenance"]["spans"].as_array().unwrap()
    );
    assert!(spans[..3]
        .iter()
        .all(|s| s["provenance"]["origin"]["input_id"] == old.input_id));
    assert!(spans[3..]
        .iter()
        .all(|s| s["provenance"]["origin"]["input_id"] == next.input_id));
    assert_eq!(spans.len(), 7);
}

#[tokio::test]
async fn d1_first_and_followup_asr_requests_keep_one_generation_and_unique_ids() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["第一段。".into()],
        },
        Scripted::Text {
            chunks: vec!["第二段。".into()],
        },
    ]);
    let asr = CountingAsr::new("讲个故事");
    let (base, state, gw) = spawn_mic_app_asr(llm, asr.clone()).await;
    reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({"tail_blind_ms": 0, "asr_wake_fallback": false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 1,
        keyword: "你好高达".into(),
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
    tokio::time::sleep(Duration::from_millis(60)).await;
    let mut seq = 1;
    for _ in 0..2 {
        for _ in 0..10 {
            gw.send(&audio_chunk(seq, &loud_block())).await;
            seq += 1;
        }
        for _ in 0..14 {
            gw.send(&audio_chunk(seq, &quiet_block())).await;
            seq += 1;
        }
        let (play, _) = gw.until_play("voice").await;
        d1_finish_voice(&gw, &play).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    let mut requests: Vec<Value> = state
        .voice
        .recent(200)
        .into_iter()
        .filter(|e| e["kind"] == "request_started")
        .map(|e| e["request"].clone())
        .collect();
    requests.sort_by_key(|r| r["request_id"].as_u64().unwrap());
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(requests[0]["generation"], requests[1]["generation"]);
    assert_ne!(requests[0]["request_id"], requests[1]["request_id"]);
    assert_eq!(requests[0]["kind"], "First");
    assert_eq!(requests[1]["kind"], "Followup");
    assert_eq!(asr.calls(), 2, "B still charges both ASR requests");
}

#[tokio::test]
async fn d1_immediate_clock_answer_terminals_close_their_own_request() {
    let (base, state, gw) = spawn_app_with_gateway(FakeLlm::new(vec![])).await;
    reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({"follow_window_enabled": false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    for seq in 1..=10 {
        explicit_request(&state, &gw, seq, "现在几点").await;
        let (play, _) = gw.until_play("voice").await;
        // No artificial pause or prior accepted/device_confirmed receipt:
        // the terminal may race the core's synchronous output registration.
        d1_finish_voice(&gw, &play).await;
        d1_wait_idle(&state).await;
    }
    let log = state.voice.recent(200);
    assert_eq!(
        log.iter()
            .filter(|e| e["kind"] == "clock_answered_locally")
            .count(),
        10
    );
}

#[tokio::test]
async fn d1_timeout_keeps_paused_media_and_explicit_exit_discards_it() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("music.search".into(), r#"{"query":"晚风"}"#.into())],
        },
        Scripted::Calls {
            calls: vec![(
                "music.play".into(),
                r#"{"source":"library","id":"t01"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["放上了。".into()],
        },
    ]);
    let (_base, state, gw, _music) = spawn_app_with_gateway_media(llm).await;
    explicit_request(&state, &gw, 1, "放第一名的歌").await;
    let (media, _) = gw.until_media_play().await;
    state.music.pause_now("dot2-d", "d1_test").unwrap();
    gw.confirm_stop(&media).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Stop outstanding speech without discarding the paused media session.
    state.voice.stop_device("dot2-d");
    state.voice.session_sweep_at(hm_core::id::now_ms() + 60_000);
    assert_eq!(state.music.sessions_json()["dot2-d"]["state"], "paused");
    explicit_request(&state, &gw, 2, "退下").await;
    for _ in 0..100 {
        if state.music.sessions_json().get("dot2-d").is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "explicit exit left media behind: {}",
        state.music.sessions_json()
    );
}

#[tokio::test]
async fn d1_resumed_whole_answer_missing_terminal_finishes_with_unknown_delivery() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["山谷里有一只小猫。".into()],
    }]);
    let (base, state, gw) = spawn_mic_app(llm, "你好高达").await;
    explicit_request(&state, &gw, 1, "讲个故事").await;
    let (first, _) = gw.until_play("voice").await;
    gw.confirm_play(&first).await;
    assert!(state.voice.pause_program("dot2-d", "d1_test"));
    for play in stops_within(&gw, Duration::from_millis(100)).await {
        gw.confirm_stop(&play).await;
    }
    reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({"follow_window_enabled": false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    state.voice.session_sweep_at(hm_core::id::now_ms() + 60_000);
    assert!(state.voice.resume_program("dot2-d"));
    let (_resumed, _) = gw.until_play("voice").await;
    // A missing terminal is bounded, not a claim that the device played it.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.voice.mode_snapshot()["devices"][0]["participation"],
        "idle"
    );
    assert!(state
        .voice
        .recent(200)
        .iter()
        .any(|e| e["kind"] == "spoken_terminal" && e["stage"] == "timeout" && e["heard"] == false));
}

#[tokio::test]
async fn d1_partial_tts_failure_with_no_stop_receipt_does_not_pin_request() {
    let release_failure = Arc::new(tokio::sync::Notify::new());
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(FakeLlm::new(vec![Scripted::Text {
            chunks: vec!["只有一个开头。".into()],
        }]))),
        Some(Arc::new(D1PartialFailTts {
            release_failure: release_failure.clone(),
        })),
        None,
        None,
    );
    let mut settings = state.voice.settings_snapshot();
    settings.follow_window_enabled = false;
    settings.filler = no_filler();
    state.voice.restore_settings(settings, "test").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], port).into());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let gw = connect_gateway_audio_in_caps(port, vec!["play_stream"]).await;
    explicit_request(&state, &gw, 1, "讲个故事").await;
    let (play, _) = gw.until_any_voice_play().await;
    gw.until(|f| matches!(f, CoreFrame::PlayChunk { task_id, .. } if task_id == &play))
        .await;
    release_failure.notify_one();
    gw.until(|f| matches!(f, CoreFrame::Stop { task_id, .. } if task_id == &play))
        .await;
    // Deliberately send no StopReceipt: the failed delivery itself must settle.
    d1_wait_idle(&state).await;
    assert!(state
        .voice
        .recent(200)
        .iter()
        .any(|e| e["kind"] == "tts_failed" && e["streaming"] == true));
}

/// 2026-11-15 (user): 「回复已经关闭…我还要说一句退下吧，才能防止他听到我说别的话」.
///
/// The product default is now "an answer ENDS the conversation": the session
/// closes by itself when the answer's audio finishes, and room speech
/// afterwards is not answered. This is the behaviour 小爱同学 has and what
/// rui-local settled on after cancelling wake-word-free continuous mode.
#[tokio::test]
async fn an_answer_ends_the_conversation_by_default() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["现在三点二十。".into()],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    // spawn_app_with_gateway enables the window for the mechanism suites;
    // this case is about the shipped default, so put it back.
    let _ = state.voice.set_mode_with_follow_window(
        "dot2-d",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        hm_core::voice::FollowWindowPatch {
            follow_window_enabled: Some(false),
            ..Default::default()
        },
        "test",
    );

    // A wake opens the window for one question (that part is unchanged).
    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 1,
        keyword: "你好高达".into(),
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
    tokio::time::sleep(Duration::from_millis(120)).await;
    explicit_request(&state, &gw, 2, "现在几点").await;
    let (play, _) = gw.until_play("voice").await;
    gw.confirm_play(&play).await;
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: play,
        played_ms: 1200,
        consumed_ms: None,
        progress_basis: None,
        ts: hm_core::id::now_ms(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let snap = state.voice.mode_snapshot();
    let session = &snap["devices"][0]["session"];
    assert_eq!(session["state"], "idle", "{session:?}");
    assert_eq!(
        session["close_reason"], "single_request",
        "the answer must close the window by itself: {session:?}"
    );
    assert_eq!(session["credits"], 0);
}

/// Contract v2 #4 (2026-11-16): the spotter's sensitivity is the household's
/// to tune. It rides the same validated management path as every other voice
/// setting (history/restore/import), and the DEVICE gets it on the next
/// deploy — one operation from the console's point of view.
#[tokio::test]
async fn kws_sensitivity_is_validated_saved_and_deployed() {
    let (base, state, gw) = spawn_app_with_gateway(FakeLlm::new(vec![])).await;
    let client = reqwest::Client::new();
    let mode_url = format!("{base}/api/v0/mode/dot2-d");
    let before = state.voice.settings_snapshot();
    assert!(
        (before.kws_threshold - 0.25).abs() < 1e-6,
        "sherpa's standard point"
    );
    assert!((before.kws_score - 3.0).abs() < 1e-6);
    let history_before = state.config.history(100).unwrap().len();
    for invalid in [
        json!({"kws_threshold": 0.01}),
        json!({"kws_threshold": 0.95}),
        json!({"kws_score": 0.2}),
        json!({"kws_score": 7.0}),
    ] {
        let mut mixed = invalid.clone();
        mixed["follow_window_ms"] = json!(12_000);
        let result = client.put(&mode_url).json(&mixed).send().await.unwrap();
        assert_eq!(result.status(), 400, "{invalid} must be refused");
        let now = state.voice.settings_snapshot();
        assert_eq!(now.follow_window_ms, 9_000, "no partial write: {invalid}");
        assert!((now.kws_threshold - 0.25).abs() < 1e-6);
    }
    assert_eq!(state.config.history(100).unwrap().len(), history_before);

    let patch = json!({"kws_threshold": 0.18, "kws_score": 3.5});
    assert_eq!(
        client
            .put(&mode_url)
            .json(&patch)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let readback: Value = client
        .get(format!("{base}/api/v0/mode"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(readback["kws_threshold"], json!(0.18));
    assert_eq!(readback["kws_score"], json!(3.5));
    let rows = state.config.history(100).unwrap();
    let row = rows
        .iter()
        .find(|r| {
            r["scope"] == "voice"
                && r["payload"]["device_overrides"]["dot2-d"]["kws_threshold"] == 0.18
        })
        .expect("the change must be in the history");
    assert_eq!(
        row["payload"]["device_overrides"]["dot2-d"]["kws_score"],
        3.5
    );
    let backup: Value = client
        .get(format!("{base}/api/v0/config/export"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        backup["mode"]["device_overrides"]["dot2-d"]["kws_threshold"],
        0.18
    );

    // The deploy is what puts it on the device.
    assert_eq!(
        client
            .post(format!("{base}/api/v0/reflex/deploy"))
            .json(&json!({"device": "dot2-d"}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let deployed = gw.until(|f| matches!(f, CoreFrame::KwsDeploy { .. })).await;
    let CoreFrame::KwsDeploy {
        threshold, score, ..
    } = deployed
    else {
        unreachable!()
    };
    assert_eq!(threshold, Some(0.18));
    assert_eq!(score, Some(3.5));

    // A restored/imported snapshot carries the same numbers.
    let id = row["id"].as_i64().unwrap();
    assert_eq!(
        client
            .put(&mode_url)
            .json(&json!({"kws_threshold": 0.4}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .post(format!("{base}/api/v0/config/history/{id}/restore"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        (state
            .voice
            .settings_snapshot()
            .effective_for("dot2-d")
            .kws_threshold
            - 0.18)
            .abs()
            < 1e-6
    );
}

/// Contract v2 #4 (2026-11-16), end to end with the window EXPLICITLY ON
/// (the shipped default closes after each answer): an ANSWER's end starts the
/// tail; the close announcement that follows its expiry is an `ack`, and
/// finishing it moves nothing — not the window it just announced (release
/// 33 said 「拜拜啦」every 21 s forever because the cue's own tail re-opened
/// it), not a new one. The gate is the output's KIND, not a play-id
/// exception.
#[tokio::test]
async fn only_an_answer_end_moves_the_follow_window_end_to_end() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["现在三点二十。".into()],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    // Window ON, short tail, boundary cue audible — a household asking for
    // this behaviour through the same management operation the console uses.
    let _ = state.voice.set_mode_with_follow_window(
        "dot2-d",
        None,
        None,
        None,
        None,
        Some(true),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        hm_core::voice::FollowWindowPatch {
            follow_window_enabled: Some(true),
            follow_window_ms: Some(5_000),
            followup_credits: Some(2),
            ..Default::default()
        },
        "test",
    );
    state.voice.synthesize_wake_acks(false).await;

    // 1) A real answer, played and finished: THAT starts the tail.
    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 1,
        keyword: "你好高达".into(),
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
    tokio::time::sleep(Duration::from_millis(120)).await;
    explicit_request(&state, &gw, 2, "现在几点").await;
    let (play, _) = gw.until_play("voice").await;
    gw.confirm_play(&play).await;
    let answer_end = hm_core::id::now_ms();
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: play,
        played_ms: 1200,
        consumed_ms: None,
        progress_basis: None,
        ts: answer_end,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let snap = state.voice.mode_snapshot();
    let session = &snap["devices"][0]["session"];
    assert_eq!(
        session["phase"], "tail",
        "the answer's end is the window: {session:?}"
    );
    let deadline = session["tail_deadline_ms"].as_u64().expect("tail deadline");
    assert!(
        (deadline as i64 - answer_end as i64 - 5_000).abs() <= 1_000,
        "the tail runs the configured 5 s from the answer's end: deadline={deadline} end={answer_end}"
    );

    // 2) The tail expires; the sweep announces the boundary with a real cue.
    tokio::time::sleep(Duration::from_millis(5_500)).await;
    let cue = gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("ack:")
            )
        })
        .await;
    let (cue_id, cue_ref) = match cue {
        CoreFrame::Play {
            task_id,
            content_ref,
            ..
        }
        | CoreFrame::PlayBegin {
            task_id,
            content_ref,
            ..
        } => (task_id, content_ref),
        _ => unreachable!(),
    };
    assert!(
        cue_ref.contains("拜拜啦"),
        "the boundary cue is the configured window_close_ack: {cue_ref}"
    );
    gw.confirm_play(&cue_id).await;
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: cue_id,
        played_ms: 900,
        consumed_ms: None,
        progress_basis: None,
        ts: hm_core::id::now_ms(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;

    let snap = state.voice.mode_snapshot();
    let session = &snap["devices"][0]["session"];
    assert_eq!(session["state"], "idle", "{session:?}");
    assert_eq!(session["close_reason"], "timeout", "{session:?}");
    assert!(
        session["tail_deadline_ms"].is_null(),
        "the cue's end must not restart the tail it announced: {session:?}"
    );
    // Release 33's signature: the cue re-opened the window and the next
    // sweep announced it again, every ~21 s. One delivered cue, ever (the
    // pair "cue started" / "cue settled" is one announcement).
    let delivered: Vec<serde_json::Value> = state
        .voice
        .recent(400)
        .into_iter()
        .filter(|e| e["kind"] == "follow_window_close" && e["stage"].is_string())
        .collect();
    assert_eq!(
        delivered.len(),
        1,
        "the cue must be announced once: {delivered:?}"
    );
}
