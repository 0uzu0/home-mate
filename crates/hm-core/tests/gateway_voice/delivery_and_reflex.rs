use super::*;

/// R1j/P1: 暂停播放 is a REFLEX, not a wake: it pauses the live program
/// (keeping the bookmark), opens no window, and the same words arriving
/// later as an ASR transcript are deduped instead of pausing twice.
#[tokio::test]
async fn a_reflex_pause_word_pauses_once_and_dedupes_the_transcript() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    let play_id = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;

    send_keyword(&gw, 2, "暂停播放", false).await;
    let stopped = stops_within(&gw, Duration::from_millis(600)).await;
    assert_eq!(
        stopped,
        vec![play_id.clone()],
        "pause must stop the live audio: {stopped:?}"
    );
    gw.send(&GatewayFrame::StopReceipt {
        task_id: play_id,
        stage: "device_confirmed".into(),
        played_ms: Some(1200),

        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;

    let log = state.voice.recent(300);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "reflex_pause" && e["paused"] == true),
        "the pause intent must be logged with its outcome: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "keyword_wake"),
        "a command word must never be treated as a wake: {log:?}"
    );
    let (hits, ignored) = reflex_word_hits(&state, "暂停播放");
    assert_eq!(
        (hits, ignored),
        (1, 0),
        "per-word counters are the word's own"
    );

    // The same speech arrives as a transcript (VAD closed after the beat):
    // the intent already ran — dedupe, don't pause again.
    explicit_request(&state, &gw, 3, "暂停播放。").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = state.voice.recent(300);
    let deduped: Vec<_> = log
        .iter()
        .filter(|e| e["kind"] == "intent_deduped")
        .collect();
    assert_eq!(deduped.len(), 1, "the transcript must be deduped: {log:?}");
    assert_eq!(deduped[0]["intent"], "pause");
    assert_eq!(deduped[0]["via"], "utterance");
    let pauses = log.iter().filter(|e| e["kind"] == "pause_intent").count();
    assert_eq!(pauses, 0, "no second pause from the transcript");
}

/// User decision 2026-11-15: 「停止播放」 ends the session. It used to be a
/// stop word (interrupt this answer) AND a retreat word depending on who
/// heard it, so on a quiet node it answered "nothing active on this device"
/// (live dot2-e 2026-11-14 23:47 and 23:49, one of them discarding a
/// program bookmark). One word, one meaning, on every entry point.
#[tokio::test]
async fn stop_playback_ends_the_session_not_the_answer() {
    let llm = FakeLlm::new(vec![]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;

    explicit_request(&state, &gw, 1, "停止播放。").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let log = state.voice.recent(300);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "session_retreat" && e["text"] == "停止播放。"),
        "停止播放 must close the session: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "reflex_stop"),
        "and it must not be executed as a stop: {log:?}"
    );

    // 「停」 keeps its old meaning: it interrupts, it does not end the session.
    explicit_request(&state, &gw, 2, "停。").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = state.voice.recent(300);
    let retreats = log
        .iter()
        .filter(|e| e["kind"] == "session_retreat")
        .count();
    assert_eq!(retreats, 1, "「停」 must not add a retreat: {log:?}");
}

/// R1j/P1: 退下 is NOT in the shipped table (the model cannot hear 2-syllable
/// words — measured). A stale keyword file that still spots it must be a
/// no-action fact: no session zeroing, no wake.
#[tokio::test]
async fn a_word_outside_the_table_is_a_no_action_fact() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["在。".into()],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    explicit_request(&state, &gw, 1, "你好高达").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    send_keyword(&gw, 2, "退下", false).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = state.voice.recent(300);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "keyword_ignored" && e["reason"] == "not_in_reflex_table"),
        "an unconfigured word is recorded, never guessed: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "session_retreat"),
        "no retreat from a stray word"
    );
}

/// R1j/P1 (current contract): a self-heard retreat is ignored only when our
/// own live output actually contains the word — a live answer, the live
/// program text, or a locally spoken cue from the last 5 s. Without that
/// evidence the guard's hint is not enough, and the retreat acts (a missed
/// retreat while the speaker talks over itself would be the worse error).
#[tokio::test]
async fn a_self_heard_retreat_without_overlap_is_accepted() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["我在。".into()],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    explicit_request(&state, &gw, 1, "你好高达").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Even with the word configured, the guard's fact wins.
    let cfg = hm_core::config::ReflexConfig {
        words: vec![
            hm_core::config::ReflexWord {
                text: "你好高达".into(),
                tokens: "n ǐ h ǎo g āo d á".into(),
                intent: hm_core::config::ReflexIntent::Wake,
                enabled: true,
            },
            hm_core::config::ReflexWord {
                text: "退下吧啊".into(),
                tokens: "t uì x ià b a a a".into(),
                intent: hm_core::config::ReflexIntent::Retreat,
                enabled: true,
            },
        ],
    };
    state.config.save_reflex(cfg, "test").unwrap();
    send_keyword(&gw, 2, "退下吧啊", true).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = state.voice.recent(300);
    assert!(log.iter().any(|e| e["kind"] == "session_closed"), "{log:?}");
    assert!(log.iter().any(|e| e["kind"] == "session_retreat"));
}

