use super::*;

/// R1e: a bare 停 OUTSIDE a barge-in is still a DISCARD. (Right after a
/// barge the guard has eaten the first syllable, so 「暂停」 and 「停」 are the
/// same transcript — that case keeps the bookmark and is pinned by
/// `a_bare_stop_after_a_barge_keeps_the_bookmark`.)
#[tokio::test]
async fn a_plain_stop_still_discards_the_bookmark() {
    let story: Vec<String> = (1..=8)
        .map(|i| format!("第{i}段故事内容，讲的是山谷里的风。"))
        .collect();
    let llm = FakeLlm::new(vec![Scripted::TextPaced {
        chunks: story,
        delay_ms: 40,
    }]);
    let (_base, state, gw) = spawn_mic_app_stream(llm, "").await;

    explicit_request(&state, &gw, 1, "讲个长故事").await;
    gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    explicit_request(&state, &gw, 2, "停").await;
    let stopped = stops_within(&gw, Duration::from_millis(800)).await;
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
    tokio::time::sleep(Duration::from_millis(300)).await;

    let log = state.voice.recent(400);
    assert!(
        log.iter().any(|e| e["kind"] == "stop_intent"),
        "停 must be a stop: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "stop_intent_after_barge"),
        "no barge-in here: the plain stop rule applies: {log:?}"
    );
    let snap = state.voice.mode_snapshot();
    assert!(
        snap["devices"][0]["program"]["paused"].is_null(),
        "a stop discards the bookmark: {snap:?}"
    );
}

/// R1e: right after an accepted barge-in the guard has eaten the first
/// syllable, so a bare 「停」 transcript IS 「暂停」 as far as the system can
/// tell. The audio already stopped at the barge; the recoverable choice is to
/// keep the bookmark (losing a 30 s story is the worse error).
#[tokio::test]
async fn a_bare_stop_after_a_barge_keeps_the_bookmark() {
    let story: Vec<String> = (1..=8)
        .map(|i| format!("第{i}段故事内容，讲的是山谷里的风。"))
        .collect();
    let llm = FakeLlm::new(vec![Scripted::Text { chunks: story }]);
    let (_base, state, gw) = spawn_mic_app(llm, "停。").await;
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
    let loud = loud_block();
    for seq in 3..=5 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 6..=13 {
        gw.send(&audio_chunk(seq, &quiet_block())).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;

    let log = state.voice.recent(400);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "stop_intent_after_barge" && e["paused"] == true),
        "a bare stop right after a barge keeps the bookmark: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "stop_intent"),
        "it must not be handled as a plain discard: {log:?}"
    );
    let snap = state.voice.mode_snapshot();
    assert!(
        !snap["devices"][0]["program"]["paused"].is_null(),
        "the bookmark survives: {snap:?}"
    );
}

/// R1: our own stopped answer never becomes words. A barge episode whose loud
/// audio never leaves the guard produces NO segment and NO turn — the guard's
/// job is the property, not its 250 ms.
#[tokio::test]
async fn a_barge_episode_with_no_audio_behind_it_is_dropped() {
    let long_answer = "这是".to_string() + &"很长".repeat(400);
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![long_answer],
    }]);
    let (_base, state, gw) = spawn_mic_app(llm, "别讲了").await;
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

    // The VAD opens on the held head (our own tail, which is what the device
    // certified as "someone is talking over us") and then the room is quiet:
    // nothing loud arrives after the guard, so the segment is our own answer.
    for seq in 3..=14 {
        gw.send(&audio_chunk(seq, &quiet_block())).await;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;

    let log = state.voice.recent(400);
    assert!(
        !log.iter().any(|e| e["kind"] == "mic_utterance"),
        "our own stopped answer must never become an utterance: {log:?}"
    );
    assert!(
        log.iter()
            .any(|e| e["kind"] == "barge_in_tail_only_segment_dropped"),
        "the drop is counted with its reason: {log:?}"
    );
}

