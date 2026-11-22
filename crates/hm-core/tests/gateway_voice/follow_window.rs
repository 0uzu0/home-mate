use super::*;

/// 2026-11-13 (user): commands no longer wait for an ack, so the bare wake is
/// the only path where the ack's outcome still matters. A device failure must
/// still be classified as NOT heard — the honesty invariant outlives the
/// waiting behaviour.
#[tokio::test]
async fn a_failed_bare_wake_ack_is_not_reported_as_heard() {
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
            task_id,
            stage: "failed".into(),
            detail: Some("device refused the wav".into()),
            ts: hm_core::id::now_ms(),
        })
        .await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log = state.voice.recent(200);
    assert!(
        log.iter().any(|e| e["kind"] == "wake_ack_settled"
            && e["outcome"] == "failed"
            && e["settled"] == false),
        "a failed ack must be classified as not heard: {log:?}"
    );
}

/// M6b (S3): the one path S2 shipped without a unit test. When the wake word
/// takes over MEDIA, the device is paused immediately and the ack must not
/// wait for the ASR confirmation (user report 2026-11-13: "得等一秒多才说
/// 来了" — the stop took 0.1–0.3 s, the ack 0.3–2.0 s). A REAL media session
/// is playing here: the assertion is that the takeover's own ack is submitted
/// while the segment that would confirm it has not even been recorded.
#[tokio::test]
async fn media_takeover_acks_without_waiting_for_asr() {
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
            chunks: vec!["正在播放晚风信笺。".into()],
        },
    ]);
    let (base, state, gw, _music) = spawn_app_with_gateway_media(llm).await;
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

    // Start the song; its stream is what makes `content.media` live.
    explicit_request(&state, &gw, 1, "播放第一名的歌").await;
    let (media_id, _) = gw.until_media_play().await;
    gw.confirm_play(&media_id).await;
    // Drain its chunks so the play is genuinely in progress (not waiting on
    // this test's socket) — the media channel stays device_confirmed.
    let _ = gw.until(|f| matches!(f, CoreFrame::PlayChunk { .. })).await;

    // The wake hits the spotter while the song plays. NO ASR transcript is
    // sent afterwards: the ack must be the takeover's fact, not the ASR's.
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
    })
    .await;

    let ack = gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("ack:")
            )
        })
        .await;
    match ack {
        CoreFrame::Play {
            task_id,
            content_ref,
            ..
        }
        | CoreFrame::PlayBegin {
            task_id,
            content_ref,
            ..
        } => {
            assert!(
                content_ref.starts_with("ack:在呢"),
                "the configured phrase: {content_ref}"
            );
            // Finish it so the note is classified from the device's finish.
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
                played_ms: 760,
                consumed_ms: Some(700),
                progress_basis: Some("hw_delay".into()),
                ts: hm_core::id::now_ms(),
            })
            .await;
        }
        f => panic!("expected the takeover ack, got {f:?}"),
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log = state.voice.recent(200);
    let ack_event = log
        .iter()
        .find(|e| e["kind"] == "wake_ack_on_media_interrupt")
        .unwrap_or_else(|| panic!("the media-takeover ack must name its trigger: {log:?}"));
    assert_ne!(
        ack_event["ack"],
        json!("failed"),
        "the ack was submitted and spoken: {ack_event}"
    );
    // The takeover paused the media session (queue + position kept).
    assert!(
        log.iter()
            .any(|e| e["kind"] == "keyword_stop" && e["content"]["media"] == json!(true)),
        "the hit stopped the playing media: {log:?}"
    );
}

