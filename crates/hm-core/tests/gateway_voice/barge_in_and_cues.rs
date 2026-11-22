use super::*;

/// The acceptance behaviour (D/T5): a near-end speech fact during voice
/// playback stops it, the follow window opens, and the interrupting speech
/// — the audio AFTER the stop — forms the segment.
///
/// The onset pre-roll is NOT replayed (2026-11-10 user round): it is
/// recorded while our own answer plays, so feeding it handed the ASR the
/// device's own voice (transcribed back as "我在。"). A fact with no live
/// speech behind it must therefore produce no utterance at all.
#[tokio::test]
async fn barge_in_stops_playback_and_live_speech_forms_the_segment() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec![long_answer],
        },
        Scripted::Text {
            chunks: vec!["好的，讲个短的。".into()],
        },
    ]);
    let (_base, state, gw) = spawn_mic_app(llm, "别讲了，换个话题").await;
    // R1g: the near-end local stop and direct barge-in are FULL-duplex
    // abilities; this test exercises that row of the matrix.
    report_duplex(&gw, "full").await;

    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;

    // D/T5 R1: a fresh answer owns an answer-protection period (default
    // 800 ms) — a fact inside it is the answer's own TTS onset transient.
    // Wait it out, then interrupt like a person would.
    tokio::time::sleep(Duration::from_millis(900)).await;
    gw.send(&barge_in_frame(2, 5)).await;

    let stopped = stops_within(&gw, Duration::from_millis(600)).await;
    assert_eq!(
        stopped,
        vec![play_id.clone()],
        "barge-in must stop the live voice play exactly once, saw {stopped:?}"
    );
    gw.send(&GatewayFrame::StopReceipt {
        task_id: play_id,
        stage: "device_confirmed".into(),
        played_ms: Some(0),
        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;

    // The interrupted speech continues on the live stream: loud (the user
    // is still talking) then quiet (hangover closes the segment).
    let loud = loud_block();
    let quiet = quiet_block();
    for seq in 3..=5 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 6..=13 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }

    // Follow window is open: the barge-in speech is a follow-up turn —
    // the second scripted answer plays without a new wake word.
    let (play2, _) = gw.until_play("voice").await;
    gw.confirm_play(&play2).await;

    let log = state.voice.recent(300);
    assert!(
        log.iter().any(|e| e["kind"] == "barge_in_stop"),
        "barge_in_stop must be logged: {log:?}"
    );
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_in_preroll_held" && e["samples"] == 6400),
        "the pre-roll must be held, not fed to the VAD: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "barge_in_preroll_fed"),
        "no pre-roll may reach the VAD: {log:?}"
    );
    // R1e: the guarded head (preroll ++ the chunks the guard refused) is
    // RELEASED into the VAD once the guard is over, so the syllable it had to
    // refuse still reaches ASR as part of the user's own segment.
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_head_released" && e["samples"].as_u64().unwrap_or(0) > 0),
        "the guarded head must reach the VAD: {log:?}"
    );
    assert!(
        log.iter().any(|e| e["kind"] == "mic_segment_closed"
            && e["barge_in"] == true
            && e["duration_ms"].as_u64().unwrap_or(0) >= 800),
        "the segment must include the head (0.4 s preroll + guarded audio): {log:?}"
    );
    assert!(
        log.iter().any(|e| {
            e["kind"] == "mic_segment_closed"
                && e["barge_in"] == true
                && e["blocks"].as_u64().unwrap_or(0) >= 8
        }),
        "the segment must be the live speech after the stop (3 loud + hangover): {log:?}"
    );
    assert!(
        log.iter()
            .any(|e| e["kind"] == "mic_utterance" && e["source"] == "asr"),
        "the interrupted words must run as a follow-up turn: {log:?}"
    );
}

/// R1 (2026-11-12): a fact whose device evidence is not certified is
/// refused. The device must prove the reference was fully locked and that
/// the residual is within its 15 dB near-end margin; old firmware (fields
/// default false/0) is refused too. No stop, no utterance.
#[tokio::test]
async fn an_uncertified_barge_in_fact_is_refused() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    // R1g: the near-end local stop and direct barge-in are FULL-duplex
    // abilities; this test exercises that row of the matrix.
    report_duplex(&gw, "full").await;
    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    tokio::time::sleep(Duration::from_millis(900)).await;

    // 1) The reference lost its lock inside the detection block.
    gw.send(&barge_in_frame_evidence(2, 5, -19.0, -0.5, false))
        .await;
    let stopped = stops_within(&gw, Duration::from_millis(500)).await;
    assert!(
        stopped.is_empty(),
        "unlocked reference must not stop the answer: {stopped:?}"
    );
    assert!(!stopped.contains(&play_id));

    // 2) Residual far below the reference: this is echo, not a person.
    gw.send(&barge_in_frame_evidence(3, 5, -19.0, -20.0, true))
        .await;
    let stopped = stops_within(&gw, Duration::from_millis(500)).await;
    assert!(
        stopped.is_empty(),
        "echo residual must not stop the answer: {stopped:?}"
    );

    let log = state.voice.recent(300);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_in_ignored" && e["reason"] == "reference_not_locked"),
        "must name the reference refusal: {log:?}"
    );
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_in_ignored" && e["reason"] == "residual_too_low"),
        "must name the residual refusal: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "barge_in_stop"),
        "no accepted barge-in may be recorded: {log:?}"
    );
}

/// Combined overlap: playback ends (entering the 400ms post-play tail blind
/// window) while dealias reference was unlocked (`ref_locked = false`). Neither
/// the unlocked barge-in nor a trailing echo segment inside `tail_blind_ms`
/// may self-wake or trigger an ASR turn.
#[tokio::test]
async fn tail_blind_window_and_unlocked_ref_both_suppress_self_wake() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    report_duplex(&gw, "full").await;
    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    tokio::time::sleep(Duration::from_millis(900)).await;

    // 1) Unlocked reference during playback must not barge in.
    gw.send(&barge_in_frame_evidence(2, 5, -18.0, -0.5, false))
        .await;
    let stopped = stops_within(&gw, Duration::from_millis(250)).await;
    assert!(stopped.is_empty(), "unlocked ref must not stop playback");

    // 2) Playback finishes -> post-play 400ms tail blind window opens.
    gw.send(&GatewayFrame::PlayReceipt {
        task_id: play_id,
        stage: "finished".into(),
        detail: None,
        ts: hm_core::id::now_ms(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(40)).await;

    // 3) Trailing echo arrives inside the 400ms post-play tail blind window.
    gw.send(&audio_chunk(3, &loud_block())).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let log = state.voice.recent(300);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_in_ignored" && e["reason"] == "reference_not_locked"),
        "expected unlocked-ref refusal in log: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "mic_utterance"),
        "echo inside tail_blind_ms after unlocked ref must not spawn a turn: {log:?}"
    );
}