/// R1e acceptance, the whole story: 讲长故事 → 暂停 → 问时间 → 继续.
///
/// 暂停 freezes the program at a sentence boundary; an interjected question
/// is answered WITHOUT resuming it (RUI-local: 聊天插问不自动恢复), and only
/// the explicit 继续 replays it — from the bookmarked sentence, not from the
/// top. Driven on the text path (the same intent entry as speech); the guard
/// that used to eat the first syllable is covered by
/// `a_pause_spoken_over_the_answer_bookmarks_the_program`.
#[tokio::test]
async fn pause_then_interject_then_continue_resumes_at_the_sentence_boundary() {
    let sentences: Vec<String> = (1..=6)
        .map(|i| format!("第{i}段故事，讲的是山谷里的风。"))
        .collect();
    let llm = FakeLlm::new(vec![
        Scripted::TextPaced {
            chunks: sentences.clone(),
            delay_ms: 40,
        },
        Scripted::Text {
            chunks: vec!["外面没下雨。".into()],
        },
    ]);
    let (base, state, gw) = spawn_mic_app_stream(llm.clone(), "").await;
    let _ = base;

    explicit_request(&state, &gw, 1, "讲个长故事").await;
    gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // --- 暂停 -------------------------------------------------------------
    explicit_request(&state, &gw, 2, "暂停").await;
    // The pause stops the live audio; acknowledge so the play is closed.
    let stopped = stops_within(&gw, Duration::from_millis(800)).await;
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
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = state.voice.recent(400);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "pause_intent" && e["paused"] == true),
        "{log:?}"
    );
    let bookmark = state
        .voice
        .paused_bookmark("dot2-d")
        .expect("a paused program must leave a bookmark");
    let cursor = bookmark["cursor"].as_u64().unwrap_or(0);
    assert!(
        cursor > 0,
        "the bookmark is past the first sentence: {bookmark:?}"
    );
    let expect_next: String = bookmark["next"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .take(8)
        .collect();

    // --- 插问一句 (it must NOT resume the story) ---------------------------
    let calls_before = llm.seen_requests().len();
    explicit_request(&state, &gw, 3, "外面下雨吗？").await;
    let (play2, text2) = gw.until_any_voice_play().await;
    assert!(
        text2.contains("没下雨"),
        "the question is answered: {text2}"
    );
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: play2,
        played_ms: 1_200,
        ts: hm_core::id::now_ms(),

        consumed_ms: None,
        progress_basis: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        llm.seen_requests().len(),
        calls_before + 1,
        "the interjection is one model turn"
    );
    let log = state.voice.recent(400);
    assert!(
        !log.iter().any(|e| e["kind"] == "program_resumed"),
        "an interjection must not resume: {log:?}"
    );
    assert!(
        state.voice.paused_bookmark("dot2-d").is_some(),
        "the bookmark survives the interjection"
    );

    // --- 继续 (from the bookmark, no model turn) ---------------------------
    let calls_before = llm.seen_requests().len();
    explicit_request(&state, &gw, 4, "继续").await;
    let (play3, meta3) = gw.until_play("voice").await;
    gw.confirm_play(&play3).await;
    let said = meta3["text"].as_str().unwrap_or("").to_string();
    assert!(
        said.contains(&expect_next),
        "resume starts at the bookmarked sentence (expected '{expect_next}'), got '{said}'"
    );
    assert!(
        !said.contains("第1段故事"),
        "a resume must not restart the story: '{said}'"
    );
    assert_eq!(
        llm.seen_requests().len(),
        calls_before,
        "继续 is local: no model turn"
    );
}

