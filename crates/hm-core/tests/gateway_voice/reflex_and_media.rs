use super::*;

/// Switch off: the reflex still reports, the pipeline records and ignores.
#[tokio::test]
async fn keyword_ignored_when_interrupt_disabled() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    let _ = state.voice.set_mode(
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
        Some(false),
        None,
        None,
        None,
        None,
        None,
        "test",
    );

    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 2,
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

    let stopped = stops_within(&gw, Duration::from_millis(500)).await;
    assert!(
        stopped.is_empty(),
        "disabled keyword must not stop anything, saw {stopped:?}"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "keyword_ignored" && e["reason"] == "disabled"),
        "must log the ignore reason: {log:?}"
    );
    let _ = play_id;
}

/// P1 (R1g): the keyword must work during MEDIA playback. It used to answer
/// `media_playback_out_of_scope`, which is how 《简单爱》 played 75.8 s through
/// three interruption attempts. On a half-duplex link the phrase stops every
/// channel and opens the follow window.
#[tokio::test]
async fn keyword_stops_media_playback_and_opens_the_window() {
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
    let (base, state, gw, _music) = spawn_app_with_gateway_media(llm).await;

    explicit_request(&state, &gw, 1, "放第一名的歌").await;
    let (media_id, _) = gw.until_media_play().await;

    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 2,
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

    let stopped = stops_within(&gw, Duration::from_millis(500)).await;
    assert_eq!(
        stopped,
        vec![media_id.clone()],
        "the keyword must stop the media play: {stopped:?}"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "keyword_stop" && e["content"]["media"] == true),
        "must record the stop with the content it stopped: {log:?}"
    );

    let snap: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dev = snap["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == "dot2-d")
        .unwrap();
    assert_eq!(dev["session"]["state"], "listening");
    assert_eq!(dev["kws"]["stops"], 1);
}

/// M3-1: a wake in the middle of a song PAUSES it. The queue and the position
/// survive, so the window the wake opened can say 继续 or 下一首 — the person
/// took the conversation over, they did not throw the song away.
#[tokio::test]
async fn a_wake_in_media_pauses_the_session_and_keeps_it() {
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
    let (base, state, gw, _music) = spawn_app_with_gateway_media(llm).await;

    explicit_request(&state, &gw, 1, "放第一名的歌").await;
    let (media_id, _) = gw.until_media_play().await;
    // Let the pump push a little audio so the freeze has a position to keep.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        state.music.sessions_json()["dot2-d"]["state"],
        "playing",
        "the session must exist while the song plays: {}",
        state.music.sessions_json()
    );

    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 2,
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

    let stopped = stops_within(&gw, Duration::from_millis(400)).await;
    assert!(
        stopped.contains(&media_id),
        "the song must stop: {stopped:?}"
    );

    let snap: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let media = &snap["media"]["dot2-d"];
    assert_eq!(
        media["state"], "paused",
        "a wake pauses media, it does not discard it: {snap}"
    );
    assert_eq!(media["title"], "晚风信笺");
    assert!(
        media["position_ms"].as_u64().unwrap_or(0) > 0,
        "the position is kept: {media}"
    );
    assert_eq!(media["tracks"], 1, "the queue is kept: {media}");
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "media_paused" && e["why"] == "wake_takeover"),
        "the pause must be recorded with its reason: {log:?}"
    );
}