/// R1f: the core must not second-guess the device's acoustic certification
/// with a level floor. The user's real interruptions measured energy
/// -47.97 / -52.22 dBFS (the device had already stopped locally at 32/35 ms);
/// the -45 dBFS floor tried on 2026-11-12 dropped them, so the user got
/// silence and no follow-up turn. Those levels overlap the measured false
/// stops (-49.4/-56.5), so only the device's 15 dB residual contract is
/// enforced here; a false stop's follow-up "啊。" is dropped downstream by
/// `is_filler_utterance`.
#[tokio::test]
async fn a_device_certified_soft_person_is_accepted() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    // R1g: the near-end local stop and direct barge-in are FULL-duplex
    // abilities; this test exercises that row of the matrix.
    report_duplex(&gw, "full").await;
    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    tokio::time::sleep(Duration::from_millis(900)).await;

    // 1) Outside the device's 15 dB margin: the device would not send this.
    gw.send(&barge_in_frame_energy(2, 5, -30.0, -25.0, -20.0, true))
        .await;
    let stopped = stops_within(&gw, Duration::from_millis(500)).await;
    assert!(
        stopped.is_empty(),
        "a fact outside the device margin must not stop: {stopped:?}"
    );

    // 2) The measured real soft interruption: -50 dBFS, residual -4. The
    //    device certified it (locked reference, inside its margin); the
    //    core must accept it even though the absolute level is low.
    gw.send(&barge_in_frame_energy(3, 5, -50.0, -46.0, -4.0, true))
        .await;
    let stopped = stops_within(&gw, Duration::from_millis(800)).await;
    assert!(
        stopped.contains(&play_id),
        "a device-certified soft person must stop the answer: {stopped:?}"
    );
    let log = state.voice.recent(300);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_in_ignored" && e["reason"] == "residual_too_low"),
        "the out-of-margin fact must name its refusal: {log:?}"
    );
    assert!(
        log.iter().any(|e| e["kind"] == "barge_in_stop"),
        "the accepted soft person must be recorded: {log:?}"
    );
}

/// R1 structural split (Step 4): accepting a barge-in stops playback, but
/// the audio captured before the stopped answer's tail has cleared is NOT
/// the user's words. The 250 ms guard discards it (and resets the VAD), so
/// the follow-up turn starts from what the person said after the stop.
#[tokio::test]
async fn the_stopped_answer_tail_never_becomes_the_utterance() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec![long_answer],
        },
        Scripted::Text {
            chunks: vec!["好的，讲个短的。".into()],
        },
    ]);
    let (_base, state, gw) = spawn_mic_app(llm, "别讲了，换个话题").await;
    // R1g: the near-end local stop and direct barge-in are FULL-duplex
    // abilities; this test exercises that row of the matrix.
    report_duplex(&gw, "full").await;
    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    gw.send(&barge_in_frame(2, 5)).await;
    // The stopped answer's tail arrives immediately (inside 250 ms): three
    // loud blocks that must be discarded, not transcribed. Sent BEFORE the
    // stop-confirmation wait so they really land in the guard window.
    let loud = loud_block();
    for seq in 3..=5 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    let stopped = stops_within(&gw, Duration::from_millis(600)).await;
    assert_eq!(stopped, vec![play_id.clone()], "one stop: {stopped:?}");
    gw.send(&GatewayFrame::StopReceipt {
        task_id: play_id,
        stage: "device_confirmed".into(),
        played_ms: Some(0),

        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The person speaks after the tail guard: this is the follow-up turn.
    for seq in 6..=10 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    let quiet = quiet_block();
    for seq in 11..=20 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }
    let (play2, _) = gw.until_play("voice").await;
    gw.confirm_play(&play2).await;

    let log = state.voice.recent(300);
    assert!(
        log.iter().any(|e| e["kind"] == "barge_in_tail_dropped"),
        "the 250 ms tail guard must record its drop: {log:?}"
    );
    assert!(
        log.iter()
            .any(|e| e["kind"] == "mic_segment_closed" && e["barge_in"] == true),
        "the post-guard speech must form the interrupting segment: {log:?}"
    );
    assert!(
        log.iter()
            .any(|e| e["kind"] == "mic_utterance" && e["source"] == "asr"),
        "the follow-up turn must use the post-guard audio: {log:?}"
    );
}

/// R1 acceptance (2026-11-09 night self-trigger): a near-end fact arriving
/// right after a fresh answer starts is the answer's OWN TTS onset
/// transient (AEC still converging → low correlation). Low correlation is
/// necessary, never sufficient: the fact is recorded and ignored, nothing
/// stops. This is the exact loop that made the first T5 acceptance fail.
/// M6b (S3): the device's energy detector starts at `f32::MIN` and an older
/// gateway sends that sentinel verbatim. `-3.4e38` in a diagnostic feed reads
/// like a measurement ("极大数字"), so the core must record it as MISSING —
/// and an unmeasurable fact must not become a stop.
#[tokio::test]
async fn an_invalid_energy_sentinel_is_recorded_as_missing() {
    let (base, state, gw) = spawn_app_with_gateway(FakeLlm::new(vec![])).await;
    gw.send(&barge_in_frame_energy(9, 5, f32::MIN, -30.0, 12.0, true))
        .await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let devices: Value = reqwest::get(format!("{base}/api/v0/devices"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rec = devices["recent"]
        .as_array()
        .and_then(|a| a.iter().find(|r| r["kind"] == "barge_in"))
        .unwrap_or_else(|| panic!("the fact is recorded: {devices}"));
    assert!(
        rec["energy_db"].is_null(),
        "the sentinel must be null, not a number: {rec}"
    );
    // The other measurements of the same fact are still real numbers.
    assert_eq!(rec["ref_db"], json!(-30.0));
    assert_eq!(rec["residual_db"], json!(12.0));
    // Nothing acted on it (half duplex routes interruption through the wake
    // word; the unmeasurable bit is irrelevant, but a STOP would be a bug).
    let log = state.voice.recent(100);
    assert!(
        !log.iter().any(|e| e["kind"] == "barge_in_stop"),
        "an invalid-energy fact must not stop playback: {log:?}"
    );
}

#[tokio::test]
async fn barge_in_fact_during_answer_protect_is_ignored() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    // R1g: the near-end local stop and direct barge-in are FULL-duplex
    // abilities; this test exercises that row of the matrix.
    report_duplex(&gw, "full").await;

    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    // Immediately: inside the 800 ms answer-protection period.
    gw.send(&barge_in_frame(2, 5)).await;

    let stopped = stops_within(&gw, Duration::from_millis(500)).await;
    assert!(
        stopped.is_empty(),
        "answer-protected fact must not stop the answer, saw {stopped:?}"
    );
    assert!(!stopped.contains(&play_id));
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_in_ignored" && e["reason"] == "answer_protect"),
        "must name the protection reason: {log:?}"
    );
    // The pre-roll must NOT have been replayed into the VAD (R3).
    assert!(
        !log.iter().any(|e| e["kind"] == "barge_in_preroll_fed"),
        "{log:?}"
    );
    let snap = state.voice.mode_snapshot();
    let dev = &snap["devices"][0];
    assert_eq!(dev["barge"]["stops"], 0);
    assert_eq!(dev["barge"]["ignored"], 1);
    assert_eq!(dev["barge"]["protected"], true);
}

/// R1 reverse direction: past the protection period a genuine fact still
/// interrupts — once. A second fact while the interruption episode is open
/// is a duplicate, and the pre-roll feeds exactly once (R2/R3).
#[tokio::test]
async fn barge_in_one_stop_and_one_preroll_per_episode() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_mic_app(llm, "别讲了，换个话题").await;
    // R1g: the near-end local stop and direct barge-in are FULL-duplex
    // abilities; this test exercises that row of the matrix.
    report_duplex(&gw, "full").await;

    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    gw.send(&barge_in_frame(2, 5)).await;
    // Duplicate fact from the same utterance: must not stop twice.
    gw.send(&barge_in_frame(3, 5)).await;

    let stopped = stops_within(&gw, Duration::from_millis(600)).await;
    assert_eq!(
        stopped,
        vec![play_id],
        "exactly one stop per interruption: {stopped:?}"
    );
    gw.send(&GatewayFrame::StopReceipt {
        task_id: stopped[0].clone(),
        stage: "device_confirmed".into(),
        played_ms: Some(0),

        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;

    // Drive the gate open once (the interrupting speech), then close it.
    let loud = loud_block();
    let quiet = quiet_block();
    for seq in 4..=6 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 7..=14 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }
    // The event pump is asynchronous: let the blocks reach the pipeline
    // before counting the pre-roll feed.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let log = state.voice.recent(400);
    let held = log
        .iter()
        .filter(|e| e["kind"] == "barge_in_preroll_held")
        .count();
    assert_eq!(
        held, 1,
        "the pre-roll is held exactly once per episode: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "barge_in_preroll_fed"),
        "no pre-roll may reach the VAD (it would open a segment on our own answer): {log:?}"
    );
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_in_ignored" && e["reason"] == "episode_active"),
        "the duplicate must be recorded with its reason: {log:?}"
    );
    let snap = state.voice.mode_snapshot();
    let dev = &snap["devices"][0];
    assert_eq!(dev["barge"]["stops"], 1);
}