/// RH-1: a program interrupted WHILE THE MODEL IS STILL WRITING cannot be
/// finished from its bookmark — the rest never existed. 继续 reads the prefix
/// that does exist and then SAYS it was truncated (the configured phrase, as
/// the program's last sentence on the voice channel), and never asks the model
/// for more (user decision 2026-11-12: label, do not regenerate).
#[tokio::test]
async fn a_program_cut_mid_generation_ends_with_the_truncation_notice() {
    let sentences: Vec<String> = (1..=8)
        .map(|i| format!("第{i}段故事，讲的是山谷里的风。"))
        .collect();
    // 400 ms per chunk: the pause below lands with the story still being
    // generated, which is exactly the state the bookmark must remember.
    let llm = FakeLlm::new(vec![Scripted::TextPaced {
        chunks: sentences,
        delay_ms: 400,
    }]);
    let (base, state, gw) = spawn_mic_app_stream(llm.clone(), "").await;
    let _ = base;
    let notice = "故事断了，后面没有再写。";
    set_truncation_notice(&state, Some(notice)).await;

    explicit_request(&state, &gw, 1, "讲个长故事").await;
    // R2 (2026-11-13): the cue now speaks BEFORE the answer and the answer
    // waits for it to finish (bounded). A real device sends play_finished for
    // the cue; this fake must too, or the answer sits out the 1.5 s bound.
    let cue = gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("filler:")
            )
        })
        .await;
    if let CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } = cue {
        gw.send(&GatewayFrame::PlayFinished {
            device: "dot2-d".into(),
            task_id,
            played_ms: 775,
            ts: hm_core::id::now_ms(),

            consumed_ms: None,
            progress_basis: None,
        })
        .await;
    }
    gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    explicit_request(&state, &gw, 2, "暂停").await;
    let stopped = stops_within(&gw, Duration::from_millis(800)).await;
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
    tokio::time::sleep(Duration::from_millis(300)).await;
    let bookmark = state
        .voice
        .paused_bookmark("dot2-d")
        .expect("a paused program must leave a bookmark");
    assert_eq!(
        bookmark["complete"], false,
        "generation had not finished, so the bookmark is explicitly incomplete: {bookmark:?}"
    );

    // --- 继续: read what exists, then label the truncation ----------------
    let calls_before = llm.seen_requests().len();
    explicit_request(&state, &gw, 3, "继续").await;
    let plays = play_program_through(&gw, Duration::from_millis(400)).await;
    let voice: Vec<(String, Value)> = plays
        .into_iter()
        .filter(|(_, m)| m["channel"] == "voice")
        .collect();
    assert!(!voice.is_empty(), "继续 must speak the remaining prefix");
    let texts: Vec<String> = voice
        .iter()
        .map(|(_, m)| m["text"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("第2段故事") || t.contains("第3段故事")),
        "the resume continues mid-story, not from the top: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("第1段故事")),
        "the resume must not restart the story: {texts:?}"
    );
    // The notice is the program's LAST sentence. It is pre-synthesized into
    // the local cue cache, so in this run it reaches the device as AUDIO with
    // no text field (the cloud path would carry the phrase as text) — either
    // way the last play is the notice, never another story sentence.
    let (_, last_meta) = voice.last().expect("a last play");
    assert!(
        last_meta["text"].as_str() == Some(notice) || last_meta["has_audio"] == true,
        "the truncation notice is the program's last play: {last_meta:?}"
    );
    assert!(
        !last_meta["text"].as_str().unwrap_or("").contains("段故事"),
        "the last sentence is the label, not another story sentence: {last_meta:?}"
    );

    // R2-BM5/BM5-6: the notice is a PROMPT output, not part of the story
    // answer — the receipt carries its class. (M5: the play is confirmed by
    // the helper, so /mode's live `outputs` list no longer holds it; the
    // submission record is the durable evidence.)
    let log_all = state.voice.recent(600);
    assert!(
        log_all
            .iter()
            .any(|e| e["kind"] == "tts_played" && e["delivery_kind"].as_str() == Some("prompt")),
        "the truncation notice must be submitted as a prompt delivery: {log_all:?}"
    );

    let log = state.voice.recent(600);
    assert!(
        log.iter().any(|e| e["kind"] == "program_truncation_notice"
            && e["phrase"] == notice
            && e["source"].is_string()),
        "the notice is recorded with its own phrase and its source: {log:?}"
    );
    assert!(
        log.iter()
            .any(|e| e["kind"] == "program_finished" && e["complete"] == false),
        "the program ends marked incomplete: {log:?}"
    );
    assert_eq!(
        llm.seen_requests().len(),
        calls_before,
        "a truncated story is labelled, never regenerated"
    );
    assert!(
        state.voice.paused_bookmark("dot2-d").is_none(),
        "the program is over: no bookmark left behind"
    );
}