/// M3-3b: the KWS hit and its ASR transcript are the SAME speech. The spotter
/// runs ~1 s earlier; when the transcript arrives it must not barge-stop what
/// the spotter just did (device 2026-11-13: 继续播放 resumed the song, then the
/// wake path stopped it 0.8 s later). The transcript travels the REAL path:
/// uplink audio → VAD segment → ASR.
#[tokio::test]
async fn a_reflex_word_is_not_undone_by_its_own_transcript() {
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
    let asr = Arc::new(FakeAsr {
        reply: "你好，高达，继续播放。".into(),
    });
    let (_base, state, gw, _music) = spawn_app_with_gateway_media_asr(llm, asr).await;
    explicit_request(&state, &gw, 1, "放第一名的歌").await;
    let (media_id, _) = gw.until_media_play().await;
    let answer = gw
        .until(|f| matches!(f, CoreFrame::PlayBegin { channel: Some(c), .. } if c == "voice"))
        .await;
    let CoreFrame::PlayBegin {
        task_id: answer_id, ..
    } = answer
    else {
        unreachable!()
    };
    gw.confirm_play(&answer_id).await;
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: answer_id,
        played_ms: 800,
        ts: hm_core::id::now_ms(),

        consumed_ms: None,
        progress_basis: None,
    })
    .await;

    // Pause by the direct word.
    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 2,
        keyword: "暂停播放".into(),
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
    let f = gw
        .until(|f| matches!(f, CoreFrame::Stop { task_id, .. } if task_id == &media_id))
        .await;
    let CoreFrame::Stop { task_id, .. } = f else {
        unreachable!()
    };
    gw.send(&GatewayFrame::StopReceipt {
        task_id,
        stage: "device_confirmed".into(),
        played_ms: Some(500),

        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The spotter resumes it.
    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 3,
        keyword: "继续播放".into(),
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
    let (resumed_id, _) = gw.until_media_play().await;
    gw.confirm_play(&resumed_id).await;

    // The same speech reaches the core as audio → ASR, inside the dedupe
    // window. 800 ms of speech, then the quiet that closes the segment.
    // The blocks go in AFTER our own answer's 400 ms tail-blind window: on
    // the device they arrive paced, so the blind eats only the first blocks.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut seq = 1;
    for _ in 0..10 {
        gw.send(&audio_chunk(seq, &loud_block())).await;
        seq += 1;
    }
    for _ in 0..12 {
        gw.send(&audio_chunk(seq, &quiet_block())).await;
        seq += 1;
    }

    let mut stops = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
    loop {
        match tokio::time::timeout_at(deadline, gw.recv()).await {
            Ok(CoreFrame::Stop { task_id, .. }) => stops.push(task_id),
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(
        !stops.contains(&resumed_id),
        "the transcript of an already-executed word must not stop it: {stops:?}"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "intent_deduped" && e["via"] == "wake_rest"),
        "the dedupe must be recorded: {log:?}"
    );
    assert_eq!(
        state.music.sessions_json()["dot2-d"]["state"],
        "playing",
        "the song keeps playing: {}",
        state.music.sessions_json()
    );
}

/// M3-3c: 「你好高达，暂停播放」 said in one breath while a song plays. The
/// wake must NOT barge-stop (that would discard the session and its place);
/// the pause intent owns the audio and keeps a resume point.
#[tokio::test]
async fn a_wake_with_pause_keeps_the_media_session() {
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
    let asr = Arc::new(FakeAsr {
        reply: "你好，高达，暂停播放。".into(),
    });
    let (_base, state, gw, _music) = spawn_app_with_gateway_media_asr(llm, asr).await;
    explicit_request(&state, &gw, 1, "放第一名的歌").await;
    let (media_id, _) = gw.until_media_play().await;
    let answer = gw
        .until(|f| matches!(f, CoreFrame::PlayBegin { channel: Some(c), .. } if c == "voice"))
        .await;
    let CoreFrame::PlayBegin {
        task_id: answer_id, ..
    } = answer
    else {
        unreachable!()
    };
    gw.confirm_play(&answer_id).await;
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: answer_id,
        played_ms: 800,
        ts: hm_core::id::now_ms(),

        consumed_ms: None,
        progress_basis: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The person says the wake and the pause word in one breath.
    let mut seq = 1;
    for _ in 0..10 {
        gw.send(&audio_chunk(seq, &loud_block())).await;
        seq += 1;
    }
    for _ in 0..12 {
        gw.send(&audio_chunk(seq, &quiet_block())).await;
        seq += 1;
    }
    // The pause sends the Stop; answer it like a device.
    let f = gw
        .until(|f| matches!(f, CoreFrame::Stop { task_id, .. } if task_id == &media_id))
        .await;
    let CoreFrame::Stop { task_id, .. } = f else {
        unreachable!()
    };
    gw.send(&GatewayFrame::StopReceipt {
        task_id,
        stage: "device_confirmed".into(),
        played_ms: Some(300),

        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let media = state.music.sessions_json();
    assert_eq!(
        media["dot2-d"]["state"], "paused",
        "the pause keeps a resume point: {media}"
    );
    assert_eq!(media["dot2-d"]["title"], "晚风信笺");
    let log = state.voice.recent(200);
    assert!(
        !log.iter().any(|e| e["kind"] == "wake_word_stop"),
        "a wake carrying a control word must not barge-stop: {log:?}"
    );
}

/// M3-2: pause → resume plays the SAME track from the SAME place, and the
/// core speaks its own sentence (`《…》已暂停。`), never the model's.
#[tokio::test]
async fn music_control_pauses_and_resumes_the_same_track() {
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
    let (media_id, _) = gw.until_media_play().await;
    // The answer that follows the tool call is a play too: confirm it, so its
    // program retires (otherwise it is still "live" and a control word has
    // two candidates — which is a real rule, not a test artifact).
    let answer = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout_at(deadline, gw.recv()).await {
                Ok(CoreFrame::PlayChunk { .. }) => continue,
                Ok(f) => {
                    eprintln!("DBG nonchunk {:.320}", format!("{f:?}"));
                    if matches!(&f, CoreFrame::PlayBegin { channel: Some(c), .. } if c == "voice") {
                        break f;
                    }
                }
                Err(_) => panic!("no voice play"),
            }
        }
    };
    let answer_id = match answer {
        CoreFrame::PlayBegin { task_id, .. } | CoreFrame::Play { task_id, .. } => task_id,
        other => panic!("not a play: {other:?}"),
    };
    gw.confirm_play(&answer_id).await;
    // The answer drained: its program retires, so only the SONG is left to
    // control. (A live program + a paused song is the ambiguity case, not
    // this one.)
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: answer_id.clone(),
        played_ms: 800,
        ts: hm_core::id::now_ms(),

        consumed_ms: None,
        progress_basis: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let ctx = || ToolCtx::new("T-m3", "C-m3", Arc::new(AtomicBool::new(false)));
    // Pause concurrently: the tool waits for the device's flush receipt, so
    // the test answers the Stop like a real device does.
    let paused = {
        let state = state.clone();
        let handle = tokio::spawn(async move {
            state
                .registry
                .call(
                    "music.control",
                    serde_json::json!({ "action": "pause" }),
                    &ctx(),
                )
                .await
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut kinds = Vec::new();
        let stop = loop {
            match tokio::time::timeout_at(deadline, gw.recv()).await {
                Ok(CoreFrame::Stop { task_id, .. }) if task_id == media_id => break task_id,
                Ok(f) => kinds.push(format!("{:?}", std::mem::discriminant(&f))),
                Err(_) => panic!("no stop; saw {} frames", kinds.len()),
            }
        };
        gw.send(&GatewayFrame::StopReceipt {
            task_id: stop,
            stage: "device_confirmed".into(),
            played_ms: Some(400),

            consumed_ms: None,
            progress_basis: None,
            detail: None,
        })
        .await;
        handle.await.unwrap()
    };
    assert_eq!(paused.status, ToolStatus::Completed, "{paused:?}");
    let fact = paused.fact.clone().unwrap();
    assert!(fact.confirmed);
    assert_eq!(
        hm_core::agent::facts::render_fact(&paused, &fact),
        "《晚风信笺》已暂停。"
    );
    let media = &state.music.sessions_json()["dot2-d"];
    assert_eq!(media["state"], "paused");
    let position = media["position_ms"].as_u64().unwrap_or(0);
    assert!(position > 0);

    // Resume: same track, and the position carries over (the source is
    // re-read from its start and the skipped part is discarded).
    let resumed = {
        let state = state.clone();
        let handle = tokio::spawn(async move {
            state
                .registry
                .call(
                    "music.control",
                    serde_json::json!({ "action": "resume" }),
                    &ctx(),
                )
                .await
        });
        let (next_id, text) = gw.until_media_play().await;
        assert!(
            text.contains("晚风信笺"),
            "resume must be the same song: {text}"
        );
        gw.confirm_play(&next_id).await;
        handle.await.unwrap()
    };
    assert_eq!(resumed.status, ToolStatus::Completed, "{resumed:?}");
    let media = &state.music.sessions_json()["dot2-d"];
    assert_eq!(media["state"], "playing");
    assert_eq!(media["title"], "晚风信笺");
    assert!(
        media["position_ms"].as_u64().unwrap_or(0) >= position,
        "the resumed position continues from the pause point: {media}"
    );
}

/// M3-3: 下一首 walks the queue the search returned. It never searches again
/// (one search, four songs) and it does not open a conversation.
#[tokio::test]
async fn next_advances_the_queue_without_a_new_search() {
    let llm = FakeLlm::new(vec![
        // An online search answers any query with the whole (4-song)
        // catalogue, so the turn's queue has something to advance into.
        Scripted::Calls {
            calls: vec![(
                "music.search".into(),
                r#"{"query":"歌","source":"online"}"#.into(),
            )],
        },
        Scripted::Calls {
            calls: vec![(
                "music.play".into(),
                r#"{"source":"online","id":"1001"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["放上了。".into()],
        },
    ]);
    let (_base, state, gw, music) = spawn_app_with_gateway_media(llm).await;
    // This test needs the ONLINE source as well (its search returns the whole
    // catalogue, so the session has a queue to walk).
    common::configure_music(&state.store, &music, true, true, false, true);
    explicit_request(&state, &gw, 1, "随便放首歌").await;
    let (first_id, first_text) = gw.until_media_play().await;
    assert!(first_text.contains("晚风信笺"), "{first_text}");
    let searches_before = music.count("online_search:");

    let next = {
        let state = state.clone();
        let handle = tokio::spawn(async move {
            state
                .registry
                .call(
                    "music.control",
                    serde_json::json!({ "action": "next" }),
                    &ToolCtx::new("T-m3", "C-m3", Arc::new(AtomicBool::new(false))),
                )
                .await
        });
        let (next_id, text) = gw.until_media_play().await;
        assert_ne!(next_id, first_id);
        assert!(
            text.contains("光年之外的光"),
            "the queue's next track: {text}"
        );
        gw.confirm_play(&next_id).await;
        handle.await.unwrap()
    };
    assert_eq!(next.status, ToolStatus::Completed, "{next:?}");
    assert_eq!(
        music.count("online_search:"),
        searches_before,
        "下一首 must not run another search"
    );
    let media = &state.music.sessions_json()["dot2-d"];
    assert_eq!(media["index"], 1);
    assert_eq!(media["tracks"], 4);
}

/// M3-4: stop DISCARDs (pause is the word that keeps a rescue point). After
/// it there is nothing to resume — not the song, not the story.
#[tokio::test]
async fn stop_discards_the_media_session() {
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
    let _ = gw.until_media_play().await;
    // Retire the answer's program too: after stop there must be NOTHING left
    // to resume, not a live answer the test never finished.
    let answer = gw
        .until(|f| matches!(f, CoreFrame::PlayBegin { channel: Some(c), .. } if c == "voice"))
        .await;
    let CoreFrame::PlayBegin {
        task_id: answer_id, ..
    } = answer
    else {
        unreachable!()
    };
    gw.confirm_play(&answer_id).await;
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: answer_id,
        played_ms: 800,
        ts: hm_core::id::now_ms(),

        consumed_ms: None,
        progress_basis: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let ctx = || ToolCtx::new("T-m3", "C-m3", Arc::new(AtomicBool::new(false)));
    let stopped = {
        let state = state.clone();
        let handle = tokio::spawn(async move {
            state
                .registry
                .call(
                    "music.control",
                    serde_json::json!({ "action": "stop" }),
                    &ctx(),
                )
                .await
        });
        let f = gw.until(|f| matches!(f, CoreFrame::Stop { .. })).await;
        let CoreFrame::Stop { task_id, .. } = f else {
            unreachable!()
        };
        gw.send(&GatewayFrame::StopReceipt {
            task_id,
            stage: "device_confirmed".into(),
            played_ms: Some(600),

            consumed_ms: None,
            progress_basis: None,
            detail: None,
        })
        .await;
        handle.await.unwrap()
    };
    assert_eq!(stopped.status, ToolStatus::Completed, "{stopped:?}");
    assert!(stopped.fact.clone().unwrap().confirmed);
    assert!(
        state.music.sessions_json().get("dot2-d").is_none(),
        "stop removes the session: {}",
        state.music.sessions_json()
    );

    let resume = state
        .registry
        .call(
            "music.control",
            serde_json::json!({ "action": "resume" }),
            &ctx(),
        )
        .await;
    assert_eq!(resume.status, ToolStatus::Failed);
    assert_eq!(resume.error.unwrap().code, "MUSIC_NOTHING_TO_CONTROL");
}

/// M3-5: two paused things and no explicit target → ASK. Guessing would stop
/// the wrong one, and the person has no way to tell which was chosen.
#[tokio::test]
async fn two_paused_targets_are_asked_about_not_guessed() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec![long_answer],
        },
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
    let (base, state, gw, _music) = spawn_app_with_gateway_media(llm).await;

    // A story, paused at a sentence boundary (the console's pause). The
    // answer is a STREAM here (the device advertises play_stream), so it
    // arrives as a PlayBegin, not a whole Play.
    explicit_request(&state, &gw, 1, "讲个长故事").await;
    let (story_play, _) = gw.until_any_voice_play().await;
    gw.confirm_play(&story_play).await;
    let paused: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/devices/dot2-d/program/pause"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(paused["paused"], true, "{paused}");

    // A song, paused by the tool.
    explicit_request(&state, &gw, 2, "放第一名的歌").await;
    let (media_id, _) = gw.until_media_play().await;
    let paused_media = {
        let state = state.clone();
        let handle = tokio::spawn(async move {
            state
                .registry
                .call(
                    "music.control",
                    serde_json::json!({ "action": "pause" }),
                    &ToolCtx::new("T-m3", "C-m3", Arc::new(AtomicBool::new(false))),
                )
                .await
        });
        let f = gw
            .until(|f| matches!(f, CoreFrame::Stop { task_id, .. } if task_id == &media_id))
            .await;
        let CoreFrame::Stop { task_id, .. } = f else {
            unreachable!()
        };
        gw.send(&GatewayFrame::StopReceipt {
            task_id,
            stage: "device_confirmed".into(),
            played_ms: Some(0),
            consumed_ms: None,
            progress_basis: None,
            detail: None,
        })
        .await;
        handle.await.unwrap()
    };
    assert_eq!(paused_media.status, ToolStatus::Completed);

    let ambiguous = state
        .registry
        .call(
            "music.control",
            serde_json::json!({ "action": "resume" }),
            &ToolCtx::new("T-m3", "C-m3", Arc::new(AtomicBool::new(false))),
        )
        .await;
    assert_eq!(ambiguous.status, ToolStatus::Failed);
    assert_eq!(ambiguous.error.unwrap().code, "MUSIC_TARGET_AMBIGUOUS");

    // Naming the target resolves it.
    let explicit = {
        let state = state.clone();
        let handle = tokio::spawn(async move {
            state
                .registry
                .call(
                    "music.control",
                    serde_json::json!({ "action": "resume", "target": "media" }),
                    &ToolCtx::new("T-m3", "C-m3", Arc::new(AtomicBool::new(false))),
                )
                .await
        });
        let (next_id, text) = gw.until_media_play().await;
        assert!(text.contains("晚风信笺"), "{text}");
        gw.confirm_play(&next_id).await;
        handle.await.unwrap()
    };
    assert_eq!(explicit.status, ToolStatus::Completed, "{explicit:?}");
}

/// P1 (R1g): with NOTHING playing the phrase is a WAKE, not a dropped fact.
/// It used to answer `no_voice_playback`; the half-duplex link's only command
/// entry must work in the idle state too (window + local ack).
#[tokio::test]
async fn keyword_with_no_playback_wakes_and_opens_the_window() {
    let llm = FakeLlm::new(vec![]);
    let (base, state, gw) = spawn_app_with_gateway(llm).await;
    // The ack is a cached WAV; synthesize it first (same setup as the ASR
    // bare-wake test) so the wake path has something to play.
    let _ = state.voice.set_mode(
        "dot2-d",
        None,
        None,
        None,
        None,
        None,
        Some(hm_core::voice::WakeAckSettings {
            enabled: true,
            phrases: vec!["在呢".into()],
            resynth: None,
        }),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(no_filler()),
        None,
        None,
        "test",
    );
    state.voice.synthesize_wake_acks(false).await;

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

    // NO ack on the keyword path (R1g): an ack is a voice play, and a voice
    // play closes the mic gate — it would swallow a command said in the same
    // breath. A bare wake's "我在" comes from the ASR confirmation instead.
    let unexpected = tokio::time::timeout(
        Duration::from_millis(400),
        gw.until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("ack:")
            )
        }),
    )
    .await;
    assert!(
        unexpected.is_err(),
        "the keyword wake must not play an ack: {unexpected:?}"
    );

    let snap: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dev = snap["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == "dot2-d")
        .unwrap();
    assert_eq!(
        dev["session"]["state"], "listening",
        "the wake opens the follow window"
    );
    assert_eq!(dev["kws"]["wakes"], 1);
    assert_eq!(dev["kws"]["stops"], 0);
    let log = state.voice.recent(100);
    assert!(
        log.iter().any(|e| e["kind"] == "keyword_wake"),
        "must record the wake, not an ignore: {log:?}"
    );
}

/// The console must be able to state whether the reflex exists: spotter
/// lifecycle lands in the mode snapshot, and the toggle round-trips.
#[tokio::test]
async fn kws_report_and_toggle_visible_in_mode_snapshot() {
    let llm = FakeLlm::new(vec![]);
    let (base, state, gw) = spawn_app_with_gateway(llm).await;

    gw.send(&GatewayFrame::KwsReport {
        device: "dot2-d".into(),
        state: "running".into(),
        detail: "ready in 6797ms".into(),
        restarts: 1,
        detections: 3,
        guarded: 2,
        epoch: 1_700_000_000_000,
        loaded_hash: String::new(),
        loaded_ms: 0,
        local_exit: 0,
        threshold: 0.25,
        score: 3.0,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let snap: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snap["keyword_interrupt"], true);
    assert_eq!(snap["keyword_phrase"], hm_core::voice::KWS_PHRASE);
    let dev = snap["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == "dot2-d")
        .unwrap();
    assert_eq!(dev["kws"]["state"], "running");
    assert_eq!(dev["kws"]["restarts"], 1);
    assert_eq!(dev["kws"]["detections"], 3);

    // Toggle through the console API and confirm the applied state.
    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({ "keyword_interrupt": false }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let snap: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snap["keyword_interrupt"], false);
    assert_eq!(state.voice.mode_snapshot()["keyword_interrupt"], false);
}

// --- R1g: half duplex means the wake word is the only command entry ----------

/// MX-2 (story playing): a direct command heard by the mic is never transcribed
/// — the half-duplex gate holds the stream closed while our own voice plays.
#[tokio::test]
async fn a_command_without_the_wake_word_does_not_act_while_a_story_plays() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_mic_app(llm.clone(), "停。").await;

    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    feed_one_segment(&gw, 2).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let stopped = stops_within(&gw, Duration::from_millis(300)).await;
    assert!(
        !stopped.contains(&play_id),
        "the story must not stop for an unwoken command: {stopped:?}"
    );
    assert_eq!(llm.seen_requests().len(), 1, "no follow-up turn ran");
    let log = state.voice.recent(200);
    assert!(
        !log.iter().any(|e| e["kind"] == "mic_utterance"),
        "the gated stream must not reach ASR as an utterance: {log:?}"
    );
}

/// MX-2 (music playing): the mic gate is OPEN during media (only our voice
/// closes it), so ASR does hear the room — and the matrix drops it.
#[tokio::test]
async fn a_command_without_the_wake_word_does_not_act_while_music_plays() {
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
        Scripted::Text {
            chunks: vec!["不应该到达".into()],
        },
    ]);
    let (_base, state, gw, _music) = spawn_media_mic_app(llm.clone(), "停。").await;

    explicit_request(&state, &gw, 1, "放第一名的歌").await;
    let (media_id, _) = gw.until_media_play().await;
    // Let the answer's own voice finish so only media is live.
    let (answer_id, _) = gw.until_any_voice_play().await;
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: answer_id,
        played_ms: 500,
        ts: hm_core::id::now_ms(),

        consumed_ms: None,
        progress_basis: None,
    })
    .await;
    // Past the post-play tail-blind window: the mic gate re-opens there, and
    // the audio this test feeds must be judged by the MATRIX, not dropped as
    // our own answer's reverb.
    tokio::time::sleep(Duration::from_millis(600)).await;
    // The follow window is still OPEN here (the music command opened it) —
    // the stronger claim: while media plays, even an in-window transcript is
    // not a command unless it contains the wake word. Otherwise the music's
    // own leaked audio would run turns (device log 2026-11-12: a diagnostic
    // asset was transcribed verbatim and answered).
    assert!(state.voice.mode_snapshot()["devices"][0]["session"]["state"] == "listening");

    feed_one_segment(&gw, 2).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    let stopped = stops_within(&gw, Duration::from_millis(300)).await;
    assert!(
        !stopped.contains(&media_id),
        "music must keep playing: {stopped:?}"
    );
    assert_eq!(
        llm.seen_requests().len(),
        3,
        "the music turn's own calls (search, play, answer) and nothing else"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "mic_followup_gated_by_media"),
        "the media gate must be recorded: {log:?}"
    );
    assert!(
        log.iter().any(|e| e["kind"] == "mic_ignored_unwoken"),
        "the transcript is counted as unwoken speech: {log:?}"
    );
}

