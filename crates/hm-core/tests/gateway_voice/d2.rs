//! D2 uses real loopback ingress, PCM/VAD/ASR and cancellable service waits.
use super::*;

fn origin(lease: &str, input: &str) -> Value {
    json!({"kind":"leased","lease":lease,"origin":{"kind":"text","input_id":input}})
}
async fn wake(gw: &FakeGateway, lease: &str, seq: u64) {
    gw.send_raw_json(
        json!({"type":"wake","device":"dot2-d","ts":hm_core::id::now_ms(),"seq":seq,
        "origin":origin(lease,&hm_core::id::next("wake"))}),
    )
    .await;
}
async fn text(gw: &FakeGateway, lease: &str, seq: u64, text: &str) {
    gw.send_raw_json(
        json!({"type":"utterance","device":"dot2-d","ts":hm_core::id::now_ms(),"seq":seq,
        "source":"text","text":text,"origin":origin(lease,&hm_core::id::next("text"))}),
    )
    .await;
}
async fn closed(state: &AppState) {
    d0_wait_note(state, "session_closed", |_| true).await;
    assert_eq!(
        state.voice.mode_snapshot()["devices"][0]["session"]["state"],
        "idle"
    );
}

#[tokio::test]
async fn wire_lease_revokes_even_a_first_delayed_result_with_new_send_timestamp() {
    let (_base, state, gw) = spawn_app_with_gateway(FakeLlm::new(vec![])).await;
    let old_lease = state.voice.capture_lease("dot2-d");
    let never_delivered_origin = origin(&old_lease, "capture-not-yet-seen");
    wake(&gw, &old_lease, 1).await;
    text(&gw, &old_lease, 2, "现在几点").await;
    let (play, _) = gw.until_play("voice").await;
    text(&gw, &old_lease, 3, "停止播放").await;
    closed(&state).await;
    let new_lease = state.voice.capture_lease("dot2-d");
    assert_ne!(old_lease, new_lease);
    for (seq, kind) in [(4, "wake"), (5, "utterance"), (6, "keyword")] {
        gw.send_raw_json(json!({"type":kind,"device":"dot2-d","ts":hm_core::id::now_ms()+10000,
            "seq":seq,"source":"text","text":"旧问题","keyword":"你好高达","origin":never_delivered_origin})).await;
    }
    gw.send_raw_json(
        json!({"type":"stopped_locally","device":"dot2-d","task_id":play,"reason":"old",
        "played_ms":100,"latency_ms":0,"ts":hm_core::id::now_ms()}),
    )
    .await;
    d1_finish_voice(&gw, &play).await;
    d0_wait_note(&state, "stale_session_event_rejected", |e| {
        e["via"] == "local_stop"
    })
    .await;
    d0_wait_note(&state, "stale_session_event_rejected", |e| {
        e["via"] == "keyword"
    })
    .await;
    assert_eq!(
        state.voice.mode_snapshot()["devices"][0]["session"]["state"],
        "idle"
    );
    assert_eq!(
        state
            .voice
            .recent(100)
            .iter()
            .filter(|e| e["kind"] == "clock_answered_locally")
            .count(),
        1
    );
    // A current capture can explicitly activate again. Ordinary Utterance cannot.
    text(&gw, &new_lease, 7, "现在几点").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        state
            .voice
            .recent(100)
            .iter()
            .filter(|e| e["kind"] == "clock_answered_locally")
            .count(),
        1
    );
    wake(&gw, &new_lease, 8).await;
    text(&gw, &new_lease, 9, "现在几点").await;
    let answer = gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { text: Some(t), .. } | CoreFrame::PlayBegin { text: Some(t), .. }
                    if t.contains('点')
            )
        })
        .await;
    assert!(matches!(
        answer,
        CoreFrame::Play { text: Some(t), .. } | CoreFrame::PlayBegin { text: Some(t), .. }
            if t.contains('点')
    ));
}