/// The other half of the flag's meaning: a program whose generation FINISHED
/// (the model delivered its whole answer) is not truncated, so 继续 ends
/// silently — no notice, no label.
#[tokio::test]
async fn a_complete_program_does_not_claim_to_be_truncated() {
    let sentences: Vec<String> = (1..=3).map(|i| format!("第{i}段故事，很短。")).collect();
    let llm = FakeLlm::new(vec![Scripted::TextPaced {
        chunks: sentences,
        delay_ms: 20,
    }]);
    let (base, state, gw) = spawn_mic_app_stream(llm.clone(), "").await;
    let _ = base;
    set_truncation_notice(&state, Some("故事断了。")).await;

    explicit_request(&state, &gw, 1, "讲个故事").await;
    gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await;
    // Wait until generation finished (the program is marked complete while
    // its sentences are still being read out).
    wait_until_sync("the program is fully generated", || {
        state.voice.mode_snapshot()["devices"][0]["program"]["live"]["complete"] == true
    })
    .await;

    explicit_request(&state, &gw, 2, "暂停").await;
    let stopped = stops_within(&gw, Duration::from_millis(800)).await;
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
    tokio::time::sleep(Duration::from_millis(300)).await;
    let bookmark = state
        .voice
        .paused_bookmark("dot2-d")
        .expect("a pause leaves a bookmark either way");
    assert_eq!(bookmark["complete"], true, "{bookmark:?}");

    explicit_request(&state, &gw, 3, "继续").await;
    let plays = play_program_through(&gw, Duration::from_millis(400)).await;
    let texts: Vec<String> = plays
        .iter()
        .filter(|(_, m)| m["channel"] == "voice")
        .map(|(_, m)| m["text"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(!texts.is_empty(), "继续 must read the rest");
    assert!(
        !texts.iter().any(|t| t.contains("故事断了")),
        "a fully generated program is not truncated: {texts:?}"
    );
    let log = state.voice.recent(600);
    assert!(
        !log.iter().any(|e| e["kind"] == "program_truncation_notice"),
        "{log:?}"
    );
    assert!(
        log.iter()
            .any(|e| e["kind"] == "program_finished" && e["complete"] == true),
        "{log:?}"
    );
}

/// The notice phrase is configuration, not code: changing it changes what the
/// device says, and an empty phrase means "stay silent".
#[tokio::test]
async fn the_truncation_notice_is_configurable_and_optional() {
    let state = AppState::new(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(FakeLlm::new(vec![]))),
    );
    // Valid: one sentence, no quotes/slashes.
    set_truncation_notice(&state, Some("这里就断了。")).await;
    assert_eq!(
        state.voice.settings_snapshot().truncation_notice,
        "这里就断了。"
    );
    // Empty = off (legal): the phrase is configuration, not a required input.
    set_truncation_notice(&state, Some("")).await;
    assert_eq!(state.voice.settings_snapshot().truncation_notice, "");
    // A wall of text or a quote is refused before it reaches the cache.
    let too_long = "很".repeat(61);
    let err = state.voice.set_mode(
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
        Some(too_long),
        "test",
    );
    assert!(err.is_err(), "an over-long notice must be refused");
    let err = state.voice.set_mode(
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
        Some("带\"引号\"的提示".to_string()),
        "test",
    );
    assert!(
        err.is_err(),
        "quotes break the phrase cache naming: refused"
    );
}

/// R1e LED: a paused program paints `paused` (amber), outranks the follow
/// window that the same utterance opened, and the bookmark expires on its own
/// so the ring cannot stay amber for ever ([docs/led-states.md]).
#[tokio::test]
async fn a_paused_program_paints_paused_and_expires() {
    let story: Vec<String> = (1..=8)
        .map(|i| format!("第{i}段故事内容，讲的是山谷里的风。"))
        .collect();
    let llm = FakeLlm::new(vec![Scripted::TextPaced {
        chunks: story,
        delay_ms: 40,
    }]);
    let (_base, state, gw) = spawn_mic_app_stream(llm, "").await;

    explicit_request(&state, &gw, 1, "讲个长故事").await;
    gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    explicit_request(&state, &gw, 2, "暂停").await;
    let stopped = stops_within(&gw, Duration::from_millis(800)).await;
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
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // The last assistant_state the core SENT is `paused` — not `listening`,
    // even though the same utterance opened the follow window.
    let snap = state.voice.mode_snapshot();
    assert!(
        !snap["devices"][0]["program"]["paused"].is_null(),
        "bookmark exists: {snap:?}"
    );
    let log = state.voice.recent(400);
    let states: Vec<&str> = log
        .iter()
        .filter(|e| e["kind"] == "assistant_state")
        .filter_map(|e| e["state"].as_str())
        .collect();
    // `recent()` is newest-first: the LAST thing the core sent is index 0.
    assert!(
        states.first() == Some(&"paused"),
        "the ring must show paused, saw {states:?}"
    );

    // Expiry is time-based: sweep as of a future instant instead of sleeping
    // ten minutes.
    let expired = state
        .voice
        .program_ttl_sweep_at(hm_core::id::now_ms() + 11 * 60 * 1000);
    assert_eq!(
        expired,
        vec!["dot2-d".to_string()],
        "the sweep expires this device's bookmark"
    );
    let log = state.voice.recent(400);
    assert!(
        log.iter().any(|e| e["kind"] == "program_expired"),
        "the bookmark must expire with a reason: {log:?}"
    );
    let snap = state.voice.mode_snapshot();
    assert!(
        snap["devices"][0]["program"]["paused"].is_null(),
        "expired bookmark is gone: {snap:?}"
    );
    // The presentation ticker converges the ring within ~1 s (send on change).
    let mut converged = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let log = state.voice.recent(400);
        let last = log
            .iter()
            .filter(|e| e["kind"] == "assistant_state")
            .filter_map(|e| e["state"].as_str())
            .next();
        if last != Some("paused") {
            converged = true;
            break;
        }
    }
    assert!(converged, "the ring converges off paused after expiry");
}