/// The other half of the same row: during media a transcript that DOES say
/// the wake word wakes and stops the music (the ASR fallback for the phrase).
#[tokio::test]
async fn the_wake_word_still_interrupts_music_through_asr() {
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
    let (_base, state, gw, _music) = spawn_media_mic_app(llm.clone(), "你好，高达").await;

    explicit_request(&state, &gw, 1, "放第一名的歌").await;
    let (media_id, _) = gw.until_media_play().await;
    let (answer_id, _) = gw.until_any_voice_play().await;
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: answer_id,
        played_ms: 500,
        ts: hm_core::id::now_ms(),

        consumed_ms: None,
        progress_basis: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    feed_one_segment(&gw, 2).await;
    let stopped = stops_within(&gw, Duration::from_millis(800)).await;
    assert!(
        stopped.contains(&media_id),
        "the wake word must stop the music: {stopped:?}"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "wake_word_stop" && e["content"]["media"] == true),
        "the stop must be recorded with its content: {log:?}"
    );
    let snap = state.voice.mode_snapshot();
    assert_eq!(snap["devices"][0]["session"]["state"], "listening");
}

/// MX-2 (idle): same rule with nothing playing.
#[tokio::test]
async fn a_command_without_the_wake_word_does_not_act_when_idle() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["不应该到达".into()],
    }]);
    let (_base, state, gw) = spawn_mic_app(llm.clone(), "停。").await;
    feed_one_segment(&gw, 1).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert_eq!(llm.seen_requests().len(), 0, "no turn ran");
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "mic_ignored_unwoken" && e["text"] == "停。"),
        "the transcript is counted as unwoken speech: {log:?}"
    );
}