/// R1j/P1: 继续播放 needs a bookmark. Without one it is recorded and ignored;
/// with one it resumes through the SAME path the text layer uses.
#[tokio::test]
async fn a_reflex_continue_word_needs_a_bookmark() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["这是".to_string() + &"很长".repeat(400)],
        },
        Scripted::Text {
            chunks: vec!["继续讲完了。".into()],
        },
    ]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;

    // No bookmark yet.
    send_keyword(&gw, 1, "继续播放", false).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let log = state.voice.recent(300);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "keyword_ignored"
                && e["reason"] == "reflex_precondition:no_bookmark"),
        "{log:?}"
    );

    // Pause a long program, then resume by word.
    let play_id = start_long_voice_play(&state, &gw, 2, "讲个长故事").await;
    send_keyword(&gw, 3, "暂停播放", false).await;
    stops_within(&gw, Duration::from_millis(400)).await;
    gw.send(&GatewayFrame::StopReceipt {
        task_id: play_id,
        stage: "device_confirmed".into(),
        played_ms: Some(900),

        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;
    let mode = state.voice.mode_snapshot();
    assert!(
        !mode["devices"][0]["program"]["paused"].is_null(),
        "the pause must leave a bookmark: {}",
        mode["devices"][0]["program"]
    );

    send_keyword(&gw, 4, "继续播放", false).await;
    let (resumed_play, _) = gw.until_play("voice").await;
    gw.confirm_play(&resumed_play).await;
    let log = state.voice.recent(300);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "reflex_continue" && e["resumed"] == true),
        "{log:?}"
    );
}

/// R1j/P1 regression (device 2026-11-13): the KWS resumed a paused story and
/// the SAME speech's transcript then ran the half-duplex barge-in, killing the
/// resumed audio ~600 ms later. A reflex word's transcript is its own command:
/// it must be consumed (deduped), not treated as a new request.
#[tokio::test]
async fn a_reflex_continue_survives_its_own_transcript() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm.clone()).await;

    let first = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    send_keyword(&gw, 2, "暂停播放", false).await;
    stops_within(&gw, Duration::from_millis(400)).await;
    gw.send(&GatewayFrame::StopReceipt {
        task_id: first,
        stage: "device_confirmed".into(),
        played_ms: Some(1200),

        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;

    // The spotter resumes it.
    send_keyword(&gw, 3, "继续播放", false).await;
    let (resumed, _) = gw.until_play("voice").await;
    gw.confirm_play(&resumed).await;

    // The transcript of the same words arrives inside the window.
    explicit_request(&state, &gw, 4, "继续播放。").await;
    // It must not stop the resumed play and must not start a turn.
    let stops = stops_within(&gw, Duration::from_millis(500)).await;
    assert!(
        !stops.contains(&resumed),
        "the transcript of the command must not stop the action it started: {stops:?}"
    );
    let log = state.voice.recent(400);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "intent_deduped" && e["intent"] == "continue"),
        "{log:?}"
    );
    assert_eq!(
        llm.seen_requests().len(),
        1,
        "no model turn for the command word"
    );
    // Still paused-or-playing from the program, never "program discarded".
    assert!(log
        .iter()
        .any(|e| e["kind"] == "reflex_continue" && e["via"] == "keyword"));
}

/// R1j/P1: when the spotter misses the word, the transcript executes the same
/// intent (the text layer is the fallback, not a second vocabulary).
#[tokio::test]
async fn a_reflex_word_works_from_its_transcript_alone() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;

    let first = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    // No Keyword event at all: the device never spotted it.
    explicit_request(&state, &gw, 2, "暂停播放。").await;
    let stops = stops_within(&gw, Duration::from_millis(600)).await;
    assert!(
        stops.contains(&first),
        "the transcript must pause: {stops:?}"
    );
    gw.send(&GatewayFrame::StopReceipt {
        task_id: first,
        stage: "device_confirmed".into(),
        played_ms: Some(1000),

        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;
    let log = state.voice.recent(400);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "reflex_text" && e["intent"] == "pause"),
        "the transcript path must record itself: {log:?}"
    );

    explicit_request(&state, &gw, 3, "继续播放。").await;
    let (resumed, _) = gw.until_play("voice").await;
    gw.confirm_play(&resumed).await;
    let log = state.voice.recent(400);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "reflex_text" && e["intent"] == "continue"),
        "{log:?}"
    );
}