/// R2 acceptance: once the follow-up ANSWER starts, the interruption window
/// is gone — a new fact is no longer "the answer is interruptible"; it is
/// either the new answer's own onset transient (protected) or a genuinely
/// new utterance after the protection. The window must not outlive the stop.
#[tokio::test]
async fn barge_in_window_closes_when_the_next_answer_starts() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let follow_up = "好的，讲个短的。".repeat(60);
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec![long_answer],
        },
        Scripted::Text {
            chunks: vec![follow_up],
        },
    ]);
    let (_base, state, gw) = spawn_mic_app(llm, "别讲了，换个话题").await;
    // R1g: the near-end local stop and direct barge-in are FULL-duplex
    // abilities; this test exercises that row of the matrix.
    report_duplex(&gw, "full").await;

    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    gw.send(&barge_in_frame(2, 5)).await;
    let stopped = stops_within(&gw, Duration::from_millis(600)).await;
    assert_eq!(stopped, vec![play_id.clone()]);
    gw.send(&GatewayFrame::StopReceipt {
        task_id: play_id,
        stage: "device_confirmed".into(),
        played_ms: Some(0),
        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;

    // Close the interrupting segment: the follow-up turn answers, and its
    // first submitted piece CLOSES the barge window (R2).
    let loud = loud_block();
    let quiet = quiet_block();
    for seq in 3..=5 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 6..=13 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }
    let (play2, _) = gw.until_play("voice").await;
    gw.confirm_play(&play2).await;

    gw.send(&barge_in_frame(14, 5)).await;
    let stopped2 = stops_within(&gw, Duration::from_millis(500)).await;
    assert!(
        stopped2.is_empty(),
        "the new answer's onset must not be cut: {stopped2:?}"
    );

    let snap = state.voice.mode_snapshot();
    let dev = &snap["devices"][0];
    assert_eq!(
        dev["barge"]["window_open"], false,
        "window must close when the next answer starts"
    );
    assert_eq!(dev["barge"]["protected"], true);
    let log = state.voice.recent(400);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_in_ignored" && e["reason"] == "answer_protect"),
        "{log:?}"
    );
}

/// Switch off: the device still reports the fact, the pipeline records and
/// ignores (same honest shape as the keyword switch).
#[tokio::test]
async fn barge_in_ignored_when_disabled() {
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
        None,
        Some(false),
        None,
        None,
        None,
        None,
        "test",
    );

    let _play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    gw.send(&barge_in_frame(2, 5)).await;

    let stopped = stops_within(&gw, Duration::from_millis(500)).await;
    assert!(
        stopped.is_empty(),
        "disabled barge-in must not stop anything, saw {stopped:?}"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_in_ignored" && e["reason"] == "disabled"),
        "must log the ignore reason: {log:?}"
    );
}

/// Scope: media playback is not the barge-in's case (the chunk gate is
/// open during media; the normal ASR wake path owns interrupting music).
/// The turn's spoken answer ("放上了。") is confirmed FIRST — with a live
/// voice play the stop would be correct behavior, not this test's case.
///
/// R1g: on a HALF-duplex link a direct near-end fact stops nothing, whatever
/// channel is playing — the reason is the duplex rule, not a scope.
#[tokio::test]
async fn a_direct_barge_during_media_is_suppressed_on_a_half_duplex_link() {
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
    let (_media_id, _) = gw.until_media_play().await;
    let (answer_id, _) = gw.until_any_voice_play().await;
    gw.confirm_play(&answer_id).await;
    // The answer must be FINISHED, not just confirmed: a live voice play
    // would make the stop correct behavior, not this test's case.
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: answer_id,
        played_ms: 900,
        ts: hm_core::id::now_ms(),

        consumed_ms: None,
        progress_basis: None,
    })
    .await;

    gw.send(&barge_in_frame(2, 5)).await;

    let stopped = stops_within(&gw, Duration::from_millis(500)).await;
    assert!(
        stopped.is_empty(),
        "media playback must not stop on a barge-in: {stopped:?}"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter().any(|e| e["kind"] == "barge_in_suppressed"
            && e["reason"] == "direct_barge_suppressed_half_duplex"
            && e["content"]["media"] == true),
        "must record the half-duplex suppression with the content: {log:?}"
    );
}

/// Idle: nothing to interrupt, the fact is recorded and dropped.
#[tokio::test]
async fn barge_in_with_no_playback_is_ignored() {
    let llm = FakeLlm::new(vec![]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    gw.send(&barge_in_frame(1, 5)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log = state.voice.recent(100);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_in_ignored" && e["reason"] == "direct_barge_ignored_idle"),
        "must record direct_barge_ignored_idle: {log:?}"
    );
}

/// R1d (2026-11-12 night): the device cut its own voice channel on the
/// near-end reflex, and the fact only reached the core later (3.078 s
/// measured). Two things must hold:
///
/// 1. The stop is already a hardware fact — the core records it, cancels the
///    turn, and must NOT send a second Stop for audio that is already gone
///    ("core 只决定算不算一轮").
/// 2. The late fact must still become a turn. Without the local-stop marker
///    `no_voice_playback` would refuse it — a lie, because the audio it
///    describes was playing when it fired.
#[tokio::test]
async fn a_fact_after_a_local_stop_still_becomes_a_turn() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec![long_answer],
        },
        Scripted::Text {
            chunks: vec!["好的，换个话题。".into()],
        },
    ]);
    let (_base, state, gw) = spawn_mic_app(llm, "别讲了，换个话题").await;
    // R1g: the near-end local stop and direct barge-in are FULL-duplex
    // abilities; this test exercises that row of the matrix.
    report_duplex(&gw, "full").await;

    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    tokio::time::sleep(Duration::from_millis(900)).await;

    // The device's reflex: the play is cut, the hardware fact names it.
    gw.send(&GatewayFrame::StoppedLocally {
        device: "dot2-d".into(),
        task_id: play_id.clone(),
        reason: "near_end".into(),
        played_ms: 900,
        latency_ms: 35,
        ahead_ms: 2_000,
        ts: hm_core::id::now_ms(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let log = state.voice.recent(100);
    assert!(
        log.iter().any(|e| e["kind"] == "stopped_locally"
            && e["task_id"] == play_id.as_str()
            && e["ahead_ms"] == 2_000),
        "the device's own stop must be recorded with its task and its read-ahead: {log:?}"
    );

    // The core must not chase an audio channel that is already silent.
    let stops = stops_within(&gw, Duration::from_millis(300)).await;
    assert!(
        stops.is_empty(),
        "no Stop may follow a local stop: {stops:?}"
    );

    // The fact arrives late (the measured wire + pump latency). It must be
    // accepted on the strength of the local stop, not refused.
    gw.send(&barge_in_frame(2, 5)).await;
    let late = stops_within(&gw, Duration::from_millis(400)).await;
    assert!(
        late.is_empty(),
        "a late fact must not re-stop anything: {late:?}"
    );

    // The interrupted speech forms the follow-up turn (the window opened).
    let loud = loud_block();
    let quiet = quiet_block();
    for seq in 3..=5 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 6..=13 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }
    let (play2, _) = gw.until_play("voice").await;
    gw.confirm_play(&play2).await;

    let log = state.voice.recent(300);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_in_stop" && e["locally_stopped"] == true),
        "the late fact must be accepted with the local-stop evidence: {log:?}"
    );
    assert!(
        log.iter()
            .any(|e| e["kind"] == "mic_utterance" && e["source"] == "asr"),
        "the words after the local stop must run as a turn: {log:?}"
    );
}