// --- 2026-11-13 M6a: connection generations bound every play fact -----------
/// M6a (旧代回执不结算): the gateway clears its audio state when its link dies,
/// so a `play_finished` for an OLD play that arrives on a NEWER connection is
/// stale evidence. It must be recorded and dropped — never resolve a waiter,
/// never look like a finish for the current generation.
#[tokio::test]
async fn an_old_generation_receipt_never_settles_anything() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["在的".into()],
    }]);
    let (base, state, gw) = spawn_app_with_gateway(llm).await;
    explicit_request(&state, &gw, 1, "在吗").await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The link dies mid-play (generation 1 ends; the core marks the play
    // unknown and retires its id).
    gw.close().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Reconnect: generation 2. The device now claims the OLD play finished.
    let gw = {
        let st: Value = reqwest::get(format!("{base}/api/v0/status"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let addr = st["gateway"]["bind_addr"].as_str().unwrap().to_string();
        let port: u16 = addr.rsplit(':').next().unwrap().parse().unwrap();
        connect_fake_gateway(port).await
    };
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: play_id.clone(),
        played_ms: 1_500,
        consumed_ms: Some(1_400),
        progress_basis: Some("hw_delay".into()),
        ts: hm_core::id::now_ms(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let devices: Value = reqwest::get(format!("{base}/api/v0/devices"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let recent = devices["recent"].as_array().unwrap();
    assert!(
        recent
            .iter()
            .any(|r| r["kind"] == "stale_receipt" && r["play_id"] == play_id.as_str()),
        "the late fact must be recorded as stale: {recent:?}"
    );
    assert!(
        !recent
            .iter()
            .any(|r| r["kind"] == "play_finished" && r["play_id"] == play_id.as_str()),
        "an old generation's finish must not be treated as a finish: {recent:?}"
    );
    // And the voice loop never saw it.
    let log = state.voice.recent(400);
    assert!(
        !log.iter()
            .any(|e| e["kind"] == "play_finished" && e["play_id"] == play_id),
        "no play_finished for a retired play id"
    );
}

/// M6a: the finish carries consumed vs written and the basis. The voice log is
/// where M5's resume cursor will read it, so the numbers must survive intact.
#[tokio::test]
async fn a_finish_logs_consumed_and_its_basis() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["好的".into()],
    }]);
    let (base, state, gw) = spawn_app_with_gateway(llm).await;
    explicit_request(&state, &gw, 1, "说句话").await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: play_id.clone(),
        played_ms: 1_500,
        consumed_ms: Some(1_340),
        progress_basis: Some("hw_delay".into()),
        ts: hm_core::id::now_ms(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log = state.voice.recent(400);
    let entry = log
        .iter()
        .find(|e| e["kind"] == "play_finished" && e["play_id"] == play_id)
        .unwrap_or_else(|| panic!("no play_finished entry: {log:?}"));
    assert_eq!(entry["played_ms"], 1_500);
    assert_eq!(
        entry["consumed_ms"], 1_340,
        "consumed must survive to the log"
    );
    assert_eq!(entry["progress_basis"], "hw_delay");
}

/// M6b (S3): the AEC owner is a DEVICE fact, never a core guess. The gateway
/// now reports "device" for the dot2-d audio path; an empty/older report is
/// shown as unknown (null), not as "core".
#[tokio::test]
async fn the_aec_owner_comes_from_the_device_report() {
    let (_base, state, gw) = spawn_app_with_gateway(FakeLlm::new(vec![])).await;
    let report = |owner: &'static str, reference: &'static str| {
        let gw = gw.clone();
        async move {
            gw.send(&GatewayFrame::ModeReport {
                device: "dot2-d".into(),
                duplex: hm_core::gateway::DuplexState {
                    capability: "half".into(),
                    requested: "half".into(),
                    negotiated: "half".into(),
                    effective: "half".into(),
                },
                aec: hm_core::gateway::AecState {
                    owner: owner.into(),
                    reference: reference.into(),
                },
            })
            .await;
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    };

    report("device", "available").await;
    let snap = state.voice.mode_snapshot();
    assert_eq!(snap["devices"][0]["aec"]["owner"], json!("device"));

    // No report yet → unknown (not a guess). An EMPTY owner is the same fact.
    report("", "").await;
    let snap = state.voice.mode_snapshot();
    assert!(
        snap["devices"][0]["aec"].is_null(),
        "an unreported owner must be unknown, not core: {}",
        snap["devices"][0]["aec"]
    );
}

/// M6b (S3): the keyword chain has three DIFFERENT proofs — what the core
/// sent, what the device installed, and what the spotter loaded. Only the
/// three agreeing (and a running spotter) says the vocabulary is in force.
#[tokio::test]
async fn the_kws_chain_records_sent_installed_and_loaded_hashes() {
    let (_base, state, gw) = spawn_app_with_gateway(FakeLlm::new(vec![])).await;
    let sent = state.voice.deploy_reflex("dot2-d").expect("deploy");
    let sent_hash = sent["hash"].as_str().unwrap().to_string();
    let (keywords, hash_on_wire, retreat_words, intents_hash) =
        match gw.until(|f| matches!(f, CoreFrame::KwsDeploy { .. })).await {
            CoreFrame::KwsDeploy {
                keywords,
                retreat_words,
                intents_hash,
                ..
            } => {
                let h = hm_core::config::fnv1a_hex(keywords.as_bytes());
                (keywords, h, retreat_words, intents_hash)
            }
            f => panic!("expected KwsDeploy, got {f:?}"),
        };
    assert_eq!(
        hash_on_wire, sent_hash,
        "the frame carries what the core compiled"
    );
    // Contract v2 §2.5 (#5): the same frame names the words the device may
    // act on locally, and versions the word→intent map it was given.
    assert_eq!(
        retreat_words,
        vec!["停止播放".to_string()],
        "the default table's only retreat intent must travel with the deploy"
    );
    assert!(!intents_hash.is_empty(), "the intent map needs a version");
    assert_eq!(sent["intents_hash"], json!(intents_hash));

    // The device installed it…
    gw.send(&GatewayFrame::KwsDeployReceipt {
        device: "dot2-d".into(),
        accepted: true,
        hash: sent_hash.clone(),
        keywords: keywords.lines().count() as u64,
        detail: "installed; 1 local exit word".into(),
        local_exit: 1,
        threshold: 0.25,
        score: 3.0,
        intents_hash: intents_hash.clone(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let snap = state.voice.mode_snapshot();
    let d = &snap["devices"][0]["reflex"]["deploy"];
    assert_eq!(d["sent_hash"], json!(sent_hash));
    assert_eq!(d["hash"], json!(sent_hash));
    assert!(d["loaded_hash"].is_null(), "installed is not loaded: {d}");
    assert_eq!(d["state"], json!("accepted"));
    // #5: installed vocabulary AND installed meaning are two facts.
    assert_eq!(d["retreat_words_sent"], json!(1));
    assert_eq!(d["local_exit"], json!(1));
    assert_eq!(d["local_exit_matches"], json!(true));
    assert_eq!(d["intents_hash_matches"], json!(true));
    assert_eq!(d["local_exits"], json!(0));

    // A local exit fact is recorded, and it creates no session of its own.
    gw.send(&GatewayFrame::LocalExitFlushed {
        device: "dot2-d".into(),
        keyword: "停止播放".into(),
        ts: hm_core::id::now_ms(),
        latency_ms: 84,
        channels: vec!["voice".into(), "music".into()],
        detail: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let snap = state.voice.mode_snapshot();
    let d = &snap["devices"][0]["reflex"]["deploy"];
    assert_eq!(d["local_exits"], json!(1));
    assert_eq!(d["last_local_exit"]["latency_ms"], json!(84));
    assert_eq!(
        snap["devices"][0]["session"]["state"], "idle",
        "a device-side flush is a fact, not a session decision"
    );

    // …but an empty loaded_hash is NOT a load proof.
    gw.send(&GatewayFrame::KwsReport {
        device: "dot2-d".into(),
        state: "starting".into(),
        detail: "restarting for keyword deploy".into(),
        restarts: 0,
        detections: 0,
        guarded: 0,
        epoch: 7,
        loaded_hash: String::new(),
        loaded_ms: 0,
        local_exit: 0,
        threshold: 0.25,
        score: 3.0,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let snap = state.voice.mode_snapshot();
    let d = &snap["devices"][0]["reflex"]["deploy"];
    assert!(d["loaded_hash"].is_null(), "starting is not loaded: {d}");

    // The spotter's own ready line proves the LOAD — and only then do all
    // three agree.
    gw.send(&GatewayFrame::KwsReport {
        device: "dot2-d".into(),
        state: "running".into(),
        detail: "ready in 1288ms".into(),
        restarts: 0,
        detections: 0,
        guarded: 0,
        epoch: 7,
        loaded_hash: sent_hash.clone(),
        loaded_ms: hm_core::id::now_ms(),
        local_exit: 1,
        threshold: 0.25,
        score: 3.0,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let snap = state.voice.mode_snapshot();
    let d = &snap["devices"][0]["reflex"]["deploy"];
    assert_eq!(d["loaded_hash"], json!(sent_hash));
    assert_eq!(d["loaded_matches"], json!(true), "{d}");
    assert_eq!(d["state"], json!("loaded"));
    // #5: the DEVICE's armed count is a fact of its own and survives a
    // restart — `/mode.kws.local_exit`, not the in-memory deploy record.
    assert_eq!(
        snap["devices"][0]["kws"]["local_exit"],
        json!(1),
        "the armed local exit must be visible as a device fact"
    );

    // A different loaded hash is a MISMATCH, never silently "fine".
    gw.send(&GatewayFrame::KwsReport {
        device: "dot2-d".into(),
        state: "running".into(),
        detail: "ready".into(),
        restarts: 0,
        detections: 0,
        guarded: 0,
        epoch: 7,
        loaded_hash: "deadbeefdeadbeef".into(),
        loaded_ms: hm_core::id::now_ms(),
        local_exit: 0,
        threshold: 0.25,
        score: 3.0,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let snap = state.voice.mode_snapshot();
    let d = &snap["devices"][0]["reflex"]["deploy"];
    assert_eq!(d["loaded_matches"], json!(false), "{d}");

    // The spotter exiting clears the load proof (it is no longer loaded).
    gw.send(&GatewayFrame::KwsReport {
        device: "dot2-d".into(),
        state: "down".into(),
        detail: "sidecar exited".into(),
        restarts: 1,
        detections: 0,
        guarded: 0,
        epoch: 7,
        loaded_hash: String::new(),
        loaded_ms: 0,
        local_exit: 0,
        threshold: 0.25,
        score: 3.0,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let snap = state.voice.mode_snapshot();
    let d = &snap["devices"][0]["reflex"]["deploy"];
    assert!(
        d["loaded_hash"].is_null(),
        "a dead spotter has no loaded vocabulary: {d}"
    );
}

// --- v0.16: ASR wake fallback (2026-11-14) --------------------------------------
//
// The user's commercial question: should every word spoken in the room be
// transcribed? It is a product decision, so it is a setting. ON (default) is
// the recall path every earlier release had: a closed VAD segment is
// transcribed even when no wake word matched, so a missed keyword can still
// wake the assistant. OFF is the privacy path: ASR runs only inside an open
// session (keyword wake, follow window, barge-in, an actual turn), and idle
// room speech never reaches the ASR endpoint.

/// OFF: a segment spoken while the session is closed never reaches ASR, and
/// the skip is counted and journaled (a privacy claim must be a number).
#[tokio::test]
async fn asr_wake_fallback_off_does_not_transcribe_idle_speech() {
    let llm = FakeLlm::new(vec![]);
    let asr = CountingAsr::new("今天天气怎么样");
    let (base, state, gw, _music) = spawn_app_with_gateway_media_asr(llm, asr.clone()).await;

    let put = reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({ "asr_wake_fallback": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);
    // The PUT returns the device view (as it does for every other card); the
    // global flag is read back from GET /mode, exactly like `spoken`.
    let snap: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snap["asr_wake_fallback"], json!(false), "{snap}");

    // 800 ms of speech, then the quiet that closes it — nobody woke the
    // device and nothing is playing.
    let mut seq = 1;
    for _ in 0..10 {
        gw.send(&audio_chunk(seq, &loud_block())).await;
        seq += 1;
    }
    for _ in 0..14 {
        gw.send(&audio_chunk(seq, &quiet_block())).await;
        seq += 1;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        asr.calls(),
        0,
        "idle speech must not be transcribed when the fallback is off"
    );
    let snap = state.voice.mode_snapshot();
    let mic = &snap["devices"][0]["mic"]["counters"];
    assert_eq!(mic["asr_skipped"], json!(1), "the skip is counted: {mic}");
    assert_eq!(
        mic["ignored_unwoken"],
        json!(0),
        "nothing was transcribed, so nothing was ignored"
    );
    let log = state.voice.recent(200);
    assert!(
        log.iter().any(|e| e["kind"] == "mic_asr_skipped"),
        "the skip must be journaled with its reason: {log:?}"
    );

    // The setting is global and survives a snapshot round-trip.
    assert_eq!(snap["asr_wake_fallback"], json!(false));
    assert_eq!(
        hm_core::voice::VoiceSettings::default().asr_wake_fallback,
        true
    );
}

/// OFF does not break the wake path: a keyword hit opens the session and the
/// words that follow ARE transcribed and executed.
#[tokio::test]
async fn asr_wake_fallback_off_still_transcribes_after_a_keyword_wake() {
    let llm = FakeLlm::new(vec![]);
    let asr = CountingAsr::new("退下");
    let (_base, state, gw, _music) = spawn_app_with_gateway_media_asr(llm, asr.clone()).await;
    let _ = state
        .voice
        .set_asr_wake_fallback("dot2-d", false, "test")
        .unwrap();
    // The ack would hold the voice channel; this test is about the window.
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

    // The local spotter hears the wake phrase (nothing is playing): this
    // opens the window — no ASR involved.
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
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut seq = 1;
    for _ in 0..10 {
        gw.send(&audio_chunk(seq, &loud_block())).await;
        seq += 1;
    }
    for _ in 0..14 {
        gw.send(&audio_chunk(seq, &quiet_block())).await;
        seq += 1;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        asr.calls(),
        1,
        "an open session is transcribed even with the fallback off"
    );
    let snap = state.voice.mode_snapshot();
    assert_eq!(
        snap["devices"][0]["mic"]["counters"]["asr_skipped"],
        json!(0)
    );
}

/// ON (the default): the same idle segment IS transcribed, so the fallback
/// path is provably unchanged for existing devices.
#[tokio::test]
async fn asr_wake_fallback_on_transcribes_idle_speech() {
    let llm = FakeLlm::new(vec![]);
    let asr = CountingAsr::new("今天天气怎么样");
    let (_base, state, gw, _music) = spawn_app_with_gateway_media_asr(llm, asr.clone()).await;
    assert_eq!(
        state.voice.mode_snapshot()["asr_wake_fallback"],
        json!(true)
    );

    let mut seq = 1;
    for _ in 0..10 {
        gw.send(&audio_chunk(seq, &loud_block())).await;
        seq += 1;
    }
    for _ in 0..14 {
        gw.send(&audio_chunk(seq, &quiet_block())).await;
        seq += 1;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        asr.calls(),
        1,
        "the default transcribes idle speech for wake matching"
    );
    let snap = state.voice.mode_snapshot();
    let mic = &snap["devices"][0]["mic"]["counters"];
    assert_eq!(mic["asr_skipped"], json!(0));
    assert!(mic["ignored_unwoken"].as_u64().unwrap() >= 1, "{mic}");
}

// --- M6c-2: the output tone (user EQ) is configuration, pushed live --------

/// The saved tone must reach the device: on connect (so a reboot cannot keep
/// a stale curve) and on every save. The console card is only as honest as
/// this frame, so the test reads the wire.
#[tokio::test]
async fn the_output_tone_is_pushed_on_connect_and_on_save() {
    let llm = FakeLlm::new(vec![]);
    let (base, state, gw) = spawn_app_with_gateway(llm).await;

    // On connect: the persisted default (the sound chosen by ear).
    let f = gw.until(|f| matches!(f, CoreFrame::AudioEq { .. })).await;
    let CoreFrame::AudioEq { curve, bands, .. } = f else {
        unreachable!()
    };
    assert_eq!(curve, "echomuse");
    assert_eq!(bands.len(), 3, "the default taste EQ rides on the voicing");
    assert!((bands[0].freq - 175.0).abs() < 0.01 && (bands[0].gain_db + 6.0).abs() < 0.01);

    // A console save is applied and pushed.
    let put: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/audio"))
        .json(&json!({
            "curve": "echomuse",
            "bands": [
                {"freq": 200.0, "gain_db": -8.0, "q": 0.7},
                {"freq": 4000.0, "gain_db": 5.0, "q": 1.1}
            ]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put["state"], "applied", "{put}");
    assert_eq!(put["sent"], json!(["dot2-d"]));

    let f = gw.until(|f| matches!(f, CoreFrame::AudioEq { .. })).await;
    let CoreFrame::AudioEq { curve, bands, .. } = f else {
        unreachable!()
    };
    assert_eq!(curve, "echomuse");
    assert_eq!(bands.len(), 2);
    assert!((bands[0].freq - 200.0).abs() < 0.01);
    assert!((bands[1].gain_db - 5.0).abs() < 0.01);

    // The saved tone is what GET reports, and it survives a settings read.
    let got: Value = reqwest::get(format!("{base}/api/v0/config/audio"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["curve"], "echomuse");
    assert_eq!(got["bands"].as_array().unwrap().len(), 2);
    assert_eq!(state.config.audio_eq().unwrap().bands[0].freq, 200.0);

    // A curve outside the list is refused and nothing is pushed.
    let bad = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/audio"))
        .json(&json!({ "curve": "loud", "bands": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

/// reminder.set speaks the CORE-computed time (the tool's authoritative
/// line), and the row is visible — and still pending — on the management API
/// the console reads.
#[tokio::test]
async fn a_voice_set_reminder_speaks_the_resolved_time() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "reminder.set".into(),
                r#"{"note":"关火","delay_minutes":10}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["好的。".into()],
        },
    ]);
    let tts = Arc::new(RecordingTts::default());
    let (base, state, gw) = spawn_mic_app_stream_tts(llm, "unused", tts.clone()).await;

    explicit_request(&state, &gw, 1, "十分钟后提醒我关火").await;
    let spoken = streamed_answer(&gw, &tts).await;
    assert!(
        spoken.contains("提醒你关火"),
        "the tool's line is what is spoken: {spoken} | reminders: {} | tts: {:?}",
        reqwest::get(format!("{base}/api/v0/reminders"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        tts.said()
    );
    // The model must not be the one naming the time: the line names minutes,
    // not a clock the model computed.
    let v: Value = reqwest::get(format!("{base}/api/v0/reminders"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = v["reminders"].as_array().unwrap();
    assert_eq!(items.len(), 1, "one reminder row: {v}");
    assert_eq!(items[0]["state"], "pending");
    assert_eq!(items[0]["note"], "关火");
    assert_eq!(items[0]["device"], "dot2-d");
    assert!(items[0]["due_ms"].as_u64().unwrap() > hm_core::id::now_ms());
}

/// The firing sweep: due → spoken by its own speaker → marked fired, and a
/// line that could not be synthesized leaves the reminder PENDING (the
/// 2026-11-14 TTS outage must not eat reminders).
#[tokio::test]
async fn a_due_reminder_is_spoken_by_its_speaker_and_marked_fired() {
    let llm = FakeLlm::new(vec![]);
    let tts = Arc::new(RecordingTts::default());
    let (base, state, gw) = spawn_mic_app_stream_tts(llm, "unused", tts.clone()).await;
    let id = state
        .voice
        .test_only_schedule_reminder_due_soon("dot2-d", "测试提醒");

    // Give the line its play-finished receipt, like a real speaker would.
    match gw
        .until(|f| matches!(f, CoreFrame::Play { .. } | CoreFrame::PlayBegin { .. }))
        .await
    {
        CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } => {
            gw.send(&GatewayFrame::PlayFinished {
                device: "dot2-d".into(),
                task_id,
                played_ms: 1_200,
                consumed_ms: None,
                progress_basis: None,
                ts: hm_core::id::now_ms(),
            })
            .await;
        }
        _ => unreachable!("the sweep submits a whole-line Play"),
    }
    let mut fired = false;
    for _ in 0..60 {
        let log = state.voice.recent(200);
        if log
            .iter()
            .any(|e| e["kind"] == "reminder_fired" && e["id"] == id && e["marked"] == true)
        {
            fired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(fired, "the sweep must speak and mark the reminder");
    assert!(
        tts.said().iter().any(|t| t.contains("测试提醒")),
        "the reminder line names the note: {:?}",
        tts.said()
    );
    let v: Value = reqwest::get(format!("{base}/api/v0/reminders"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["reminders"][0]["state"], "fired", "{v}");
    assert!(
        v["reminders"][0]["fired_ms"].as_u64().unwrap()
            >= v["reminders"][0]["due_ms"].as_u64().unwrap()
    );
}

/// A synthesis outage does not consume the reminder: it stays pending and
/// the failure is named.
#[tokio::test]
async fn a_failed_synthesis_leaves_the_reminder_pending() {
    let llm = FakeLlm::new(vec![]);
    let (_base, state, _gw) = spawn_mic_app_stream_tts(llm, "unused", Arc::new(BrokenTts)).await;
    let id = state
        .voice
        .test_only_schedule_reminder_due_soon("dot2-d", "测试提醒");

    let mut failed = false;
    for _ in 0..60 {
        let log = state.voice.recent(200);
        if log
            .iter()
            .any(|e| e["kind"] == "reminder_speech_failed" && e["id"] == id)
        {
            failed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        failed,
        "the failed attempt must be named: {:?}",
        state.voice.recent(60)
    );
    let v: Value = reqwest::get(format!("{_base}/api/v0/reminders"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        v["reminders"][0]["state"], "pending",
        "the reminder survives: {v}"
    );
}

// --- interaction audit fixes (2026-11-14, codex findings 5/7/8/9) ----------

/// Codex finding 7: 「继续」 took the bookmark, a retreat landed before the
/// spawned continuation started, and the story played on after 退下 — the
/// task read a fresh epoch and believed it belonged to the new conversation.
#[tokio::test]
async fn a_continuation_started_after_a_retreat_does_not_play() {
    let answer = "第一段故事。第二段故事。第三段故事。第四段故事。";
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec![answer.into()],
    }]);
    let tts = Arc::new(RecordingTts::default());
    let (_base, state, gw) = spawn_mic_app_stream_tts(llm, "unused", tts.clone()).await;
    low_backstop(&state, 2);

    explicit_request(&state, &gw, 1, "讲个故事").await;
    let first = streamed_answer(&gw, &RecordingTts::default()).await;
    assert!(
        !first.contains("第三段"),
        "setup: the cap cut the answer: {first}"
    );
    assert!(
        !state.voice.mode_snapshot()["devices"][0]["program"]["paused"].is_null(),
        "setup: the cut left a bookmark"
    );

    // The retreat lands while the continuation is being set up.
    explicit_request(&state, &gw, 2, "退下").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let paused = state.voice.mode_snapshot()["devices"][0]["program"]["paused"].clone();
    assert!(paused.is_null(), "退下 must clear the bookmark: {paused}");
    assert!(
        tts.said()
            .iter()
            .all(|t| !t.contains("第三段") && !t.contains("第四段")),
        "nothing of the story may play after the retreat: {:?}",
        tts.said()
    );
}

/// Codex finding 8: a resume that could not take a bookmark must not report
/// `resumed: true`.
#[tokio::test]
async fn a_continue_without_a_bookmark_does_not_claim_a_resume() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["好的。".into()],
    }]);
    let (_base, state, gw) =
        spawn_mic_app_stream_tts(llm, "unused", Arc::new(RecordingTts::default())).await;
    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 2,
        keyword: "继续播放".into(),
        fed_ms: None,
        self_heard: false,
        rms_dbfs: None,
        lag_ms: None,
        bundle_id: None,
        kws_generation: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = state.voice.recent(100);
    assert!(
        log.iter().any(|e| e["kind"] == "keyword_ignored"
            && e["keyword"] == "继续播放"
            && e["reason"] == "reflex_precondition:no_bookmark"),
        "no bookmark = an honest refusal: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "reflex_continue"),
        "nothing may claim a resume: {log:?}"
    );
}

/// Codex finding 5: chunks dropped while our own voice was live must be
/// countable — an ignored interruption used to leave no trace.
#[tokio::test]
async fn chunks_dropped_during_our_own_voice_are_counted() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["这是一段比较长的回答，会持续一小会儿。".repeat(6)],
    }]);
    let tts = Arc::new(RecordingTts::default());
    let (_base, state, gw) = spawn_mic_app_stream_tts(llm, "unused", tts).await;
    explicit_request(&state, &gw, 1, "讲点什么").await;
    // Wait for the answer to be sounding, then push mic audio at it.
    let _ = gw
        .until(|f| matches!(f, CoreFrame::PlayBegin { .. } | CoreFrame::Play { .. }))
        .await;
    {
        use base64::Engine;
        let bytes: Vec<u8> = vec![1200i16; 480]
            .iter()
            .flat_map(|&s| s.to_le_bytes())
            .collect();
        let pcm = base64::engine::general_purpose::STANDARD.encode(bytes);
        for seq in 0..8u64 {
            gw.send(&GatewayFrame::AudioChunk {
                origin: hm_core::gateway::InputOrigin::Legacy,
                device: "dot2-d".into(),
                ts: hm_core::id::now_ms(),
                seq,
                pcm_b64: pcm.clone(),
                format: "s16le".into(),
                sample_rate: 16_000,
                channels: 1,
                layout: "mono".into(),
                vad_speech: None,
                vad_prob: None,
            })
            .await;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snap = state.voice.mode_snapshot();
    let n = snap["devices"][0]["mic"]["counters"]["suppressed_during_voice"]
        .as_u64()
        .unwrap_or(0);
    assert!(n > 0, "the voice gate must count what it dropped: {snap}");
}

/// Device 2026-11-14 19:07: the wake arrived as WORDS (`mic_wake`) instead of
/// a spotter hit; that path stopped the media outright, the session ended,
/// and 「下一首」 said twice was refused with `no_media`. A wake takes over,
/// it does not throw the queue away — both paths must pause.
#[tokio::test]
async fn a_transcript_wake_in_media_pauses_the_session_and_keeps_the_queue() {
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
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(state.music.sessions_json()["dot2-d"]["state"], "playing");

    // The wake comes in as a TRANSCRIPT (no spotter hit at all).
    explicit_request(&state, &gw, 2, "你好高达").await;
    let stopped = stops_within(&gw, Duration::from_millis(600)).await;
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
        "a wake that arrives as words must PAUSE, not end the session: {snap}"
    );
    assert_eq!(media["tracks"], 1, "the queue is kept: {media}");
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "wake_word_stop" && !e["media_paused"].is_null()),
        "the pause must be recorded on the wake fact: {log:?}"
    );

    // …and the window the wake opened can move the queue.
    explicit_request(&state, &gw, 3, "下一首").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let log = state.voice.recent(200);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "reflex_next" || e["kind"] == "media_next"),
        "「下一首」 must work after a transcript wake: {log:?}"
    );
    assert!(
        !log.iter().any(
            |e| e["kind"] == "keyword_ignored" && e["reason"] == "reflex_precondition:no_media"
        ),
        "no_media is the bug being fixed: {log:?}"
    );
}

/// 2026-11-18 (owner decision): an idle speaker with no active session and no
/// live/paused playback must stay silent when a retreat word ("退下吧" /
/// "停止播放") is spotted. Once a session is active (or playback is present),
/// the retreat word closes the session and speaks `retreat_ack`.
#[tokio::test]
async fn a_retreat_keyword_closes_an_idle_device_and_says_so() {
    let llm = FakeLlm::new(vec![]);
    let (base, state, gw) = spawn_app_with_gateway(llm).await;
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

    // 1. Idle: no session, no bookmark, nothing playing -> silent ignore.
    let before: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dev = before["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == "dot2-d")
        .unwrap();
    assert_eq!(dev["session"]["state"], "idle", "{before}");

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
    })
    .await;

    let idle_play = tokio::time::timeout(
        Duration::from_millis(300),
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
        idle_play.is_err(),
        "an idle retreat must NOT play the retreat ack: {idle_play:?}"
    );
    let log = state.voice.recent(100);
    assert!(
        log.iter().any(|e| e["kind"] == "keyword_ignored"
            && e["keyword"] == "停止播放"
            && e["reason"] == "reflex_precondition:no_session"),
        "idle retreat must record reflex_precondition:no_session: {log:?}"
    );
    assert!(
        !log.iter()
            .any(|e| e["kind"] == "reflex_retreat" || e["kind"] == "session_retreat"),
        "idle retreat must not emit reflex_retreat/session_retreat: {log:?}"
    );

    // 2. Active session: wake first, then retreat -> closes session and plays retreat_ack.
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
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 3,
        keyword: "停止播放".into(),
        fed_ms: None,
        self_heard: false,
        rms_dbfs: None,
        lag_ms: None,
        bundle_id: None,
        kws_generation: None,
    })
    .await;

    let play = tokio::time::timeout(
        Duration::from_secs(2),
        gw.until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("ack:")
            )
        }),
    )
    .await
    .expect("an active-session retreat must play the retreat ack");
    match play {
        CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. } => {
            assert!(content_ref.starts_with("ack:"), "got {content_ref}");
            assert_ne!(content_ref, "ack:", "an empty ack phrase is not an ack");
        }
        f => panic!("expected the ack play, got {f:?}"),
    }

    let log = state.voice.recent(100);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "reflex_retreat" && e["keyword"] == "停止播放"),
        "the keyword must execute the retreat when session is active: {log:?}"
    );
    let after: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dev = after["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == "dot2-d")
        .unwrap();
    assert_eq!(dev["session"]["state"], "idle", "closed back to idle");
}

/// Window policy uses the same saved snapshot for PUT, restore and backup import.
#[tokio::test]
async fn follow_window_config_roundtrips_history_restore_and_import() {
    let (base, state, gw) = spawn_app_with_gateway(FakeLlm::new(vec![])).await;
    let client = reqwest::Client::new();
    let mode_url = format!("{base}/api/v0/mode/dot2-d");
    let before = state.voice.settings_snapshot();
    assert!(before.follow_window_enabled);
    assert_eq!(before.follow_window_ms, 9_000);
    assert_eq!(before.followup_credits, None);
    let history_before = state.config.history(100).unwrap().len();
    for invalid in [
        json!({"follow_window_ms": 4_000}),
        json!({"follow_window_ms": 61_000}),
        json!({"followup_credits": 6}),
    ] {
        let mut mixed = invalid;
        mixed["asr_wake_fallback"] = json!(false);
        let result = client.put(&mode_url).json(&mixed).send().await.unwrap();
        assert_eq!(result.status(), 400);
        assert_eq!(state.voice.settings_snapshot().follow_window_ms, 9_000);
        assert!(
            state.voice.settings_snapshot().asr_wake_fallback,
            "invalid window must not partially change mode"
        );
    }
    assert_eq!(state.config.history(100).unwrap().len(), history_before);
    let patch = json!({"follow_window_enabled": false, "follow_window_ms": 5_000,
        "followup_credits": 1, "follow_window_tone": true});
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
    for (key, value) in patch.as_object().unwrap() {
        assert_eq!(&readback[key], value, "{key}");
    }
    let rows = state.config.history(100).unwrap();
    let row = rows
        .iter()
        .find(|r| r["scope"] == "voice" && r["payload"]["follow_window_enabled"] == false)
        .unwrap();
    let id = row["id"].as_i64().unwrap();
    assert_eq!(row["payload"]["follow_window_ms"], 5_000);
    assert_eq!(row["payload"]["followup_credits"], 1);
    let backup: Value = client
        .get(format!("{base}/api/v0/config/export"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(backup["mode"]["follow_window_ms"], 5_000);
    assert_eq!(backup["mode"]["followup_credits"], 1);
    assert_eq!(
        client
            .put(&mode_url)
            .json(&json!({"follow_window_enabled": true,
        "follow_window_ms": 60_000, "followup_credits": 5}))
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
    assert!(!state.voice.settings_snapshot().follow_window_enabled);
    assert_eq!(state.voice.settings_snapshot().follow_window_ms, 5_000);
    assert_eq!(state.voice.settings_snapshot().followup_credits, Some(1));
    // Use the real import path, without services, network calls or credentials.
    let imported = json!({"format": "home-mate-config", "version": 1, "mode": backup["mode"]});
    assert_eq!(
        client
            .post(format!("{base}/api/v0/config/import"))
            .json(&imported)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(state.voice.settings_snapshot().followup_credits, Some(1));
    // Fleet applies exactly this same snapshot/validator on its receiving side.
    let fleet_body: hm_core::api::ConfigImportBody = serde_json::from_value(imported).unwrap();
    hm_core::api::apply_config_import(&state, fleet_body, "fleet:test-source")
        .await
        .unwrap();
    assert!(state
        .config
        .history(100)
        .unwrap()
        .iter()
        .any(|r| r["actor"] == "fleet:test-source" && r["payload"]["follow_window_ms"] == 5_000));
    // D3: zero is valid for both management PUT and full-snapshot restore.
    assert_eq!(
        client
            .put(&mode_url)
            .json(&json!({"followup_credits": 0}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(state.voice.settings_snapshot().followup_credits, Some(0));
    let zero_restore = state.voice.settings_snapshot();
    assert_eq!(
        client
            .put(&mode_url)
            .json(&json!({"followup_credits": 5}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    state.voice.restore_settings(zero_restore, "test").unwrap();
    assert_eq!(state.voice.settings_snapshot().followup_credits, Some(0));
    explicit_request(&state, &gw, 1, "现在几点").await;
    let (play, meta) = gw.until_play("voice").await;
    assert!(
        meta["text"].as_str().unwrap().contains('点'),
        "zero followups must retain the first question"
    );
    d1_finish_voice(&gw, &play).await;
    let invalid_import = json!({"mode": {"follow_window_ms": 61_000}});
    assert_eq!(
        client
            .post(format!("{base}/api/v0/config/import"))
            .json(&invalid_import)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(state.voice.settings_snapshot().follow_window_ms, 5_000);
}

#[tokio::test]
async fn disabled_follow_window_still_admits_keyword_first_question_then_closes() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["山谷里有一只小猫。".into()],
    }]);
    let asr = CountingAsr::new("讲个故事");
    let (base, state, gw) = spawn_mic_app_asr(llm, asr.clone()).await;
    let response = reqwest::Client::new().put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({"follow_window_enabled": false, "asr_wake_fallback": false, "tail_blind_ms": 0}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
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
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut seq = 1;
    for _ in 0..10 {
        gw.send(&audio_chunk(seq, &loud_block())).await;
        seq += 1;
    }
    for _ in 0..14 {
        gw.send(&audio_chunk(seq, &quiet_block())).await;
        seq += 1;
    }
    let (play, _) = gw.until_play("voice").await;
    gw.confirm_play(&play).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(asr.calls(), 1, "first question must reach ASR");
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: play,
        played_ms: 1_000,
        consumed_ms: Some(1_000),
        progress_basis: Some("hw_delay".into()),
        ts: hm_core::id::now_ms(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mode = state.voice.mode_snapshot();
    assert_eq!(mode["devices"][0]["session"]["state"], "idle", "{mode}");
    assert_eq!(mode["devices"][0]["participation"], "idle");
    for _ in 0..10 {
        gw.send(&audio_chunk(seq, &loud_block())).await;
        seq += 1;
    }
    for _ in 0..14 {
        gw.send(&audio_chunk(seq, &quiet_block())).await;
        seq += 1;
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        asr.calls(),
        1,
        "disabled window must not admit an unwoken follow-up"
    );
}

#[tokio::test]
async fn disabled_follow_window_stop_during_thinking_does_not_leave_a_running_window() {
    let llm = FakeLlm::new(vec![Scripted::Delayed {
        delay_ms: 5_000,
        chunks: vec!["晚了".into()],
    }]);
    let (base, state, gw) = spawn_mic_app(llm, "你好高达").await;
    assert_eq!(
        reqwest::Client::new()
            .put(format!("{base}/api/v0/mode/dot2-d"))
            .json(&json!({"follow_window_enabled": false}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    explicit_request(&state, &gw, 1, "讲个故事").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!state.voice.mode_snapshot()["devices"][0]["current_task"].is_null());
    state.voice.stop_device("dot2-d");
    let mode = state.voice.mode_snapshot();
    assert_eq!(mode["devices"][0]["participation"], "idle", "{mode}");
    assert_eq!(mode["devices"][0]["session"]["state"], "idle");
}

#[tokio::test]
async fn disabled_follow_window_resumed_answer_closes_after_playback() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["山谷里有一只小猫。".into()],
    }]);
    let (base, state, gw) = spawn_mic_app(llm, "你好高达").await;
    explicit_request(&state, &gw, 1, "讲个故事").await;
    let (first, _) = gw.until_play("voice").await;
    gw.confirm_play(&first).await;
    assert!(state.voice.pause_program("dot2-d", "test"));
    for play in stops_within(&gw, Duration::from_millis(300)).await {
        gw.confirm_stop(&play).await;
    }
    assert_eq!(
        reqwest::Client::new()
            .put(format!("{base}/api/v0/mode/dot2-d"))
            .json(&json!({"follow_window_enabled": false}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    // A new activation resets the first-question marker; continue is the first command.
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
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(state.voice.resume_program("dot2-d"));
    let (resumed, _) = gw.until_play("voice").await;
    gw.confirm_play(&resumed).await;
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: resumed,
        played_ms: 1_000,
        consumed_ms: Some(1_000),
        progress_basis: Some("hw_delay".into()),
        ts: hm_core::id::now_ms(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mode = state.voice.mode_snapshot();
    assert_eq!(mode["devices"][0]["participation"], "idle", "{mode}");
    assert_eq!(mode["devices"][0]["session"]["state"], "idle");
}

#[tokio::test]
async fn disabled_follow_window_disconnect_during_thinking_closes() {
    let llm = FakeLlm::new(vec![Scripted::Delayed {
        delay_ms: 5_000,
        chunks: vec!["晚了".into()],
    }]);
    let (base, state, gw) = spawn_mic_app(llm, "你好高达").await;
    assert_eq!(
        reqwest::Client::new()
            .put(format!("{base}/api/v0/mode/dot2-d"))
            .json(&json!({"follow_window_enabled": false}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    explicit_request(&state, &gw, 1, "讲个故事").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!state.voice.mode_snapshot()["devices"][0]["current_task"].is_null());
    gw.close().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mode = state.voice.mode_snapshot();
    assert_eq!(mode["devices"][0]["participation"], "idle", "{mode}");
}