/// R1j/P1: the card must be able to tell WHICH path a word takes. The spotter's
/// hits and the transcript executions are counted apart (the owner's question
/// 2026-11-13: "I set it in the reflex card — is it actually reflex?").
#[tokio::test]
async fn a_words_path_is_visible_in_mode() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;

    let first = start_long_voice_play(&state, &gw, 1, "讲个长故事").await;
    // Spotter path.
    send_keyword(&gw, 2, "暂停播放", false).await;
    stops_within(&gw, Duration::from_millis(400)).await;
    gw.send(&GatewayFrame::StopReceipt {
        task_id: first,
        stage: "device_confirmed".into(),
        played_ms: Some(900),

        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;
    // Transcript path for the same word, outside the dedupe window.
    tokio::time::sleep(Duration::from_millis(1700)).await;
    explicit_request(&state, &gw, 3, "继续播放。").await;
    let (resumed, _) = gw.until_play("voice").await;
    gw.confirm_play(&resumed).await;

    let mode = state.voice.mode_snapshot();
    let words = mode["devices"][0]["reflex"]["words"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let find = |t: &str| {
        words
            .iter()
            .find(|w| w["text"] == t)
            .cloned()
            .unwrap_or_default()
    };
    assert!(
        find("暂停播放")["hits"].as_u64().unwrap_or(0) >= 1,
        "spotter hits are counted"
    );
    assert_eq!(
        find("暂停播放")["text_hits"].as_u64().unwrap_or(0),
        0,
        "no transcript run for pause"
    );
    assert!(
        find("继续播放")["text_hits"].as_u64().unwrap_or(0) >= 1,
        "transcript runs are counted apart"
    );
}

/// 2026-11-13 device hang: the core closed a streamed answer (`play_end`) and
/// the device sent nothing more — no `play_finished`, no further frames at all.
/// Before this fix the output stayed `device_confirmed` forever, so
/// `activity`/LED/`session_open` all kept claiming the device was speaking.
/// The core now retires it on a bounded clock: `unknown`, out of the hub
/// registry, live program cleared (the turn was already over), event logged.
#[tokio::test]
async fn a_stream_the_device_never_finishes_is_retired_by_the_core() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["第一句。".into(), "第二句。".into()],
    }]);
    let (_base, state, gw) = spawn_mic_app_stream(llm, "你好高达，说两句").await;

    let _ = gw
        .until(|f| matches!(f, CoreFrame::MicSession { action, .. } if action == "start"))
        .await;
    let loud = loud_block();
    let quiet = quiet_block();
    for seq in 1..=10 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 11..=20 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }

    let play_id = match gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await {
        CoreFrame::PlayBegin { task_id, .. } => task_id,
        f => panic!("expected play_begin, got {f:?}"),
    };
    // Drain the stream up to the core's `play_end` (the close stamp).
    loop {
        match gw
            .until(|f| matches!(f, CoreFrame::PlayChunk { .. } | CoreFrame::PlayEnd { .. }))
            .await
        {
            CoreFrame::PlayEnd { .. } => break,
            CoreFrame::PlayChunk { .. } => continue,
            f => panic!("unexpected frame {f:?}"),
        }
    }
    // The device confirms the stream but never reports the end.
    gw.send(&GatewayFrame::PlayReceipt {
        task_id: play_id.clone(),
        stage: "device_confirmed".into(),
        detail: None,
        ts: hm_core::id::now_ms(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let snap = state.voice.mode_snapshot();
    assert_eq!(
        snap["devices"][0]["outputs"][0]["stage"], "device_confirmed",
        "precondition: the output is live: {snap:?}"
    );
    assert_eq!(snap["devices"][0]["activity"], "speaking");

    // Inside the bound nothing is retired (a normal long answer must survive).
    let now = hm_core::id::now_ms();
    assert!(
        state.voice.play_stall_sweep_at(now + 9_000).is_empty(),
        "9 s after play_end is not a stall"
    );
    let retired = state.voice.play_stall_sweep_at(now + 11_000);
    assert_eq!(retired, vec![("dot2-d".to_string(), play_id.clone())]);

    let snap = state.voice.mode_snapshot();
    assert_eq!(
        snap["devices"][0]["outputs"][0]["stage"], "unknown",
        "a stalled output is marked unknown, not silently dropped: {snap:?}"
    );
    assert_ne!(
        snap["devices"][0]["activity"], "speaking",
        "the core stops speaking: {snap:?}"
    );
    assert!(
        state.hub.active_plays_for("dot2-d").is_empty(),
        "hub registry cleared"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "play_stalled" && e["play_id"] == play_id),
        "play_stalled carries the play id: {log:?}"
    );
    assert!(
        snap["devices"][0]["program"]["live"].is_null(),
        "the finished turn's live program is cleared: {snap:?}"
    );

    // The terminal record ages out; `/mode.outputs` converges on empty and the
    // hub has nothing pending, so the next turn is not blocked.
    state.voice.stale_output_sweep_at(now + 17_000);
    let snap = state.voice.mode_snapshot();
    assert!(
        snap["devices"][0]["outputs"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true),
        "outputs empty after the stale sweep: {snap:?}"
    );
    assert_eq!(state.hub.snapshot()["pending_plays"], 0);
}

/// The R1i observation: a stop left `stage=stopping` records in `/mode` long
/// after `activity=idle`, because removal depended on a receipt the device no
/// longer owed. A stop now keeps only records the hub actually submitted a
/// Stop for, and those age out; a local stop removes them immediately.
#[tokio::test]
async fn a_stop_leaves_no_stopping_records_behind() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["第一句。".into(), "第二句。".into()],
    }]);
    let (_base, state, gw) = spawn_mic_app_stream(llm, "你好高达，说两句").await;

    let _ = gw
        .until(|f| matches!(f, CoreFrame::MicSession { action, .. } if action == "start"))
        .await;
    let loud = loud_block();
    let quiet = quiet_block();
    for seq in 1..=10 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 11..=20 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }
    let play_id = match gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await {
        CoreFrame::PlayBegin { task_id, .. } => task_id,
        f => panic!("expected play_begin, got {f:?}"),
    };
    gw.confirm_play(&play_id).await;

    // A named stop: the play stays registered until its stop receipt, then
    // the record must not outlive the receipt either.
    let _ = state.voice.stop_device("dot2-d");
    let stopped = stops_within(&gw, Duration::from_millis(600)).await;
    for id in &stopped {
        gw.send(&GatewayFrame::StopReceipt {
            task_id: id.clone(),
            stage: "device_confirmed".into(),
            played_ms: Some(0),

            consumed_ms: None,
            progress_basis: None,
            detail: None,
        })
        .await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snap = state.voice.mode_snapshot();
    assert!(
        snap["devices"][0]["outputs"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true),
        "the stop receipt retires the record: {snap:?}"
    );
}