/// The console must see the whole barge-in truth: the global switch, the
/// per-device stops/ignored counters, and the spotter's guard-suppressed
/// count (v0.2.5).
#[tokio::test]
async fn barge_in_and_kws_guard_visible_in_mode_snapshot() {
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
    assert_eq!(snap["barge_in"], true);
    let dev = snap["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == "dot2-d")
        .unwrap();
    assert_eq!(dev["kws"]["guarded"], 2);
    assert_eq!(dev["barge"]["stops"], 0);
    assert_eq!(dev["barge"]["ignored"], 0);

    // Toggle through the console API and confirm the applied state.
    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({ "barge_in": false }))
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
    assert_eq!(snap["barge_in"], false);
    assert_eq!(state.voice.mode_snapshot()["barge_in"], false);
}

/// The 2026-11-09 on-device stutter: a barge-in stopped the sentence that
/// was playing, but the turn kept streaming the remaining sentences — each
/// one restarted the speaker and got cut again ("一直卡"). The LLM here is
/// PACED (one sentence every 200 ms), so the interruption lands in the gap
/// between two sentences, which is exactly the window that made the old
/// per-sentence epoch snapshot look "fresh".
#[tokio::test]
async fn barge_in_stops_the_whole_streamed_turn() {
    let llm = FakeLlm::new(vec![Scripted::TextPaced {
        chunks: vec![
            "第一句话在讲一个很长很长的故事的开头。".into(),
            "第二句话接着讲中间发生了什么。".into(),
            "第三句话讲最后大家都很开心。".into(),
            "第四句话是收尾。".into(),
        ],
        delay_ms: 200,
    }]);
    let (_base, state, gw) = spawn_mic_app_stream_tts(
        llm,
        "讲个长故事",
        Arc::new(SlowStreamTts {
            synth_ms: 80,
            piece_ms: 900,
        }),
    )
    .await;
    // R1g: the near-end local stop and direct barge-in are FULL-duplex
    // abilities; this test exercises that row of the matrix.
    report_duplex(&gw, "full").await;
    // Calibration knob: no answer-protection delay for this test — the
    // interruption is deliberately mid-answer.
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
        None,
        None,
        Some(0),
        None,
        None,
        None,
        "test",
    );

    explicit_request(&state, &gw, 1, "讲个长故事").await;
    gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await;
    // Sentence 2 is still 100+ ms away: the stream is live, the turn is not.
    tokio::time::sleep(Duration::from_millis(120)).await;
    gw.send(&barge_in_frame(2, 5)).await;

    // Collect stops across the remaining sentences: exactly ONE distinct
    // play may be stopped. The old code re-opened a stream per sentence,
    // each one a fresh play id that then got stopped too.
    let stopped = stops_within(&gw, Duration::from_millis(1100)).await;
    let distinct: std::collections::BTreeSet<&String> = stopped.iter().collect();
    assert_eq!(
        distinct.len(),
        1,
        "a barge-in stops one stream and the turn (no re-opened sentence): {stopped:?}"
    );
    let log = state.voice.recent(400);
    assert!(
        log.iter().any(|e| e["kind"] == "turn_barged_in"),
        "the turn must record that it was invalided: {log:?}"
    );
}