/// MX-7 (P7): the gateway's lifetime counters and this core's counters belong
/// to different processes. The snapshot must carry BOTH epochs and a
/// since-epoch delta, so "detections=14 vs stops=0" can be read honestly.
#[tokio::test]
async fn kws_counters_carry_both_process_epochs() {
    let (_base, state, gw) = spawn_app_with_gateway(FakeLlm::new(vec![])).await;
    let send = |detections: u64, epoch: u64| {
        let gw = gw.clone();
        async move {
            gw.send(&GatewayFrame::KwsReport {
                device: "dot2-d".into(),
                state: "running".into(),
                detail: "ready".into(),
                restarts: 0,
                detections,
                guarded: 1,
                epoch,
                loaded_hash: String::new(),
                loaded_ms: 0,
                local_exit: 0,
                threshold: 0.25,
                score: 3.0,
            })
            .await;
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
    };

    // First sight of epoch A: the gateway's count becomes this core's baseline.
    send(14, 1_700_000_000_000).await;
    let snap = state.voice.mode_snapshot();
    let dev = &snap["devices"][0];
    assert_eq!(dev["kws"]["epoch"], 1_700_000_000_000u64);
    assert_eq!(dev["kws"]["detections"], 14);
    assert_eq!(dev["kws"]["detections_since_epoch"], 0);
    assert!(dev["kws"]["counted_since_ms"].as_u64().unwrap() > 0);
    assert!(dev["kws"]["core_epoch"].as_u64().unwrap() > 0);
    assert_eq!(
        dev["kws"]["guarded"], 1,
        "the guard count reaches the core now"
    );

    // More hits in the SAME gateway epoch: the delta grows.
    send(16, 1_700_000_000_000).await;
    let snap = state.voice.mode_snapshot();
    assert_eq!(snap["devices"][0]["kws"]["detections_since_epoch"], 2);

    // A gateway RESTART (new epoch): the lifetime count is no longer ours to
    // compare, so the delta restarts at zero and the epoch says why.
    send(17, 1_700_000_500_000).await;
    let snap = state.voice.mode_snapshot();
    assert_eq!(snap["devices"][0]["kws"]["epoch"], 1_700_000_500_000u64);
    assert_eq!(snap["devices"][0]["kws"]["detections"], 17);
    assert_eq!(snap["devices"][0]["kws"]["detections_since_epoch"], 0);
}

/// R1g: the device's echo guard is only a HINT. When OUR OWN live answer text
/// contains the phrase, a "self-heard" hit is refused (that is the real
/// self-trigger). Anything else is accepted — otherwise the wake word cannot
/// interrupt our own playback, which is the only interruption path a
/// half-duplex link has.
#[tokio::test]
async fn a_self_heard_hit_inside_our_own_answer_text_is_refused() {
    let answer = "你可以对我说你好高达，我就会回答你。".to_string() + &"再讲一点。".repeat(200);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![answer],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;

    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 2,
        keyword: "你好高达".into(),
        fed_ms: None,
        self_heard: true,
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

    let stopped = stops_within(&gw, Duration::from_millis(400)).await;
    assert!(
        !stopped.contains(&play_id),
        "our own words must not interrupt us: {stopped:?}"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter().any(|e| e["kind"] == "keyword_ignored"
            && e["reason"] == "self_heard_phrase_in_our_own_answer"),
        "the refusal must say why: {log:?}"
    );
}

/// The other half: the guard suspects our speakers, but the live answer does
/// not contain the phrase — a person said it over the playback, so it counts.
#[tokio::test]
async fn a_suspected_hit_outside_our_own_text_still_interrupts() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;

    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 2,
        keyword: "你好高达".into(),
        fed_ms: None,
        self_heard: true,
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

    let stopped = stops_within(&gw, Duration::from_millis(600)).await;
    assert_eq!(
        stopped,
        vec![play_id.clone()],
        "a person over the playback must win: {stopped:?}"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "keyword_self_heard_accepted"),
        "the accepted hint must be recorded: {log:?}"
    );
}