#[tokio::test]
async fn zero_credits_voice_playback_pcm_can_exit_without_a_fourth_request() {
    let (base, state, gw) = spawn_mic_app(FakeLlm::new(vec![]), "停止播放").await;
    assert_eq!(
        reqwest::Client::new()
            .put(format!("{base}/api/v0/mode/dot2-d"))
            .json(&json!({"followup_credits": 2}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let lease = state.voice.capture_lease("dot2-d");
    wake(&gw, &lease, 1).await;
    let mut playing = Vec::new();
    for seq in 2..=4 {
        text(&gw, &lease, seq, "现在几点").await;
        let (play, _) = gw.until_play("voice").await;
        playing.push(play);
    }
    let before = state
        .voice
        .recent(100)
        .iter()
        .filter(|e| e["kind"] == "request_started")
        .count();
    assert_eq!(before, 3);
    for play in stops_within(&gw, Duration::from_millis(50)).await {
        gw.confirm_stop(&play).await;
    }
    text(&gw, &lease, 5, "现在几点").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        state
            .voice
            .recent(100)
            .iter()
            .filter(|e| e["kind"] == "request_started")
            .count(),
        3
    );
    assert!(
        stops_within(&gw, Duration::from_millis(50))
            .await
            .is_empty(),
        "denied fourth request must not stop the live third answer"
    );
    assert!(state
        .voice
        .recent(100)
        .iter()
        .any(|e| e["kind"] == "input_admission" && e["reason"] == "followup_credits_exhausted"));
    feed_one_segment(&gw, 10).await;
    closed(&state).await;
    let stops = stops_within(&gw, Duration::from_millis(100)).await;
    assert!(
        stops.contains(playing.last().unwrap()),
        "live answer must be stopped: {stops:?}"
    );
    assert_eq!(
        state
            .voice
            .recent(100)
            .iter()
            .filter(|e| e["kind"] == "request_started")
            .count(),
        3
    );
    assert!(state
        .voice
        .recent(100)
        .iter()
        .any(|e| e["kind"] == "mic_transcript_source"
            && e["ticket"]["spans"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["control_only"] == true)));
}

#[tokio::test]
async fn media_and_voice_pcm_exit_discards_media_and_never_answers_the_exit() {
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
    let (_base, state, gw, _music) = spawn_app_with_gateway_media_asr(
        llm,
        Arc::new(FakeAsr {
            reply: "停止播放".into(),
        }),
    )
    .await;
    let lease = state.voice.capture_lease("dot2-d");
    wake(&gw, &lease, 1).await;
    text(&gw, &lease, 2, "放首歌").await;
    let (media, _) = gw.until_media_play().await;
    let (_, _) = gw.until_any_voice_play().await;
    feed_one_segment(&gw, 10).await;
    closed(&state).await;
    assert!(state.music.sessions_json().get("dot2-d").is_none());
    let stops = stops_within(&gw, Duration::from_millis(100)).await;
    assert!(stops.contains(&media));
    assert_eq!(
        state
            .voice
            .recent(100)
            .iter()
            .filter(|e| e["kind"] == "request_started")
            .count(),
        1
    );
}

#[tokio::test]
async fn blocked_answer_and_local_clock_synthesis_cannot_speak_after_exit() {
    for local_clock in [false, true] {
        let (release, gate) = tokio::sync::watch::channel(false);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let fragment = if local_clock { "点" } else { "旧答案" };
        let tts = Arc::new(GatedTts {
            gates: vec![(fragment, gate)],
            calls: calls.clone(),
        });
        let llm = FakeLlm::new(if local_clock {
            vec![]
        } else {
            vec![Scripted::Text {
                chunks: vec!["旧答案。".repeat(600)],
            }]
        });
        let (_base, state, gw) = spawn_app_with_tts(llm, tts).await;
        let lease = state.voice.capture_lease("dot2-d");
        wake(&gw, &lease, 1).await;
        text(
            &gw,
            &lease,
            2,
            if local_clock {
                "现在几点"
            } else {
                "讲一句"
            },
        )
        .await;
        wait_until_sync("blocked synthesis", || {
            calls.lock().unwrap().iter().any(|t| t.contains(fragment))
        })
        .await;
        text(&gw, &lease, 3, "退下").await;
        closed(&state).await;
        release.send(true).unwrap();
        d0_wait_note(&state, "stale_session_event_rejected", |e| {
            e["via"] == "tts_result"
        })
        .await;
        let plays = tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                if matches!(
                    gw.recv().await,
                    CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                        if content_ref.starts_with("tts:")
                ) {
                    return true;
                }
            }
        })
        .await;
        assert!(plays.is_err(), "old answer reached the wire");
        assert_eq!(
            state.voice.mode_snapshot()["devices"][0]["session"]["state"],
            "idle"
        );
        assert!(
            state.voice.paused_bookmark("dot2-d").is_none(),
            "released old capped TTS must not rebuild a bookmark"
        );
        let current_sid = state.sessions.device_session_id("dot2-d").unwrap();
        assert!(state.sessions.messages(&current_sid).unwrap().is_empty());
    }
}