/// R0 (2026-11-13): the firmware confirms a stop only after the hardware
/// flush, and states the audio the DAC actually took. That number is the
/// only way to tell "stopped at 0 ms" (the T4 defect: receipt early, DAC
/// kept playing) from a real stop — so it must reach the event log, not be
/// dropped at the protocol edge.
#[tokio::test]
async fn stop_receipt_played_ms_reaches_the_event_log() {
    let llm = FakeLlm::new(vec![]);
    let (_base, state, gw) = spawn_mic_app(llm, "").await;

    gw.send(&GatewayFrame::StopReceipt {
        task_id: "stop-pm".into(),
        stage: "device_confirmed".into(),
        played_ms: Some(12_345),

        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let log = state.voice.recent(50);
    assert!(
        log.iter().any(|e| e["kind"] == "stop_stage"
            && e["play_id"] == "stop-pm"
            && e["played_ms"] == 12_345),
        "the stop's hardware fact must be recorded: {log:?}"
    );

    // A device that does not measure (older firmware omits the field)
    // stays honest with null instead of a fabricated number.
    gw.send(&GatewayFrame::StopReceipt {
        task_id: "stop-none".into(),
        stage: "device_confirmed".into(),
        played_ms: None,

        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let log = state.voice.recent(50);
    assert!(
        log.iter().any(|e| e["kind"] == "stop_stage"
            && e["play_id"] == "stop-none"
            && e["played_ms"].is_null()
            && e["stop_outcome"]["outcome"] == "unknown"),
        "an unmeasured stop must be null, not 0: {log:?}"
    );
}

/// A slow answer gets exactly ONE waiting cue, and the cue is cancelled (on
/// the wire) the moment the answer's own audio is submitted. Criteria M0-2.
#[tokio::test]
async fn slow_turn_plays_one_waiting_cue_then_the_answer_replaces_it() {
    let llm = FakeLlm::new(vec![Scripted::Delayed {
        delay_ms: 700,
        chunks: vec!["今天没什么新鲜事。".into()],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    set_filler(&state, one_cue(200, "我想想。")).await;

    explicit_request(&state, &gw, 1, "今天有什么新鲜事").await;

    // The cue comes first, on the voice channel, from the device-local cache.
    let cue = gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("filler:")
            )
        })
        .await;
    // R2-BM5/BM5-6: the waiting cue is kind=filler, not an answer.
    {
        let mode: Value = reqwest::get(format!("{_base}/api/v0/mode"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let outs = mode["devices"][0]["outputs"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            outs.iter().any(|o| o["kind"] == "filler"),
            "the cue must be kind=filler: {outs:?}"
        );
    }
    let cue_id = match &cue {
        CoreFrame::PlayBegin {
            task_id,
            content_ref,
            text,
            ..
        } => {
            assert_eq!(content_ref, "filler:我想想。");
            assert!(text.is_none(), "a waiting cue carries no answer text");
            task_id.clone()
        }
        CoreFrame::Play {
            task_id,
            content_ref,
            audio_b64,
            text,
            ..
        } => {
            assert_eq!(content_ref, "filler:我想想。");
            assert!(
                audio_b64.is_some(),
                "the cue must be a synthesized WAV, not cloud TTS"
            );
            assert!(text.is_none(), "a waiting cue carries no answer text");
            task_id.clone()
        }
        f => panic!("expected filler play, got {f:?}"),
    };

    // The answer arrives and the cue is stopped on the wire before it plays.
    // One pass over the window: `until` would discard the stop while waiting
    // for the answer play.
    let mut cue_stopped = false;
    let mut cue_stop_flushes = false;
    let answer = loop {
        match tokio::time::timeout(Duration::from_secs(5), gw.recv()).await {
            Ok(CoreFrame::Stop { task_id, flush }) if task_id == cue_id => {
                cue_stopped = true;
                cue_stop_flushes = flush;
                // R1j/P0-a: the device answers a stop only after its flush;
                // without this receipt the answer waits out its bound.
                gw.confirm_stop(&cue_id).await;
            }
            Ok(f @ (CoreFrame::Play { .. } | CoreFrame::PlayBegin { .. })) => match &f {
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("tts:") =>
                {
                    break f
                }
                other => panic!("unexpected play before the answer: {other:?}"),
            },
            Ok(_) => {}
            Err(_) => panic!("timeout waiting for the answer play"),
        }
    };
    match &answer {
        CoreFrame::Play { text, .. } | CoreFrame::PlayBegin { text, .. } => {
            assert_eq!(text.as_deref(), Some("今天没什么新鲜事。"))
        }
        f => panic!("expected answer play, got {f:?}"),
    }
    assert!(
        cue_stopped,
        "the cue must be stopped on the wire before the answer"
    );
    assert!(
        cue_stop_flushes,
        "replacing a cue must flush the device queue"
    );

    // Exactly one cue, and the log states why it ended.
    let played = voice_log_has(&state, "filler_played");
    assert_eq!(played.len(), 1, "one cue per turn: {played:?}");
    assert_eq!(played[0]["scene"], "thinking");
    let cancelled = voice_log_has(&state, "filler_cancelled");
    assert_eq!(
        cancelled.len(),
        1,
        "the cue must record its replacement: {cancelled:?}"
    );
    assert_eq!(cancelled[0]["reason"], "answer_started");
    assert_eq!(cancelled[0]["play_id"], json!(cue_id));
}

/// R1j/P0-a: the answer's first audio must not be submitted until the device
/// has actually flushed the cancelled cue. This fake device delays its stop
/// receipt on purpose: the answer play has to wait for it.
#[tokio::test]
async fn the_answer_waits_for_the_cue_flush_receipt() {
    let llm = FakeLlm::new(vec![Scripted::Delayed {
        delay_ms: 700,
        chunks: vec!["已经关好了。".into()],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    set_filler(&state, one_cue(200, "我想想。")).await;

    explicit_request(&state, &gw, 1, "今天有什么新鲜事").await;
    let cue_id = match gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("filler:")
            )
        })
        .await
    {
        CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } => task_id,
        f => panic!("expected filler play, got {f:?}"),
    };

    let stop_at = loop {
        match tokio::time::timeout(Duration::from_secs(5), gw.recv()).await {
            Ok(CoreFrame::Stop { task_id, .. }) if task_id == cue_id => {
                break tokio::time::Instant::now();
            }
            Ok(_) => {}
            Err(_) => panic!("timeout waiting for the cue stop"),
        }
    };
    // Hold the "flush" for 120 ms — the answer must wait for it, not race it.
    tokio::time::sleep(Duration::from_millis(120)).await;
    gw.confirm_stop(&cue_id).await;

    let answer_at = loop {
        match tokio::time::timeout(Duration::from_secs(5), gw.recv()).await {
            Ok(CoreFrame::Play { .. } | CoreFrame::PlayBegin { .. }) => {
                break tokio::time::Instant::now()
            }
            Ok(_) => {}
            Err(_) => panic!("timeout waiting for the answer play"),
        }
    };
    let waited = answer_at.duration_since(stop_at);
    assert!(
        waited >= Duration::from_millis(110),
        "the answer must wait for the cue's flush receipt, waited only {waited:?}"
    );

    let settled = voice_log_has(&state, "filler_stop_settled");
    assert_eq!(settled.len(), 1, "the wait must be recorded: {settled:?}");
    assert_eq!(settled[0]["settled"], true);
    assert_eq!(settled[0]["detail"], "flushed");
    assert!(settled[0]["waited_ms"].as_u64().unwrap() >= 110);
}

/// R1j/P0-a: a missing flush receipt is reported, never hidden — and above
/// all it must not mute the turn. The answer starts after the bounded wait.
#[tokio::test]
async fn a_missing_cue_receipt_is_reported_and_does_not_mute_the_turn() {
    let llm = FakeLlm::new(vec![Scripted::Delayed {
        delay_ms: 700,
        chunks: vec!["已经关好了。".into()],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    set_filler(&state, one_cue(200, "我想想。")).await;

    explicit_request(&state, &gw, 1, "今天有什么新鲜事").await;
    let cue_id = match gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("filler:")
            )
        })
        .await
    {
        CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } => task_id,
        f => panic!("expected filler play, got {f:?}"),
    };
    let stop_at = loop {
        match tokio::time::timeout(Duration::from_secs(5), gw.recv()).await {
            Ok(CoreFrame::Stop { task_id, .. }) if task_id == cue_id => {
                break tokio::time::Instant::now()
            }
            Ok(_) => {}
            Err(_) => panic!("timeout waiting for the cue stop"),
        }
    };
    // No confirm_stop: the answer may wait at most the bound, then speak.
    let answer_at = loop {
        match tokio::time::timeout(Duration::from_secs(5), gw.recv()).await {
            Ok(CoreFrame::Play { .. } | CoreFrame::PlayBegin { .. }) => {
                break tokio::time::Instant::now()
            }
            Ok(_) => {}
            Err(_) => panic!("a missing receipt must not mute the turn"),
        }
    };
    let waited = answer_at.duration_since(stop_at);
    assert!(
        waited >= Duration::from_millis(180) && waited < Duration::from_millis(900),
        "the bounded wait was {waited:?}"
    );
    let settled = voice_log_has(&state, "filler_stop_settled");
    assert_eq!(settled.len(), 1, "{settled:?}");
    assert_eq!(settled[0]["settled"], false);
    assert_eq!(settled[0]["detail"], "timeout");
}

/// A quick answer never pays a waiting cue — the timer loses the race.
/// Criteria M0-1.
#[tokio::test]
async fn quick_turn_pays_no_waiting_cue() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["没什么新鲜事。".into()],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    set_filler(&state, one_cue(200, "我想想。")).await;

    explicit_request(&state, &gw, 1, "今天有什么新鲜事").await;
    let (play_id, meta) = gw.until_play("voice").await;
    assert_eq!(
        meta["text"].as_str(),
        Some("没什么新鲜事。"),
        "the first sound must be the answer"
    );
    gw.confirm_play(&play_id).await;

    // Give the cue timer more than its deadline: it must never fire.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let played = voice_log_has(&state, "filler_played");
    assert!(
        played.is_empty(),
        "a ready answer must not be preceded by a cue: {played:?}"
    );
    assert!(
        !state
            .voice
            .recent(400)
            .iter()
            .any(|e| e["kind"] == "filler_cancelled"),
        "nothing to cancel either"
    );
}

/// An interrupt during the cue stops it and the old turn never speaks again.
/// Criteria M0-4.
#[tokio::test]
async fn interrupt_during_the_cue_leaves_no_late_answer() {
    let llm = FakeLlm::new(vec![Scripted::Delayed {
        delay_ms: 900,
        chunks: vec!["迟到的回答。".into()],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    // 200 ms is the validated floor — 150 would be rejected (M0-7).
    set_filler(&state, one_cue(200, "我想想。")).await;

    explicit_request(&state, &gw, 1, "今天有什么新鲜事").await;
    let cue_id = match gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("filler:")
            )
        })
        .await
    {
        CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } => task_id,
        f => panic!("expected filler play, got {f:?}"),
    };

    // The action button (hardware stop) is the generic "user said stop" path.
    gw.send(&GatewayFrame::Button {
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 2,
        action: "action_press".into(),
    })
    .await;

    // The cue is stopped (stop_device fans out to every play).
    gw.until(|f| matches!(f, CoreFrame::Stop { task_id, .. } if task_id == &cue_id))
        .await;

    // Past the original answer delay: the stale turn must stay silent.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let log = state.voice.recent(400);
    assert!(
        !log.iter()
            .any(|e| matches!(e["kind"].as_str(), Some("filler_played"))
                && e["play_id"] != json!(cue_id)),
        "no second cue for the same turn: {log:?}"
    );
    let cancelled = voice_log_has(&state, "filler_cancelled");
    assert!(
        cancelled.iter().any(|e| e["reason"] == "turn_stopped"),
        "the stopped turn must record its cue ending: {cancelled:?}"
    );
    assert!(
        log.iter().any(|e| e["kind"] == "turn_barged_in"),
        "the turn itself must be recorded stale: {log:?}"
    );
}

/// The console can change one cue phrase and the next slow turn speaks the
/// NEW phrase (criteria M0-3), and clearing a scene disables only that scene
/// (M0-5) through the same validated API.
#[tokio::test]
async fn editing_a_cue_phrase_takes_effect_and_an_empty_scene_is_off() {
    let llm = FakeLlm::new(vec![
        Scripted::Delayed {
            delay_ms: 600,
            chunks: vec!["第一答。".into()],
        },
        Scripted::Delayed {
            delay_ms: 600,
            chunks: vec!["第二答。".into()],
        },
    ]);
    let (base, state, gw) = spawn_app_with_gateway(llm).await;

    // Console edit: replace the thinking cue, keep the others off.
    let put = reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({ "filler": {
            "delay_ms": 200,
            "thinking": ["让我看看。"],
            "search": [], "device": [], "story": [],
        }}))
        .send()
        .await
        .unwrap();
    assert!(
        put.status().is_success(),
        "valid filler config must be accepted"
    );
    state.voice.synthesize_wake_acks(false).await;

    // The saved value reads back on the same endpoint.
    let mode: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mode["filler"]["thinking"][0], "让我看看。");
    assert_eq!(mode["filler"]["delay_ms"], 200);
    assert_eq!(mode["filler"]["story"], json!([]));

    explicit_request(&state, &gw, 1, "今天有什么新鲜事").await;
    match gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("filler:")
            )
        })
        .await
    {
        CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. } => {
            assert_eq!(content_ref, "filler:让我看看。")
        }
        f => panic!("expected filler play, got {f:?}"),
    }

    // A disabled scene stays silent even though the cue cache is warm.
    explicit_request(&state, &gw, 2, "讲个故事").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let played = voice_log_has(&state, "filler_played");
    assert_eq!(
        played.len(),
        1,
        "only the thinking turn gets a cue: {played:?}"
    );
    assert_eq!(played[0]["scene"], "thinking");

    // Invalid values are rejected with a message, not stored.
    let bad = reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({ "filler": { "delay_ms": 100, "thinking": [], "search": [], "device": [], "story": [] }}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = bad.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("200"),
        "server message names the bound: {body}"
    );
}