/// The same leak on the local-stop path: the device cut the channel on its
/// own reflex and no receipt will ever arrive for it.
#[tokio::test]
async fn a_local_stop_leaves_no_stopping_records_behind() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["第一句。".into(), "第二句。".into()],
    }]);
    let (_base, state, gw) = spawn_mic_app_stream(llm, "你好高达，说两句").await;

    let _ = gw
        .until(|f| matches!(f, CoreFrame::MicSession { action, .. } if action == "start"))
        .await;
    let loud = loud_block();
    let quiet = quiet_block();
    for seq in 1..=10 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 11..=20 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }
    let play_id = match gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await {
        CoreFrame::PlayBegin { task_id, .. } => task_id,
        f => panic!("expected play_begin, got {f:?}"),
    };
    gw.confirm_play(&play_id).await;

    let _ = state.voice.stop_device_locally("dot2-d");
    let snap = state.voice.mode_snapshot();
    assert!(
        snap["devices"][0]["outputs"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true),
        "a local stop leaves no stopping record: {snap:?}"
    );
    assert!(state.hub.active_plays_for("dot2-d").is_empty());
}

/// R2 (2026-11-13): the cue is not gated on a single turn-level timer any
/// more. The moment the model DECIDES to call a tool, the wait is real, so the
/// cue may speak `tool_delay_ms` later (RUI's tools_pending + 0.6 s); the
/// 1.8–5 s turn cap only covers turns with no tool decision at all.
#[tokio::test]
async fn a_tool_decision_pulls_the_waiting_cue_forward() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("time.now".into(), "{}".into())],
        },
        // The second round must be slow enough that the cue has somewhere to
        // land; the trigger is what is under test, not the answer's speed.
        Scripted::Delayed {
            delay_ms: 900,
            chunks: vec!["现在是十点。".into()],
        },
    ]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    // The turn cap is far away (5 s): only the tool trigger can explain a cue
    // within the first two seconds.
    let mut filler = one_cue(5_000, "我想想。");
    filler.tool_delay_ms = 100;
    set_filler(&state, filler).await;

    let started = std::time::Instant::now();
    explicit_request(&state, &gw, 1, "随便说点什么").await;
    let _cue = gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("filler:")
            )
        })
        .await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(2_500),
        "the cue should follow the tool decision, not the 5000 ms cap: {elapsed:?}"
    );
    assert_eq!(
        voice_log_has(&state, "filler_tool_armed").len(),
        1,
        "the tool decision is recorded as the cue's trigger"
    );
}

/// R2 (2026-11-13, field report "唤醒应答词消失了"): the ASR transcript IS the
/// wake confirmation the interaction matrix reserved the ack for. Both ASR
/// command paths (wake word at the head of a fresh utterance, and an in-window
/// transcript that still carries it) must say the cached ack, let it finish,
/// and then run the command — with no waiting cue on top (the ack covers it).
#[tokio::test]
async fn a_wake_word_with_a_command_does_not_say_the_ack_first() {
    // 2026-11-13 (user): 「你好高达，今天周几」 said 「来了」 before the answer,
    // which sounds wrong. The ack belongs to the BARE wake; a command in the
    // same breath is answered directly (a slow turn is covered by the cue).
    let llm = FakeLlm::new(vec![Scripted::Delayed {
        delay_ms: 300,
        chunks: vec!["好，我查一下今天的新闻。".into()],
    }]);
    let (base, state, gw) = spawn_mic_app_stream_asr(
        llm,
        Arc::new(FakeAsr {
            reply: "你好高达，帮我播新闻".into(),
        }),
    )
    .await;
    let _ = base;
    // Enable the ack and build its cache: if the code plays it here, the
    // first frame below is an `ack:` play and the test fails.
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
            ..Default::default()
        }),
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
        "test",
    );
    state.voice.synthesize_wake_acks(false).await;

    let _ = gw
        .until(|f| matches!(f, CoreFrame::MicSession { action, .. } if action == "start"))
        .await;
    for seq in 1..=10 {
        gw.send(&audio_chunk(seq, &loud_block())).await;
    }
    for seq in 11..=20 {
        gw.send(&audio_chunk(seq, &quiet_block())).await;
    }

    // The FIRST audio submission must be the answer, never an ack.
    match gw
        .until(|f| matches!(f, CoreFrame::Play { .. } | CoreFrame::PlayBegin { .. }))
        .await
    {
        CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
            if content_ref.starts_with("ack:") =>
        {
            panic!("a command in the same breath must not be preceded by the ack");
        }
        CoreFrame::PlayBegin { .. } | CoreFrame::Play { .. } => {}
        f => panic!("unexpected frame {f:?}"),
    }
    let log = state.voice.recent(200);
    assert!(
        !log.iter().any(|e| e["kind"] == "wake_ack_settled"),
        "no ack may be spoken on a wake-with-command turn: {log:?}"
    );
}