#[tokio::test]
async fn blocked_resume_and_next_fetch_are_revoked_before_media_registration() {
    for action in ["resume", "next"] {
        let llm = FakeLlm::new(vec![
            Scripted::Calls {
                calls: vec![("music.search".into(), r#"{"query":"*"}"#.into())],
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
        let (_base, state, gw, music) = spawn_app_with_gateway_media(llm).await;
        explicit_request(&state, &gw, 1, "放首歌").await;
        let (media, _) = gw.until_media_play().await;
        state.music.pause_now("dot2-d", "d2").unwrap();
        gw.confirm_stop(&media).await;
        assert!(
            state.music.sessions_json()["dot2-d"]["tracks"]
                .as_u64()
                .unwrap()
                >= 2,
            "next must really have a successor"
        );
        let (release, gate) = tokio::sync::watch::channel(false);
        *music.stream_gate.lock().unwrap() = Some(gate);
        let rt = state.music.clone();
        let pending = tokio::spawn(async move { rt.control("dot2-d", action, None).await });
        wait_until_sync("blocked stream fetch", || music.count("stream:") >= 2).await;
        let lease = state.voice.capture_lease("dot2-d");
        text(&gw, &lease, 2, "退下").await;
        closed(&state).await;
        assert!(state.music.sessions_json().get("dot2-d").is_none());
        release.send(true).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(state.music.sessions_json().get("dot2-d").is_none());
        assert_eq!(music.count("stream:"), 2);
    }
}

#[tokio::test]
async fn actual_stream_answer_with_exit_words_rejects_only_trusted_self_overlap() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["回答包含停止播放，还有暂停。".into()],
    }]);
    let (_base, state, gw) = spawn_mic_app_stream(llm, "未用").await;
    let lease = state.voice.capture_lease("dot2-d");
    wake(&gw, &lease, 1).await;
    text(&gw, &lease, 2, "讲一句").await;
    let (play, answer) = gw.until_any_voice_play().await;
    assert!(answer.contains("停止播放") && answer.contains("暂停"));
    let hit = json!({"kind":"leased","lease":lease,
        "origin":{"kind":"audio","capture_epoch":"mic-a","ticket":"capture-a","input_id":"kw-a","start_sample":100,"end_sample":200},
        "overlap":{"play_id":play,"capture_epoch":"mic-a","ticket":"capture-a","start_sample":150,"end_sample":250}});
    gw.send_raw_json(
        json!({"type":"keyword","device":"dot2-d","ts":hm_core::id::now_ms(),"seq":3,
        "keyword":"停止播放","self_heard":true,"origin":hit}),
    )
    .await;
    d0_wait_note(&state, "keyword_ignored", |e| {
        e["reason"] == "self_heard_retreat_overlap"
    })
    .await;
    assert!(!state
        .voice
        .recent(100)
        .iter()
        .any(|e| e["kind"] == "session_closed"));
    // The same live answer and self_heard hint WITHOUT the overlap proof
    // cannot override a person's exit.
    let mut missing = hit;
    missing.as_object_mut().unwrap().remove("overlap");
    gw.send_raw_json(
        json!({"type":"keyword","device":"dot2-d","ts":hm_core::id::now_ms(),"seq":4,
        "keyword":"停止播放","self_heard":true,"origin":missing}),
    )
    .await;
    closed(&state).await;
    assert!(stops_within(&gw, Duration::from_millis(100))
        .await
        .contains(&play));
}