/// Regression (found on the real device 2026-11-12): the answer's first
/// audio can arrive WHILE the turn loop is awaiting TTS, so the cue timer
/// expires overdue and used to be submitted ON TOP of the live answer
/// stream — the cue superseded the answer it was waiting for. The cue must
/// only ever cover silence.
#[tokio::test]
async fn an_answer_that_beats_the_deadline_is_never_superseded_by_the_cue() {
    // First token WELL inside the cue deadline, first AUDIO well after it.
    // The margins are wide on purpose: with 100/500 these two timings are
    // close enough that a loaded test machine flips the order (seen as a
    // flake 2026-11-12), which would be testing the scheduler, not the guard.
    let llm = FakeLlm::new(vec![Scripted::Delayed {
        delay_ms: 40,
        chunks: vec!["答案是四十。".into()],
    }]);
    let tts = Arc::new(SlowStreamTts {
        synth_ms: 700,
        piece_ms: 200,
    });
    let (base, state, gw) = spawn_mic_app_stream_tts(llm, "你好，高达，四乘十是多少", tts).await;
    let _ = base;
    set_filler(&state, one_cue(400, "我想想。")).await;

    let loud = loud_block();
    let quiet = quiet_block();
    for seq in 1..=10 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 11..=20 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }

    // The answer's stream opens (its synthesis outlived the cue deadline).
    gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await;

    // Nothing may supersede it: drain the window and look for a cue play.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
    let mut cue_after_answer = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, gw.recv()).await {
            Ok(CoreFrame::Play {
                task_id,
                content_ref,
                ..
            })
            | Ok(CoreFrame::PlayBegin {
                task_id,
                content_ref,
                ..
            }) => {
                if content_ref.starts_with("filler:") {
                    cue_after_answer.push((task_id, content_ref));
                }
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(
        cue_after_answer.is_empty(),
        "the cue must not replace a live answer: {cue_after_answer:?}"
    );
    // The invariant is "no cue after the answer"; the skip NOTE is
    // best-effort telemetry (a turn whose stream ends before the overdue
    // timer is polled never needs one). Either proof is accepted — a cue
    // that played, or a cue after the answer, is not.
    let skipped = voice_log_has(&state, "filler_skipped");
    let played = voice_log_has(&state, "filler_played");
    assert!(
        skipped.iter().any(|e| e["skipped"] == "answer_already_playing") || played.is_empty(),
        "either the cue was skipped for a live answer, or it never played:          skipped={skipped:?} played={played:?}"
    );
}

/// R1g/P4 (user: "垫话默认 radio（电台风）… 仅适用于音乐播放"): in radio mode the
/// cue may talk OVER the music. A song starting (or already playing) is not a
/// reason to cut it; only the answer's own voice replaces it, in the normal
/// order.
#[tokio::test]
async fn a_song_does_not_cut_the_cue_in_radio_mode() {
    let (_base, state, gw, _music) = spawn_app_with_gateway_media(slow_turn_then_music(300)).await;
    set_filler(&state, one_cue(200, "我想想这个到底是怎么回事。")).await;

    explicit_request(&state, &gw, 1, "放第一名的歌").await;
    let cue_id = match gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("filler:")
            )
        })
        .await
    {
        CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } => task_id,
        f => panic!("expected filler play, got {f:?}"),
    };
    let _ = gw.until_media_play().await;
    // The media receipts arrive asynchronously: give the core the window it
    // needs to run its policy before judging it.
    tokio::time::sleep(Duration::from_millis(250)).await;

    assert!(
        voice_log_has(&state, "filler_skipped").is_empty(),
        "radio mode: a song may start under the cue: {:?}",
        voice_log_has(&state, "filler_skipped")
    );
    let cancelled = voice_log_has(&state, "filler_cancelled");
    assert!(
        !cancelled.iter().any(|e| e["play_id"] == json!(cue_id)
            && e["reason"]
                .as_str()
                .unwrap_or("")
                .starts_with("other_play_started")),
        "radio mode: the song must not cut the cue: {cancelled:?}"
    );
    assert!(
        voice_log_has(&state, "filler_played")
            .iter()
            .any(|e| e["play_id"] == json!(cue_id)),
        "the cue really played: {:?}",
        voice_log_has(&state, "filler_played")
    );
}