/// 退下 clears the session, and a resume that was in flight must not undo
/// that: the interrupted path used to recreate a bookmark from the resume's
/// own cursor, so after a plain stop or a retreat the ring went back to amber
/// and 「继续」 could replay a program the person had just ended (codex
/// whole-interaction review, 2026-11-13).
#[tokio::test]
async fn a_stop_during_a_resume_does_not_recreate_the_bookmark() {
    let answer = "第一段很短。第二段接着说。第三段从这里开始。第四段收尾。";
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![answer.into()],
    }]);
    let tts = Arc::new(SlowTts {
        delay_ms: 250,
        ..Default::default()
    });
    let (_base, state, gw) = spawn_mic_app_stream_tts(llm, "unused", tts).await;
    low_backstop(&state, 2);

    explicit_request(&state, &gw, 1, "今天有什么新闻").await;
    let first = streamed_answer(&gw, &RecordingTts::default()).await;
    assert!(
        !first.contains("第三段"),
        "setup: the cap cut the answer: {first}"
    );
    let paused = state.voice.mode_snapshot()["devices"][0]["program"]["paused"].clone();
    assert!(!paused.is_null(), "the cut must book the rest: {paused}");

    // 继续 takes the bookmark and starts reading… the slow synthesis is the
    // window in which the stop lands.
    explicit_request(&state, &gw, 2, "继续").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    state.voice.stop_device("dot2-d");
    tokio::time::sleep(Duration::from_millis(700)).await;

    let after = state.voice.mode_snapshot()["devices"][0]["program"].clone();
    assert!(
        after["paused"].is_null(),
        "a stopped resume must not leave a bookmark behind: {after}"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "program_interrupted" && e["bookmark_kept"] == false),
        "the interruption must record that nothing was kept: {log:?}"
    );
}

/// R2 (2026-11-13): the spoken-delivery policy is edited through the SAME
/// mode endpoint the console already uses (its own history row), validated
/// there, and read back from the snapshot the card renders. The prompt and
/// the runtime cap read this one block, so a save can never drift from what
/// the device does.
#[tokio::test]
async fn the_spoken_delivery_policy_is_saved_validated_and_read_back() {
    let llm = FakeLlm::new(vec![]);
    let (base, _state, _gw) = spawn_mic_app(llm, "你好高达，退下").await;
    let client = reqwest::Client::new();

    // Defaults are visible (2 sentences / 120 chars, long-content words).
    let mode: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mode["spoken"]["max_sentences"], json!(2));
    assert_eq!(mode["spoken"]["max_chars"], json!(120));
    assert_eq!(mode["spoken"]["detail_max_sentences"], json!(6));
    assert_eq!(mode["spoken"]["detail_max_chars"], json!(300));
    // The runaway backstop is visible and sits far above every soft target.
    assert_eq!(mode["spoken"]["backstop_max_chars"], json!(2000));
    assert!(mode["spoken"]["backstop_max_sentences"].as_u64().unwrap() > 6);

    // Out-of-range values are refused, not clamped silently.
    for bad in [
        json!({ "max_sentences": 0, "max_chars": 120, "long_form_words": ["讲个"], "continue_hint": "" }),
        json!({ "max_sentences": 2, "max_chars": 10, "long_form_words": ["讲个"], "continue_hint": "" }),
        // A backstop no wider than a soft target would cut ordinary answers.
        json!({ "max_sentences": 2, "max_chars": 120, "backstop_max_sentences": 6, "backstop_max_chars": 300 }),
        json!({ "max_sentences": 2, "max_chars": 120, "backstop_max_chars": 100 }),
        // 45 chars: over the 40-char ceiling.
        json!({ "max_sentences": 2, "max_chars": 120, "long_form_words": ["讲个"], "continue_hint": "长".repeat(45) }),
        // A detail budget outside the same bounds is refused too.
        json!({ "max_sentences": 2, "max_chars": 120, "detail_max_sentences": 0,
                "long_form_words": ["讲个"], "continue_hint": "" }),
    ] {
        let r = client
            .put(format!("{base}/api/v0/mode/dot2-d"))
            .json(&json!({ "spoken": bad }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            400,
            "invalid spoken policy {bad} must be rejected"
        );
    }

    // A valid edit persists and is recorded on the voice history chain.
    let r = client
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({
            "spoken": {
                "max_sentences": 3,
                "max_chars": 180,
                "detail_max_sentences": 5,
                "detail_max_chars": 260,
                "detail_words": ["讲讲", "说说"],
                "long_form_words": ["讲个", "念"],
                "continue_hint": "还有，说继续。"
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mode: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mode["spoken"]["max_sentences"], json!(3));
    assert_eq!(mode["spoken"]["max_chars"], json!(180));
    assert_eq!(mode["spoken"]["detail_max_sentences"], json!(5));
    assert_eq!(mode["spoken"]["detail_max_chars"], json!(260));
    assert_eq!(mode["spoken"]["detail_words"], json!(["讲讲", "说说"]));
    assert_eq!(mode["spoken"]["continue_hint"], json!("还有，说继续。"));
    let hist: Value = reqwest::get(format!("{base}/api/v0/config/history"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        hist["history"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["scope"] == "voice"
                && e["summary"]
                    .as_str()
                    .unwrap_or("")
                    .contains("spoken short=3/180")),
        "the change must be in the voice history: {hist:?}"
    );
}

/// A runaway answer is cut at the backstop, the rest stays in the session,
/// and the person is told how to hear it. (R2, 2026-11-13: this used to be
/// the ordinary two-sentence cap; since 2026-11-14 the prompt carries that
/// target and the runtime only stops what ran past the backstop.)
#[tokio::test]
async fn a_runaway_answer_is_cut_at_the_backstop_and_offers_continuing() {
    let answer = "第一句在这里。第二句接着说。第三句不该被听见。第四句也不该。第五句同样。";
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![answer.into()],
    }]);
    let tts = Arc::new(RecordingTts::default());
    let (_base, state, gw) = spawn_mic_app_stream_tts(llm, "unused", tts.clone()).await;
    low_backstop(&state, 2);

    explicit_request(&state, &gw, 1, "今天有什么新闻").await;
    let spoken = streamed_answer(&gw, &tts).await;
    assert!(spoken.contains("第一句"), "the answer must start: {spoken}");
    assert!(
        spoken.contains("第二句"),
        "two sentences fit the backstop: {spoken}"
    );
    assert!(
        !spoken.contains("第三句"),
        "the third sentence is past the backstop: {spoken}"
    );
    assert!(
        spoken.contains("说继续"),
        "the person must be told how to hear the rest: {spoken}"
    );

    let log = state.voice.recent(200);
    assert!(
        log.iter().any(|e| e["kind"] == "spoken_budget_cut"),
        "the cut must be recorded with its numbers: {log:?}"
    );
    assert!(
        log.iter().any(|e| e["kind"] == "spoken_continue_hint") && e_hint(&log),
        "the continuation clue must be its own record: {log:?}"
    );
    // The FULL answer is what the conversation holds — the cap costs speech,
    // never content.
    let hist = state.sessions.messages("dev:dot2-d").unwrap();
    assert!(
        hist.iter()
            .any(|m| m["role"] == "assistant" && m["content"] == answer),
        "the session must keep the whole answer: {hist:?}"
    );
}