/// R1e: the console's stop button means 停 — it must discard the bookmark,
/// not silently keep a resume point (the pause/resume card is the way to keep
/// one).
#[tokio::test]
async fn the_console_stop_discards_the_bookmark() {
    let story: Vec<String> = (1..=8)
        .map(|i| format!("第{i}段故事内容，讲的是山谷里的风。"))
        .collect();
    let llm = FakeLlm::new(vec![Scripted::TextPaced {
        chunks: story,
        delay_ms: 40,
    }]);
    let (base, state, gw) = spawn_mic_app_stream(llm, "").await;

    explicit_request(&state, &gw, 1, "讲个长故事").await;
    gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    explicit_request(&state, &gw, 2, "暂停").await;
    let stopped = stops_within(&gw, Duration::from_millis(800)).await;
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
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        state.voice.paused_bookmark("dot2-d").is_some(),
        "premise: a bookmark exists"
    );

    let stop: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/devices/dot2-d/stop"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        stop["program_discarded"], true,
        "the stop reports what it dropped: {stop:?}"
    );
    assert!(
        state.voice.paused_bookmark("dot2-d").is_none(),
        "停 discards the bookmark"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter().any(|e| e["kind"] == "program_discarded"),
        "the discard is recorded: {log:?}"
    );
}

/// R1e: 退下 is the strongest command (the user's words: "说最高指令，就是
/// close") — it must end the session AND drop the paused program, otherwise the
/// ring stays amber after the user closed the session.
#[tokio::test]
async fn retreat_discards_the_paused_program() {
    let story: Vec<String> = (1..=8)
        .map(|i| format!("第{i}段故事内容，讲的是山谷里的风。"))
        .collect();
    let llm = FakeLlm::new(vec![Scripted::TextPaced {
        chunks: story,
        delay_ms: 40,
    }]);
    let (base, state, gw) = spawn_mic_app_stream(llm, "").await;

    explicit_request(&state, &gw, 1, "讲个长故事").await;
    gw.until(|f| matches!(f, CoreFrame::PlayBegin { .. })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    explicit_request(&state, &gw, 2, "暂停").await;
    let stopped = stops_within(&gw, Duration::from_millis(800)).await;
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
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        state.voice.paused_bookmark("dot2-d").is_some(),
        "premise: paused"
    );

    explicit_request(&state, &gw, 3, "退下吧").await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let log = state.voice.recent(300);
    let retreat = log
        .iter()
        .find(|e| e["kind"] == "session_retreat")
        .unwrap_or_else(|| panic!("retreat must be logged: {log:?}"));
    assert_eq!(
        retreat["program_discarded"], true,
        "退下 discards the bookmark: {retreat:?}"
    );
    assert!(
        state.voice.paused_bookmark("dot2-d").is_none(),
        "no bookmark after 退下"
    );
    let mode: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(
        mode["devices"][0]["assistant_state"], "paused",
        "the ring leaves paused: {mode:?}"
    );
}

/// R1e: a pure clock question is answered by the runtime — no model call at all
/// (so it cannot be answered by narrating a tool call).
#[tokio::test]
async fn a_pure_clock_question_never_reaches_the_model() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["不应该到达".into()],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm.clone()).await;

    explicit_request(&state, &gw, 1, "今天周几？").await;
    let (play_id, meta) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;
    let said = meta["text"].as_str().unwrap_or("").to_string();
    assert!(
        said.starts_with("今天星期") && said.ends_with('。'),
        "spoken weekday: {said}"
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(llm.seen_requests().len(), 0, "the model was never asked");
    let log = state.voice.recent(100);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "clock_answered_locally" && e["text"] == "今天周几？"),
        "the local answer is recorded: {log:?}"
    );

    // A question that is more than the clock still goes to the model.
    explicit_request(&state, &gw, 2, "今天周几，我要不要带伞").await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;
    assert_eq!(
        llm.seen_requests().len(),
        1,
        "a mixed question still needs the model"
    );
}