/// The same timeline under `interrupt`: the cue only covers silence, so it
/// ends the moment another play starts (the older, stricter reading).
#[tokio::test]
async fn a_song_cuts_the_cue_in_interrupt_mode() {
    let (_base, state, gw, _music) = spawn_app_with_gateway_media(slow_turn_then_music(500)).await;
    set_filler(
        &state,
        cue_with_mode(
            200,
            "我想想这个到底是怎么回事。",
            hm_core::voice::FillerMode::Interrupt,
        ),
    )
    .await;
    assert_eq!(
        state.voice.mode_snapshot()["filler"]["mode"],
        "interrupt",
        "premise: the interrupt mode is applied"
    );

    explicit_request(&state, &gw, 1, "放第一名的歌").await;
    let cue_id = match gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("filler:")
            )
        })
        .await
    {
        CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } => task_id,
        f => panic!("expected filler play, got {f:?}"),
    };
    let _ = gw.until_media_play().await;
    // The cancel decision runs in the pipeline's event pump, after the
    // receipt: poll instead of assuming a fixed delay.
    wait_until_sync("the cue is cancelled by the song", || {
        voice_log_has(&state, "filler_cancelled")
            .iter()
            .any(|e| e["play_id"] == json!(cue_id))
    })
    .await;

    let cancelled = voice_log_has(&state, "filler_cancelled");
    assert!(
        cancelled
            .iter()
            .any(|e| e["play_id"] == json!(cue_id) && e["reason"] == "other_play_started:media"),
        "interrupt mode: the song ends the cue: {cancelled:?}"
    );
}

/// The mode rides the same config/validation/read-back path as the rest of
/// the cue settings (a management API change the console edits).
#[tokio::test]
async fn the_cue_mode_round_trips_through_the_mode_api() {
    let (_base, state, gw) = spawn_app_with_gateway(FakeLlm::new(vec![])).await;
    let snap = state.voice.mode_snapshot();
    assert_eq!(snap["filler"]["mode"], "radio", "radio is the default");

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
        None,
        None,
        None,
        Some(cue_with_mode(
            1800,
            "我想想。",
            hm_core::voice::FillerMode::Interrupt,
        )),
        None,
        None,
        "test",
    );
    assert_eq!(state.voice.mode_snapshot()["filler"]["mode"], "interrupt");
    let _ = gw;
}

// --- B-M1: stop / don't-stop / retreat / expiry --------------------------------

/// "停" is a LOCAL stop: the playback stops, no model turn runs, nothing is
/// said back (talking back is not what a stop asks for). Criteria M1-1.
#[tokio::test]
async fn stop_word_stops_without_a_model_turn() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["这是一个很长的回答。".to_string(); 8],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm.clone()).await;

    explicit_request(&state, &gw, 1, "讲个长故事").await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;
    let calls_before = llm.seen_requests().len();

    // The user interrupts with the stop word (text path = same intent).
    explicit_request(&state, &gw, 2, "停").await;

    // The live play is stopped...
    gw.until(|f| matches!(f, CoreFrame::Stop { task_id, .. } if task_id == &play_id))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // ...and the stop never became a question for the model.
    assert_eq!(
        llm.seen_requests().len(),
        calls_before,
        "a stop word must not reach the model"
    );
    let log = state.voice.recent(200);
    let stop = log
        .iter()
        .find(|e| e["kind"] == "stop_intent")
        .unwrap_or_else(|| panic!("stop_intent missing: {log:?}"));
    assert_eq!(stop["via"], "utterance");
    assert_eq!(
        stop["voice_live"], true,
        "the stop must see the live playback"
    );
}

/// "不要停" is a keep-going request, NOT a stop, and it must not be answered
/// as a question either. Honest limit, measured not hidden: the device's
/// barge-in stops at the onset, so the request arrives after the audio is
/// already gone — the event records that instead of claiming a resume.
/// Criteria M1-2.
#[tokio::test]
async fn dont_stop_is_never_read_as_stop_and_is_counted() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["这是一个很长的回答。".to_string(); 8],
        },
        Scripted::Text {
            chunks: vec!["好的，我接着说。".into()],
        },
    ]);
    let (_base, state, gw) = spawn_app_with_gateway(llm.clone()).await;

    explicit_request(&state, &gw, 1, "讲个长故事").await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;
    let calls_before = llm.seen_requests().len();

    explicit_request(&state, &gw, 2, "不要停").await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let log = state.voice.recent(200);
    assert!(
        log.iter().any(|e| e["kind"] == "continue_intent"),
        "the keep-going intent must be recorded: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "stop_intent"),
        "the negation must never be classified as a stop: {log:?}"
    );
    assert_eq!(
        llm.seen_requests().len(),
        calls_before + 1,
        "when nothing is paused/resumable, a keep-going utterance must hand off to the model instead of being swallowed"
    );
    // The counter is the acceptance metric ("误停 0/轮" measured honestly).
    let mode: Value = reqwest::get(format!("{_base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mode["devices"][0]["stop_intents"]["continue_intents"], 1);
}

/// The wake ack's own words at the head of the next transcript are OUR echo,
/// not the user's request: they are stripped, recorded, and the turn asks the
/// remaining question. Criteria M1-3 ("在呢" source explainable).
#[tokio::test]
async fn wake_ack_echo_is_attributed_and_stripped_from_the_transcript() {
    /// ASR whose reply the test rewrites between segments.
    struct MutableAsr {
        reply: Arc<std::sync::Mutex<String>>,
    }
    #[async_trait::async_trait]
    impl hm_core::speech::AsrClient for MutableAsr {
        async fn transcribe(&self, _wav: &[u8]) -> Result<String, hm_core::speech::SpeechError> {
            Ok(self.reply.lock().unwrap().clone())
        }
        fn provider(&self) -> &str {
            "mutable"
        }
    }

    let reply = Arc::new(std::sync::Mutex::new("你好，高达".to_string()));
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["没什么新鲜事。".into()],
    }]);
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm.clone())),
        Some(Arc::new(MockTts::default())),
        Some(Arc::new(MutableAsr {
            reply: reply.clone(),
        })),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
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
    tokio::time::sleep(Duration::from_millis(50)).await;
    let gw = connect_gateway_audio_in(gw_port).await;
    enable_follow_window(&state);
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _base = format!("http://{addr}");

    let loud = loud_block();
    let quiet = quiet_block();
    // 1) Bare wake -> the pre-synthesized ack plays from the device.
    for seq in 1..=10 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 11..=20 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }
    let ack_id = match gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("ack:")
            )
        })
        .await
    {
        CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } => task_id,
        f => panic!("expected ack play, got {f:?}"),
    };
    // The ack finishes; the 400 ms tail-blind window then has to pass before
    // real speech flows again (the gate drops mic audio while we play).
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: ack_id,
        played_ms: 600,
        ts: hm_core::id::now_ms(),

        consumed_ms: None,
        progress_basis: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // 2) The next segment transcribes OUR ack plus the real question.
    *reply.lock().unwrap() = "在呢。外面下雨吗".to_string();
    for seq in 21..=30 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 31..=40 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;

    let log = state.voice.recent(300);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "cue_echo_stripped" && e["phrase"] == "在呢"),
        "the strip must be recorded with its phrase: {log:?}"
    );
    let reqs = llm.seen_requests();
    let last = reqs.last().expect("a turn must reach the model");
    let user = last
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    assert_eq!(
        user, "外面下雨吗",
        "the model sees the question without our own cue"
    );
}