/// 「继续」 reads on from exactly where the cap stopped — not from the start
/// of the answer (that would replay what the person just heard).
#[tokio::test]
async fn continuing_reads_on_from_the_cut_point() {
    let answer = "第一句在这里。第二句接着说。第三句从这里开始。第四句收尾。";
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![answer.into()],
    }]);
    let tts = Arc::new(RecordingTts::default());
    let (_base, state, gw) = spawn_mic_app_stream_tts(llm, "unused", tts.clone()).await;
    low_backstop(&state, 2);

    explicit_request(&state, &gw, 1, "今天有什么新闻").await;
    let first = streamed_answer(&gw, &tts).await;
    assert!(
        !first.contains("第三句"),
        "setup: the cap must have cut it: {first}"
    );

    explicit_request(&state, &gw, 2, "继续").await;
    let (_, meta) = gw.until_play("voice").await;
    let resumed = meta["text"].as_str().unwrap_or("").to_string();
    assert!(
        resumed.contains("第三句"),
        "the continuation must start where the person stopped: {resumed}"
    );
    assert!(
        !resumed.contains("第一句"),
        "the continuation must not replay the beginning: {resumed}"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter().any(|e| e["kind"] == "program_resumed"),
        "the resume must be recorded: {log:?}"
    );
}

/// A five-sentence answer is NOT cut: how much to say is the model's call
/// (the prompt carries the tiers), and the runtime only stops a runaway.
/// The transcript is deliberately one the old keyword lists matched — the
/// behaviour must no longer depend on it.
#[tokio::test]
async fn a_five_sentence_answer_is_the_models_call_and_is_not_cut() {
    let answer = "第一句背景。第二句现状。第三句原因。第四句影响。第五句结论。";
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![answer.into()],
    }]);
    let tts = Arc::new(RecordingTts::default());
    let (_base, state, gw) = spawn_mic_app_stream_tts(llm, "unused", tts.clone()).await;

    explicit_request(&state, &gw, 1, "给我讲讲英伟达最近咋了").await;
    let spoken = streamed_answer(&gw, &tts).await;
    for s in ["第一句", "第二句", "第三句", "第四句", "第五句"] {
        assert!(
            spoken.contains(s),
            "a five-sentence answer must not stop at two sentences ({s} missing): {spoken}"
        );
    }
    let log = state.voice.recent(200);
    assert!(
        !log.iter()
            .any(|e| e["kind"] == "spoken_detail" || e["kind"] == "spoken_long_form"),
        "the runtime no longer classifies the transcript: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "spoken_budget_cut"),
        "five sentences are far below the backstop: {log:?}"
    );
}

/// The 2026-11-14 device case, end to end: ASR dropped 「讲」, the person's
/// words were 「一个很长的故事」 and NO keyword list could match them. The old
/// classifier read it as an ordinary turn and cut the story at two sentences,
/// then again at every 「继续」. Nothing in the transcript decides now.
#[tokio::test]
async fn a_story_is_read_to_the_end_whatever_the_transcript_looks_like() {
    let answer = "第一句开场。第二句铺垫。第三句转折。第四句高潮。第五句收尾。";
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![answer.into()],
    }]);
    let tts = Arc::new(RecordingTts::default());
    let (_base, state, gw) = spawn_mic_app_stream_tts(llm, "unused", tts.clone()).await;

    explicit_request(&state, &gw, 1, "一个很长的故事").await;
    let spoken = streamed_answer(&gw, &tts).await;
    for s in ["第一句", "第二句", "第三句", "第四句", "第五句"] {
        assert!(
            spoken.contains(s),
            "the story must run to its end ({s} missing): {spoken}"
        );
    }
    let log = state.voice.recent(200);
    assert!(
        !log.iter()
            .any(|e| e["kind"] == "spoken_budget_cut" || e["kind"] == "spoken_continue_hint"),
        "no cut and no 「说继续」 on a story: {log:?}"
    );
}