/// R1g (user flow "你好高达 → 暂停"): on a half-duplex link the wake word is
/// the only entry and it stops the audio — so the stop must leave a BOOKMARK.
/// Discarding it here would make 暂停/继续 unreachable for every playing
/// program (the R1e feature would be dead on arrival).
#[tokio::test]
async fn a_wake_word_during_a_story_leaves_a_bookmark() {
    let story: Vec<String> = (1..=8)
        .map(|i| format!("第{i}段故事内容，讲的是山谷里的风。"))
        .collect();
    let llm = FakeLlm::new(vec![Scripted::TextPaced {
        chunks: story,
        delay_ms: 60,
    }]);
    let (base, state, gw) = spawn_mic_app(llm, "你好高达").await;

    explicit_request(&state, &gw, 1, "讲个长故事").await;
    let (play_id, _) = gw.until_any_voice_play().await;
    gw.confirm_play(&play_id).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    // The wake word (KWS is the fast path; the same policy runs for ASR).
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
    let stopped = stops_within(&gw, Duration::from_millis(800)).await;
    assert!(
        stopped.contains(&play_id),
        "the wake must stop the story: {stopped:?}"
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
    assert!(
        dev["program"]["paused"].is_object(),
        "the stopped story must be resumable: {dev:?}"
    );

    // 继续 (the in-window word, or the console button) picks it up.
    let resumed = state.voice.resume_program("dot2-d");
    assert!(resumed, "the bookmark must resume");
    let (play2, _) = gw.until_any_voice_play().await;
    assert_ne!(play2, play_id, "the resume is a new play");
}

/// M5: a resumed program speaks one sentence at a time and waits for each
/// sentence's terminal. Two things this pins:
///   * the NEXT sentence is not submitted before the current one finished
///     (no crossing, no interleaved cuts),
///   * the bookmark advances only on a CONSUMED terminal — a sentence whose
///     play is stopped keeps the cursor ON it (repeat, never skip).
#[tokio::test]
async fn a_resumed_program_waits_for_each_sentence_and_advances_on_consumed() {
    let story: Vec<String> = (1..=3)
        .map(|i| format!("第{i}句讲的是山谷里的风。"))
        .collect();
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: story.clone(),
    }]);
    let (base, state, gw) = spawn_mic_app(llm, "你好高达").await;

    explicit_request(&state, &gw, 1, "讲个长故事").await;
    let (first_play, _) = gw.until_play("voice").await;
    gw.confirm_play(&first_play).await;
    assert!(
        state.voice.pause_program("dot2-d", "test"),
        "premise: a program is paused"
    );
    for play in stops_within(&gw, Duration::from_millis(800)).await {
        gw.confirm_stop(&play).await;
    }

    assert!(
        state.voice.resume_program("dot2-d"),
        "the bookmark must resume"
    );

    // Sentence 1 arrives and gets a hardware-measured finish — AFTER the
    // device's ordinary `accepted` + `device_confirmed` receipts, which are
    // NOT this play's end (a regression here stopped the reading after one
    // sentence and re-bookmarked it).
    let (p1, meta1) = gw.until_play("voice").await;
    let said1 = meta1["text"].as_str().unwrap_or("").to_string();
    assert!(
        said1.starts_with("第1句"),
        "the resume starts at the bookmark: {said1}"
    );
    gw.confirm_play(&p1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: p1.clone(),
        played_ms: 2000,
        consumed_ms: Some(1900),
        progress_basis: Some("hw_delay".into()),
        ts: hm_core::id::now_ms(),
    })
    .await;

    // ONLY NOW sentence 2 may be submitted.
    let (p2, meta2) = gw.until_play("voice").await;
    let said2 = meta2["text"].as_str().unwrap_or("").to_string();
    assert!(
        said2.starts_with("第2句"),
        "the second sentence follows, not crosses: {said2}"
    );

    // Sentence 2 is STOPPED before it finished: the bookmark must stay on it.
    gw.send(&GatewayFrame::StopReceipt {
        task_id: p2.clone(),
        stage: "device_confirmed".into(),
        played_ms: Some(400),
        consumed_ms: Some(380),
        progress_basis: Some("hw_delay".into()),
        detail: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

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
    let bk = &dev["program"]["paused"];
    assert!(
        bk.is_object(),
        "the stopped sentence leaves a bookmark: {dev:?}"
    );
    let next = bk["next"].as_str().unwrap_or("");
    assert!(
        next.starts_with("第2句"),
        "the bookmark repeats the unheard sentence: {next}"
    );

    let log = state.voice.recent(100);
    let delivery = log
        .iter()
        .filter(|e| e["kind"] == "program_delivery")
        .last();
    let d = delivery.unwrap_or_else(|| panic!("a program_delivery record is written: {log:?}"));
    // Both sentences' CONSUMED milliseconds: 1900 for the finished one plus
    // 380 the second one played before the stop. Not `played_ms` (2400).
    assert_eq!(
        d["consumed_ms"].as_u64(),
        Some(2280),
        "the consumed cursor is the DAC number: {d}"
    );
    assert_eq!(
        d["played_ms"].as_u64(),
        Some(2400),
        "written stays separate: {d}"
    );
    assert_eq!(d["basis"].as_str(), Some("hw_delay"));
    assert_eq!(
        d["advanced"].as_bool(),
        Some(false),
        "the delivery did not run to its end: {d}"
    );
}

/// M5: the turn budget bounds the WAITING, and only the waiting. A model that
/// never answers ends the turn in bounded time with ONE terminal — and a
/// turn that is still speaking is never cut by the same clock.
#[tokio::test]
async fn a_silent_model_ends_the_turn_within_the_wait_budget() {
    let llm = FakeLlm::new(vec![Scripted::Delayed {
        delay_ms: 5_000,
        chunks: vec!["太晚了".into()],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm.clone()).await;
    state.voice.set_turn_wait_budget(300, 1_200);

    let started = std::time::Instant::now();
    explicit_request(&state, &gw, 1, "讲个故事").await;
    // 300 ms budget + one 250 ms tick of granularity + slack.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let log = state.voice.recent(60);
    let hit = log.iter().find(|e| e["kind"] == "turn_budget_exceeded");
    let hit = hit.unwrap_or_else(|| panic!("the budget fired: {log:?}"));
    assert_eq!(hit["phase"].as_str(), Some("first_answer"));
    assert!(hit["wait_ms"].as_u64().unwrap_or(0) >= 300, "{hit}");
    // Exactly ONE terminal, and this turn's ownership is cleared: a budget
    // break that skipped the normal end left `current_task` set forever.
    let ends = log.iter().filter(|e| e["kind"] == "turn_end").count();
    assert_eq!(ends, 1, "one turn_end for the bounded end: {log:?}");
    assert!(
        log.iter()
            .any(|e| e["kind"] == "turn_end" && e["status"] == "budget_exceeded"),
        "the terminal names the reason: {log:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mode: Value = reqwest::get(format!("{_base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        mode["devices"][0]["current_task"].is_null(),
        "no orphaned task: {mode:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(2_500),
        "the turn ended bounded, not when the model finally answered"
    );
}

/// M5: two capture windows never swap identities — an OLDER capture's text
/// that arrives after a newer one is dropped by name, not executed.
#[tokio::test]
async fn an_out_of_order_transcript_is_dropped_by_name() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["不应该到达".into()],
    }]);
    let (_base, state, _gw) = spawn_app_with_gateway(llm).await;
    state
        .voice
        .test_deliver_transcript("dot2-d", "开灯", 5, 100, 0)
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let before = state
        .voice
        .recent(50)
        .iter()
        .filter(|e| e["kind"] == "mic_ignored_unwoken")
        .count();
    assert_eq!(before, 1, "the newer capture was processed once");

    // The older capture's text arrives late.
    state
        .voice
        .test_deliver_transcript("dot2-d", "开灯", 4, 90, 0)
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log = state.voice.recent(50);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "transcript_out_of_order" && e["input_id"] == 4),
        "the late older capture is named: {log:?}"
    );
    let after = log
        .iter()
        .filter(|e| e["kind"] == "mic_ignored_unwoken")
        .count();
    assert_eq!(after, before, "the late capture ran no second decision");
}