/// Device log 2026-11-12 08:33: after a (false) barge-in the core reopened
/// the mic gate, and the device's OWN answer came back through ASR as
/// "无法提供实施的当前时间。如果你需要知道。" — a fresh turn then answered the
/// assistant's own voice. R1 removes the text-match stopgap: the stopped
/// answer's audio arrives inside the 250 ms tail guard and is discarded
/// before VAD/ASR, so no transcript (and no new turn) can form from it.
#[tokio::test]
async fn our_own_answer_heard_back_is_not_a_question() {
    struct MutableAsr {
        reply: Arc<std::sync::Mutex<String>>,
    }
    #[async_trait::async_trait]
    impl hm_core::speech::AsrClient for MutableAsr {
        async fn transcribe(&self, _wav: &[u8]) -> Result<String, hm_core::speech::SpeechError> {
            Ok(self.reply.lock().unwrap().clone())
        }
        fn provider(&self) -> &str {
            "mutable"
        }
    }

    const ANSWER: &str = "我无法提供实时的当前时间。如果你需要知道，请告诉我。";
    // A real question: the answer must be SPOKEN for the gate to have
    // something to compare against (a bare wake only plays the ack).
    let reply = Arc::new(std::sync::Mutex::new("你好，高达，现在几点了".to_string()));
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![ANSWER.to_string()],
    }]);
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm.clone())),
        Some(Arc::new(MockTts::default())),
        Some(Arc::new(MutableAsr {
            reply: reply.clone(),
        })),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
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
        None,
        None,
        None,
        Some(no_filler()),
        None,
        None,
        "test",
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let gw = connect_gateway_audio_in(gw_port).await;
    // R1g: this test is about the tail guard of an ACCEPTED barge-in, which
    // is the full-duplex row of the matrix.
    report_duplex(&gw, "full").await;
    enable_follow_window(&state);
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _base = format!("http://{addr}");

    let loud = loud_block();
    let quiet = quiet_block();
    // 1) Wake + question -> the answer is spoken (and remembered).
    for seq in 1..=10 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 11..=20 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }
    // Await the answer play so the text is registered, then let the
    // answer-protection window (800 ms) pass — a barge-in inside it is
    // ignored by design, and the device's real case was past it.
    let _ = gw.until_play("voice").await;
    tokio::time::sleep(Duration::from_millis(950)).await;
    let calls_after_answer = llm.seen_requests().len();

    // 2) A barge-in fact arrives (the device thought it heard a person).
    gw.send(&barge_in_frame(21, 5)).await;
    // 3) IMMEDIATELY, inside the 250 ms tail guard: the stopped answer's
    // audio. R1 must discard it before VAD/ASR — this is the structural
    // replacement for the removed text-match gate.
    *reply.lock().unwrap() = "无法提供实施的当前时间。如果你需要知道。".to_string();
    for seq in 22..=31 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 32..=41 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;

    let log = state.voice.recent(300);
    assert!(
        log.iter().any(|e| e["kind"] == "barge_in_tail_dropped"),
        "the stopped answer's tail must be discarded by the 250 ms guard: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "mic_utterance"),
        "our own answer must never reach ASR / become a transcript: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "mic_suppressed_own_answer"),
        "the temporary text-match gate is retired: {log:?}"
    );
    assert_eq!(
        llm.seen_requests().len(),
        calls_after_answer,
        "our own answer must not become a model question"
    );
}

/// R1e acceptance: 「暂停」 said while the answer is playing must PAUSE.
///
/// The 250 ms post-barge guard exists so the stopped answer's own tail cannot
/// open a VAD segment. Its cost was the first syllable: the device heard
/// 「停」, which is the DISCARD intent — "暂停没接上". The fix keeps the guarded
/// audio (preroll ++ guarded chunks) as the HEAD of the segment the user's own
/// speech opens, and the intents read the LAST sentence, so 「暂停」 arrives
/// whole. The head is still never fed to the VAD.
#[tokio::test]
async fn a_pause_spoken_over_the_answer_bookmarks_the_program() {
    let story: Vec<String> = (1..=8)
        .map(|i| format!("第{i}段故事内容，讲的是山谷里的风。"))
        .collect();
    let llm = FakeLlm::new(vec![Scripted::TextPaced {
        chunks: story,
        delay_ms: 60,
    }]);
    let lens = Arc::new(std::sync::Mutex::new(Vec::new()));
    let asr = Arc::new(RecordingAsr {
        reply: "暂停。".into(),
        lens: lens.clone(),
    });
    // The production path: the device advertises play_stream, so the answer
    // is streamed sentence by sentence and the bookmark is a real sentence
    // boundary (the whole-answer fallback can only bookmark at 0).
    let (_base, state, gw) = spawn_mic_app_stream_asr(llm, asr).await;
    // R1g: the near-end local stop and direct barge-in are FULL-duplex
    // abilities; this test exercises that row of the matrix.
    report_duplex(&gw, "full").await;
    // Calibration knob: the interruption is deliberately mid-answer, inside
    // the fresh answer's 800 ms protection period.
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
        None,
        None,
        Some(0),
        None,
        None,
        None,
        "test",
    );

    explicit_request(&state, &gw, 1, "讲个长故事").await;
    gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await;
    // Sentence 1 is in the speaker; generation is still running.
    tokio::time::sleep(Duration::from_millis(150)).await;

    gw.send(&barge_in_frame(2, 5)).await;
    let stopped = stops_within(&gw, Duration::from_millis(600)).await;
    assert!(!stopped.is_empty(), "the barge-in must stop the answer");
    for play in &stopped {
        gw.send(&GatewayFrame::StopReceipt {
            task_id: play.clone(),
            stage: "device_confirmed".into(),
            played_ms: Some(0),

            consumed_ms: None,
            progress_basis: None,
            detail: None,
        })
        .await;
    }

    // Inside the guard (this is the audio that used to be thrown away) …
    let loud = loud_block();
    for seq in 3..=4 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    // … then the user's own speech opens the segment, with its hangover.
    tokio::time::sleep(Duration::from_millis(300)).await;
    for seq in 5..=7 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 8..=15 {
        gw.send(&audio_chunk(seq, &quiet_block())).await;
    }

    // The pause intent consumes the utterance locally: no model turn.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let log = state.voice.recent(400);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "pause_intent" && e["paused"] == true && e["said"] == "暂停。"),
        "暂停 must pause the program: {log:?}"
    );
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_head_released" && e["samples"].as_u64().unwrap_or(0) > 0),
        "the guarded head must reach the VAD: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "stop_intent"),
        "暂停 is a pause, not a discard: {log:?}"
    );
    // The ASR heard MORE than the live audio: the head is what makes the
    // difference between 「暂停」 and 「停」. The segment carries it (the head
    // is fed into the VAD), so its duration is the audibly spoken one.
    let seg = log
        .iter()
        .find(|e| e["kind"] == "mic_segment_closed" && e["barge_in"] == true)
        .expect("a barge-in segment must close");
    let dur_ms = seg["duration_ms"].as_u64().unwrap_or(0);
    assert!(
        dur_ms >= 800,
        "the segment must include the guarded head: {seg:?}"
    );
    let wav_len = *lens.lock().unwrap().last().expect("ASR must have run");
    assert!(
        wav_len >= 44 + (dur_ms as usize) * 16 * 2 / 1000,
        "ASR must receive the whole segment (head included): wav {wav_len} B for {dur_ms} ms"
    );

    let snap = state.voice.mode_snapshot();
    let paused = &snap["devices"][0]["program"]["paused"];
    assert!(
        !paused.is_null(),
        "the bookmark must be visible in the mode snapshot: {snap:?}"
    );
    assert!(
        paused["cursor"].as_u64().unwrap_or(0) > 0,
        "the bookmark is a sentence boundary past the first sentence: {paused:?}"
    );
}