/// The person's actual complaint (2026-11-13): a weather lookup that could
/// not connect produced a long explanation. A turn whose tools ALL failed
/// speaks ONE core-owned sentence; the model's prose is dropped, not spoken.
#[tokio::test]
async fn a_failed_query_speaks_one_core_line_instead_of_model_prose() {
    let prose = "抱歉，我现在无法获取天气信息。这可能是因为网络连接问题，也可能是服务暂时不可用。\
                 您可以稍后再试，或者在设置里检查天气服务是否已经配置好地点和密钥。\
                 如果问题持续存在，建议检查设备的网络连接。";
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("weather.now".into(), r#"{}"#.into())],
        },
        Scripted::Text {
            chunks: vec![prose.into()],
        },
    ]);
    let tts = Arc::new(RecordingTts::default());
    let (base, state, gw) = spawn_mic_app_stream_tts(llm, "unused", tts.clone()).await;

    explicit_request(&state, &gw, 1, "今天天气怎么样").await;
    let spoken = streamed_answer(&gw, &tts).await;
    assert!(
        spoken.contains("天气服务") && spoken.contains("配置"),
        "the failure must be the core's own short line: {spoken}"
    );
    assert!(
        !spoken.contains("稍后再试") && !spoken.contains("建议检查"),
        "the model's explanation must not be read out: {spoken}"
    );
    assert!(
        spoken.chars().count() <= 40,
        "a failed lookup is one short sentence, got {} chars: {spoken}",
        spoken.chars().count()
    );
    assert!(
        !spoken.contains("。".to_string().as_str()) || spoken.matches('。').count() <= 1,
        "one sentence only: {spoken}"
    );
    let traces: Value = reqwest::get(format!("{base}/api/v0/trace?limit=5"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let trace_id = traces["traces"][0]["id"].as_str().expect("a trace id");
    let detail: Value = reqwest::get(format!("{base}/api/v0/trace/{trace_id}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rendered = detail["events"]
        .as_array()
        .and_then(|evs| evs.iter().find(|e| e["kind"] == "answer_rendered"))
        .expect("the turn's trace must carry answer_rendered");
    assert_eq!(
        rendered["payload"]["source"],
        json!("tool_failure"),
        "the core-owned failure answer must be recorded: {rendered:?}"
    );
}

/// M7 residual (2026-11-13): a search that finds NOTHING is an answer, not a
/// reason to explain. `music.search` returns `observed_ok{no_matches}`, so
/// the old "all tools failed" short-circuit never fired and a "搜一下" request
/// (Detail tier, six sentences) produced a paragraph about why the song is
/// missing. Now an empty result counts as a failed delivery: one core line,
/// the model's prose dropped.
#[tokio::test]
async fn a_no_match_search_speaks_one_line_and_drops_the_model_prose() {
    let prose = "我在曲库里帮您找了一下，不过很抱歉，这首《不存在的歌》暂时没有找到。\
                 您可以换一个歌名再试试，或者让我用在线音乐源搜索，不过在线搜索会消耗配额。\
                 也可以到控制台里添加曲库，或者检查一下曲库服务是否已经配置好。\
                 如果问题一直存在，建议您稍后再试。";
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("music.search".into(), r#"{"query":"不存在的歌"}"#.into())],
        },
        Scripted::Text {
            chunks: vec![prose.into()],
        },
    ]);
    let tts = Arc::new(RecordingTts::default());
    let (base, _state, gw, _music) =
        spawn_mic_app_stream_tts_music(llm, "unused", tts.clone()).await;

    // "搜一下" is in detail_words: before the fix this made the turn Detail
    // (6 sentences / 300 chars) even though the search found nothing.
    explicit_request(&_state, &gw, 1, "帮我搜一下不存在的歌").await;
    let spoken = streamed_answer(&gw, &tts).await;
    assert!(
        spoken.contains("没找到这首歌"),
        "the no-match answer must be the core's own line: {spoken}"
    );
    assert!(
        !spoken.contains("配额") && !spoken.contains("控制台") && !spoken.contains("曲库"),
        "the model's explanation must not be read out: {spoken}"
    );
    assert!(
        spoken.chars().count() <= 40,
        "one short line, got {} chars: {spoken}",
        spoken.chars().count()
    );
    let traces: Value = reqwest::get(format!("{base}/api/v0/trace?limit=5"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let trace_id = traces["traces"][0]["id"].as_str().expect("a trace id");
    let detail: Value = reqwest::get(format!("{base}/api/v0/trace/{trace_id}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rendered = detail["events"]
        .as_array()
        .and_then(|evs| evs.iter().find(|e| e["kind"] == "answer_rendered"))
        .expect("the turn's trace must carry answer_rendered");
    assert_eq!(
        rendered["payload"]["source"],
        json!("tool_failure"),
        "an empty result is delivered like a failure: {rendered:?}"
    );
    assert!(
        rendered["payload"]["model_chars_dropped"]
            .as_u64()
            .unwrap_or(0)
            >= (prose.chars().count() as u64) / 2,
        "the dropped model prose is accounted for: {rendered:?}"
    );
}

/// A request for long content is NOT cut: the reading runs to the end. (The
/// shape used to come from a configurable word list; since 2026-11-14 the
/// model chooses it and the runtime keeps only the runaway backstop.)
#[tokio::test]
async fn a_long_content_request_is_not_cut_by_the_spoken_budget() {
    let answer = "第一句开场。第二句铺垫。第三句转折。第四句高潮。第五句收尾。";
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![answer.into()],
    }]);
    let tts = Arc::new(RecordingTts::default());
    let (_base, state, gw) = spawn_mic_app_stream_tts(llm, "unused", tts.clone()).await;

    explicit_request(&state, &gw, 1, "讲个故事").await;
    let spoken = streamed_answer(&gw, &tts).await;
    for s in ["第一句", "第二句", "第三句", "第四句", "第五句"] {
        assert!(
            spoken.contains(s),
            "a long-content request reads to the end ({s} missing): {spoken}"
        );
    }
    let log = state.voice.recent(200);
    assert!(
        !log.iter().any(|e| e["kind"] == "spoken_budget_cut"),
        "nothing may be cut on a long-content turn: {log:?}"
    );
}

/// The spotter opens the window, then the ASR delivers the SAME breath as
/// words. The wake phrase alone is the person's entire input — the matrix
/// reserved the ack for this confirmation, and returning silently was the
/// "wake word does nothing" report (codex interaction review B-2).
#[tokio::test]
async fn a_wake_only_transcript_after_a_keyword_hit_says_the_ack() {
    let llm = FakeLlm::new(vec![]);
    let (base, state, gw) = spawn_mic_app_stream_asr(
        llm,
        Arc::new(FakeAsr {
            reply: "你好，高达".into(),
        }),
    )
    .await;
    let _ = base;
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
            ..Default::default()
        }),
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
        "test",
    );
    state.voice.synthesize_wake_acks(false).await;

    let _ = gw
        .until(|f| matches!(f, CoreFrame::MicSession { action, .. } if action == "start"))
        .await;
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
    })
    .await;
    for seq in 2..=11 {
        gw.send(&audio_chunk(seq, &loud_block())).await;
    }
    for seq in 12..=21 {
        gw.send(&audio_chunk(seq, &quiet_block())).await;
    }

    let ack = gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("ack:")
            )
        })
        .await;
    if let CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } = ack {
        gw.send(&GatewayFrame::PlayReceipt {
            task_id: task_id.clone(),
            stage: "device_confirmed".into(),
            detail: None,
            ts: hm_core::id::now_ms(),
        })
        .await;
        gw.send(&GatewayFrame::PlayFinished {
            device: "dot2-d".into(),
            task_id,
            played_ms: 700,
            ts: hm_core::id::now_ms(),

            consumed_ms: None,
            progress_basis: None,
        })
        .await;
    }
    // The ack's outcome note lands when its wait finishes, not at submission.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "wake_ack_after_keyword_window"),
        "the ack of a bare wake must name its trigger: {log:?}"
    );
    assert!(
        log.iter()
            .any(|e| e["kind"] == "wake_ack_settled" && e["outcome"] == "finished"),
        "the ack must be classified from the device's own finish: {log:?}"
    );
}