/// M5: a transcript whose audio was captured BEFORE a stop/退下 must never
/// execute — the text can arrive after the session ended (retreat and stop
/// share the epoch bump, `retreat_session` -> `stop_device`). The identity
/// gate names the reason instead of trusting the ordering of two async paths.
#[tokio::test]
async fn a_transcript_captured_before_a_stop_never_executes() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["不应该到达".into()],
    }]);
    let (_base, state, gw) = spawn_mic_app(llm.clone(), "你好高达").await;
    // 退下 (a session end) lands BEFORE the captured audio's transcript.
    let _ = state.voice.stop_device("dot2-d");
    // The late transcript of the earlier capture (captured at epoch 0).
    state
        .voice
        .test_deliver_transcript("dot2-d", "开灯", 1, 42, 0)
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = state.voice.recent(50);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "transcript_stale_after_epoch"),
        "the stale transcript is named, not executed: {log:?}"
    );
    assert_eq!(
        llm.seen_requests().len(),
        0,
        "no turn may run for stale audio"
    );
}

/// R1g (P3): the amber converges through the WAKE WORD. R1f had a second
/// policy path — an explicit close word closed a paused program without a
/// wake — which contradicts "half duplex = the wake word first". Now the
/// wake (KWS works with nothing playing) opens the window, and 退下 inside it
/// clears the bookmark; the ring stops being amber because the program is
/// gone, not because a timer expired.
#[tokio::test]
async fn a_wake_word_then_retreat_clears_the_paused_amber() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["不应该到达".into()],
    }]);
    let (base, state, gw) = spawn_mic_app(llm.clone(), "退下吧").await;
    // The ack is irrelevant here and would hold the voice channel; the
    // window is what this test is about.
    let _ = state.voice.set_mode(
        "dot2-d",
        None,
        None,
        None,
        None,
        None,
        Some(hm_core::voice::WakeAckSettings {
            enabled: false,
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

    // Premise: a paused program (the amber).
    explicit_request(&state, &gw, 1, "讲个长故事").await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;
    assert!(
        state.voice.pause_program("dot2-d", "test"),
        "premise: a program is paused"
    );
    let stopped = stops_within(&gw, Duration::from_millis(500)).await;
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
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The wake: a keyword hit with nothing playing (the P1 idle cell).
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
    tokio::time::sleep(Duration::from_millis(150)).await;
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
        "the wake opens the window"
    );
    assert!(
        dev["program"]["paused"].is_object(),
        "a wake must not clear a pause"
    );

    // 退下 inside the window: the session closes and the bookmark is dropped.
    let loud = loud_block();
    let quiet = quiet_block();
    for seq in 3..=12 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 13..=22 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;

    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "session_retreat" && e["program_discarded"] == true),
        "退下 closes and drops the paused program: {log:?}"
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
    assert!(
        dev["program"]["paused"].is_null(),
        "the amber has nothing left to show"
    );
    assert_ne!(dev["assistant_state"], "paused");
    assert_eq!(
        llm.seen_requests().len(),
        1,
        "only the story turn reached the model"
    );
}