/// Phase 5 (#45): KWS keyword + ASR transcript of the same sentence executes once,
/// and speaking a retreat word during active playback stops playback cleanly.
#[tokio::test]
async fn kws_and_asr_unified_admission_executes_once_and_retreat_stops_playback() {
    let story: Vec<String> = (1..=6)
        .map(|i| format!("第{i}段故事内容，讲的是山谷里的风。"))
        .collect();
    let llm = FakeLlm::new(vec![Scripted::TextPaced {
        chunks: story,
        delay_ms: 120,
    }]);
    let (asr, _reply) = SwitchableAsr::new("你好高达，讲个故事");
    let (_base, state, gw) = spawn_mic_app_asr(llm.clone(), Arc::new(asr)).await;

    // 1. KWS wake + ASR transcript of the same sentence ("你好高达，讲个故事").
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
    })
    .await;
    let loud = loud_block();
    let quiet = quiet_block();
    for seq in 1..=10 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 11..=20 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }

    let play_id = match gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("tts:")
            )
        })
        .await
    {
        CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } => task_id,
        _ => unreachable!(),
    };
    assert_eq!(
        llm.seen_requests().len(),
        1,
        "KWS wake + ASR transcript of the same sentence must execute the turn exactly once"
    );

    // 2. Speaking a retreat word during active playback stops playback cleanly.
    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 21,
        keyword: "停止播放".into(),
        fed_ms: None,
        self_heard: false,
        rms_dbfs: None,
        lag_ms: None,
        bundle_id: None,
        kws_generation: None,
    })
    .await;
    let stop = gw
        .until(|f| matches!(f, CoreFrame::Stop { task_id, .. } if task_id == &play_id))
        .await;
    assert!(matches!(stop, CoreFrame::Stop { flush: true, .. }));
    gw.confirm_stop(&play_id).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if state
            .voice
            .recent(200)
            .iter()
            .any(|e| e["kind"] == "session_retreat" || e["kind"] == "reflex_retreat")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "session_retreat" || e["kind"] == "reflex_retreat"),
        "retreat during playback must close the session: {log:?}"
    );
    assert_eq!(
        state.voice.mode_snapshot()["devices"][0]["session"]["state"],
        "idle"
    );
}