/// R1e: a punctuation-only segment is not speech. `take_speak_segment` cuts at
/// punctuation INSIDE quoted dialogue, so a segment can end up being `”` alone;
/// Volc answers that with 45002001 "No readable text!" and the pipeline used to
/// treat it as an answer-level failure (device log 2026-11-12: a long joke with
/// quoted dialogue → one punctuation-only segment → 8 tts_failed events).
#[tokio::test]
async fn a_punctuation_only_segment_is_skipped_not_sent_to_tts() {
    // The splitter really does produce such a head: a HARD cut after a quoted
    // question leaves the closing quote behind as the next segment.
    let mut buf = "他问：“你在哭吗？”".to_string();
    let first = hm_core::voice::VoicePipeline::take_speak_segment(&mut buf).unwrap();
    assert!(
        first.ends_with('？'),
        "premise: cut after the question mark, got {first:?}"
    );
    assert_eq!(
        buf.trim(),
        "”",
        "premise: the leftover is the closing quote alone"
    );

    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["他问：“你在哭吗？".to_string(), "”".to_string()],
    }]);
    let (_base, state, gw) = spawn_mic_app_stream(llm, "").await;

    explicit_request(&state, &gw, 1, "讲个笑话").await;
    let (play_id, meta) = gw.until_any_voice_play().await;
    gw.confirm_play(&play_id).await;
    assert_eq!(
        meta.as_str(),
        "他问：“你在哭吗？",
        "the readable part still speaks"
    );

    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "tts_segment_skipped" && e["segment"] == "”"),
        "the quote-only segment is skipped and recorded: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "tts_failed"),
        "nothing was sent to a TTS engine that cannot read it: {log:?}"
    );
}
