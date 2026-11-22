use super::*;

/// Diagnostic mic capture: the WAV the management API hands back is the same
/// post-AEC mono the SPOTTER consumes (the gateway feeds the spotter exactly
/// what it sends north), so a KWS hit-rate measurement can use it directly.
#[tokio::test]
async fn a_mic_capture_returns_the_spotter_input_as_wav() {
    let llm = FakeLlm::new(vec![]);
    let (base, _state, gw) = spawn_mic_app(llm, "忽略").await;
    let client = reqwest::Client::new();
    let started: Value = client
        .post(format!("{base}/api/v0/devices/dot2-d/mic/capture"))
        .json(&json!({ "seconds": 1 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(started["state"], "recording", "{started}");

    // 13 × 80 ms = 1.04 s, more than the requested second.
    for seq in 1..=13u64 {
        gw.send(&audio_chunk(seq, &loud_block())).await;
    }
    let mut status = Value::Null;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        status = client
            .get(format!("{base}/api/v0/devices/dot2-d/mic/capture"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if status["state"] == "done" {
            break;
        }
    }
    assert_eq!(status["state"], "done", "{status}");
    assert_eq!(status["rate"], 16000);

    let wav = client
        .get(format!("{base}/api/v0/devices/dot2-d/mic/capture.wav"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(
        wav.len() > 44 + 16_000,
        "about a second of PCM: {}",
        wav.len()
    );
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    let data = &wav[44..];
    let peak = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]).unsigned_abs())
        .max()
        .unwrap_or(0);
    assert!(
        peak > 8000,
        "the fed tone must be in the file (peak {peak})"
    );
}

/// Uplink audio closes a VAD segment → ASR → pinyin wake match → the
/// stripped remainder runs a turn → answer spoken on the voice channel.
#[tokio::test]
async fn uplink_audio_wakes_and_runs_turn() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["在呢，朋友。".into()],
    }]);
    let (base, state, gw) = spawn_mic_app(llm, "你好高达，讲个故事").await;

    // The core must ask for the mic once the audio_in device joins.
    match gw
        .until(|f| matches!(f, CoreFrame::MicSession { action, .. } if action == "start"))
        .await
    {
        CoreFrame::MicSession { device, mode, .. } => {
            assert_eq!(device, "dot2-d");
            assert_eq!(mode.as_deref(), Some("continuous"));
        }
        f => panic!("expected mic_session start, got {f:?}"),
    }

    // Speech (10 blocks) then silence (hangover closes the segment).
    let loud = loud_block();
    let quiet = quiet_block();
    for seq in 1..=10 {
        gw.send(&audio_chunk(seq, &loud)).await;
    }
    for seq in 11..=20 {
        gw.send(&audio_chunk(seq, &quiet)).await;
    }

    let (play_id, meta) = gw.until_play("voice").await;
    assert_eq!(meta["text"].as_str(), Some("在呢，朋友。"));
    gw.confirm_play(&play_id).await;

    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = state.voice.recent(100);
    assert!(
        log.iter().any(|e| e["kind"] == "mic_wake"),
        "wake must be logged: {log:?}"
    );
    assert!(log
        .iter()
        .any(|e| e["kind"] == "mic_utterance" || e["kind"] == "mic_segment_closed"));

    // The turn ran with the wake word STRIPPED and the asr source recorded.
    let traces: Value = reqwest::get(format!("{base}/api/v0/trace?limit=5"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let t = &traces["traces"][0];
    assert_eq!(
        t["input_text"], "讲个故事",
        "wake word must be stripped from the turn input"
    );
    assert_eq!(t["source"], "device:asr");
    assert_eq!(t["wake_channel"], "asr");
    assert_eq!(t["wake_word"], "你好高达");
}

/// Malformed audio_chunk fields must be recorded and DROPPED — never a
/// panic (review P2-1: channels=0 once divided by zero and poisoned the
/// hub lock, taking the whole service down). The link stays usable after.
#[tokio::test]
async fn malformed_audio_chunk_is_recorded_not_fatal() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["还在。".into()],
    }]);
    let (base, _state, gw) = spawn_mic_app(llm, "你好高达").await;

    // channels=0 (would panic a modulo) and a truncated stereo payload.
    use base64::Engine;
    for bad in [
        json!({ "type": "audio_chunk", "device": "dot2-d", "ts": 1, "seq": 1,
                "pcm_b64": base64::engine::general_purpose::STANDARD.encode(vec![0u8; 2560]),
                "format": "s16le", "sample_rate": 16000, "channels": 0, "layout": "mono" }),
        json!({ "type": "audio_chunk", "device": "dot2-d", "ts": 2, "seq": 2,
                "pcm_b64": base64::engine::general_purpose::STANDARD.encode(vec![0u8; 2562]),
                "format": "s16le", "sample_rate": 16000, "channels": 2, "layout": "mic_ref" }),
    ] {
        gw.send_raw_json(bad).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The core survived: the anomaly is in the diagnostic ring...
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
            .any(|r| r["kind"] == "audio_chunk_bad_channels"),
        "channels=0 must be recorded: {recent:?}"
    );
    assert!(
        recent
            .iter()
            .any(|r| r["kind"] == "audio_chunk_bad_alignment"),
        "odd stereo payload must be recorded"
    );

    // ...and normal traffic still turns (device online, text utterance runs).
    explicit_request(&_state, &gw, 10, "你好").await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;
}

/// Uplink audio WITHOUT the wake word (idle, no conversation window) is
/// transcribed for matching but never answered: no turn, no play.
#[tokio::test]
async fn uplink_audio_without_wake_is_ignored() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["不该说话".into()],
    }]);
    let (_base, state, gw) = spawn_mic_app(llm, "今天天气不错").await;

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
    tokio::time::sleep(Duration::from_millis(700)).await;

    let log = state.voice.recent(100);
    assert!(
        log.iter().any(|e| e["kind"] == "mic_ignored_unwoken"),
        "must be logged as ignored: {log:?}"
    );
    // No play was ever submitted.
    let drained = gw.rx.lock().await.try_recv();
    if let Ok(f) = drained {
        assert!(
            !matches!(f, CoreFrame::Play { .. }),
            "no play may be submitted for unwoken speech, got {f:?}"
        );
    }
}

/// v0.2 device_status introduces a device mid-connection: mic starts, the
/// snapshot shows it online, and offline/online toggles are honest.
#[tokio::test]
async fn device_status_introduces_and_toggles_device() {
    let llm = FakeLlm::new(vec![]);
    let (base, _state, gw) = spawn_mic_app(llm, "你好高达").await;

    // Introduce via device_status (hello announced nothing for this device).
    gw.send(&GatewayFrame::DeviceStatus {
        northbound: None,
        device: "dot2-d".into(),
        online: true,
        reason: "registered".into(),
        descriptor: Some(HelloDevice {
            stable_id: "dot2-d".into(),
            model: "AEOBC".into(),
            firmware: Some("emuse-2.19".into()),
            capabilities: vec![
                "audio_in".into(),
                "audio_out".into(),
                "volume".into(),
                "stop".into(),
            ],
            audio_profile: "fireos_builtin_speaker".into(),
            wifi_ssid: None,
        }),
    })
    .await;
    let _ = gw
        .until(|f| matches!(f, CoreFrame::MicSession { device, .. } if device == "dot2-d"))
        .await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    let devices: Value = reqwest::get(format!("{base}/api/v0/devices"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = devices["gateway"]["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["stable_id"] == "dot2-d")
        .expect("device introduced by status");
    assert_eq!(entry["online"], true);

    // Offline: slot flips, honest in the snapshot.
    gw.send(&GatewayFrame::DeviceStatus {
        northbound: None,
        device: "dot2-d".into(),
        online: false,
        reason: "control_lost".into(),
        descriptor: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let devices: Value = reqwest::get(format!("{base}/api/v0/devices"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = devices["gateway"]["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["stable_id"] == "dot2-d")
        .unwrap();
    assert_eq!(entry["online"], false);
}

/// M6b (S3): every turn ends with an OBSERVED timeline — first model byte,
/// first/last answer audio, how much the device played, and whether a stop
/// cut it. A turn that never made audio must say null, not zero.
#[tokio::test]
async fn turn_end_carries_the_observed_timeline() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["你好，我是高达。".into()],
    }]);
    let (base, state, gw) = spawn_app_with_gateway(llm).await;
    // Read the in-memory RING, not the journal: every pipeline in this test
    // process writes to the same test data dir, and an export of the shared
    // journal would parse megabytes per poll. That turn_end IS exportable is
    // covered by `voice_events_journal_exports_past_the_ring` (isolated dir).
    let epoch = state.voice.mode_snapshot()["devices"][0]["kws"]["core_epoch"]
        .as_u64()
        .expect("core epoch");

    explicit_request(&state, &gw, 1, "你是谁").await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;
    // The device's terminal receipt: 1.2 s written, 1.1 s consumed by the DAC.
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: play_id.clone(),
        played_ms: 1_200,
        consumed_ms: Some(1_100),
        progress_basis: Some("hw_delay".into()),
        ts: hm_core::id::now_ms(),
    })
    .await;

    // Wait for the turn's own terminal (the ring holds the last 100).
    let mut end = Value::Null;
    for _ in 0..100 {
        if let Some(e) = ring_event(&state, epoch, "turn_end") {
            end = e;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(end.is_object(), "turn_end must be recorded");
    let tl = &end["timeline"];
    assert!(tl.is_object(), "turn_end carries a timeline: {end}");
    assert!(
        tl["first_byte_ms"].as_u64().is_some(),
        "the model produced a token: {tl}"
    );
    assert!(
        tl["first_audio_ms"].as_u64().is_some(),
        "the answer reached the device: {tl}"
    );
    // The whole path submits without waiting for the terminal, so the
    // snapshot at turn_end is either the finished numbers or an honest
    // "still draining" flag — never a fabricated end.
    let live = tl["audio_live_at_end"].as_bool().unwrap_or(false);
    let played = tl["played_ms"].as_u64().unwrap();
    assert!(
        live || played == 1_200,
        "turn_end must not invent an end: {tl}"
    );
    assert_eq!(tl["answer_plays"].as_u64(), Some(if live { 0 } else { 1 }));
    assert!(tl["stop"].is_null(), "nothing stopped this turn: {tl}");
    let first = tl["first_audio_ms"].as_u64().unwrap();
    let last = tl["last_audio_ms"].as_u64().unwrap();
    assert!(last >= first, "last audio cannot precede the first: {tl}");

    // The audio-end fact is a separate event (join by task): it is the ONLY
    // place the device's full playback is known for a turn that outlived
    // its record.
    let mut audio_end = None;
    for _ in 0..100 {
        if let Some(e) = ring_event(&state, epoch, "turn_audio_end") {
            audio_end = Some(e);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let audio_end = audio_end.expect("turn_audio_end must be recorded");
    assert_eq!(audio_end["played_ms"].as_u64(), Some(1_200));
    assert_eq!(audio_end["task"], end["task"]);
}

/// Device utterance → brain turn (fake model) → TTS (mock) → voice play on
/// the gateway, confirmed by receipt; the device session/trace are recorded.
#[tokio::test]
async fn device_turn_speaks_answer_via_gateway() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["你好，我是高达。".into()],
    }]);
    let (base, _state, gw) = spawn_app_with_gateway(llm).await;

    explicit_request(&_state, &gw, 1, "你是谁").await;
    let (play_id, meta) = gw.until_play("voice").await;
    assert_eq!(meta["text"].as_str(), Some("你好，我是高达。"));
    assert_eq!(meta["has_audio"], true, "mock TTS WAV must be inline");
    // R2-BM5/BM5-6: the live output record carries its class, so a receipt
    // can no longer pass an answer off as a cue (or the reverse).
    let mode: Value = reqwest::get(format!("{base}/api/v0/mode"))
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
        outs.iter()
            .any(|o| o["kind"] == "answer" && o["channel"] == "voice"),
        "the spoken answer must be kind=answer: {outs:?}"
    );
    gw.confirm_play(&play_id).await;

    // Wait for the pipeline to record the turn, then verify persistence.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let devices: Value = reqwest::get(format!("{base}/api/v0/devices"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(devices["gateway"]["connected"], true);
    assert_eq!(devices["gateway"]["devices"][0]["stable_id"], "dot2-d");

    let traces: Value = reqwest::get(format!("{base}/api/v0/trace?limit=5"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let t = &traces["traces"][0];
    assert_eq!(
        t["source"], "device:text",
        "trace must show the device origin"
    );
    assert_eq!(t["status"], "completed");

    // Session for the device persists under a stable id.
    let msgs: Value = reqwest::get(format!("{base}/api/v0/session/dev:dot2-d/messages"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        msgs["messages"]
            .as_array()
            .map(|m| m.len() >= 2)
            .unwrap_or(false),
        "user + answer persisted"
    );
}

/// Stale seq (late/replayed event from an old connection) is recorded, not
/// acted on: no second turn, no second play.
#[tokio::test]
async fn stale_seq_events_are_recorded_not_acted_on() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["一次就好".into()],
    }]);
    let (base, _state, gw) = spawn_app_with_gateway(llm).await;

    explicit_request(&_state, &gw, 7, "讲个故事").await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;

    // A replayed/lower seq arrives: recorded only.
    say(&gw, 3, "旧事件").await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let devices: Value = reqwest::get(format!("{base}/api/v0/devices"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let recent = devices["recent"].as_array().unwrap();
    assert!(
        recent.iter().any(|r| r["kind"] == "stale_event"),
        "stale event must land in the diagnostic feed"
    );

    let traces: Value = reqwest::get(format!("{base}/api/v0/trace?limit=10"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let count = traces["traces"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["input_text"] == "旧事件")
        .count();
    assert_eq!(count, 0, "stale utterance must not run a turn");
}

/// music.play routes through the gateway: the song is a STREAMED play on the
/// media channel (decoded PCM, device's own rate), the spoken answer lands on
/// voice, and the service is the real one (fake Subsonic, real protocol).
#[tokio::test]
async fn music_play_routes_through_gateway_media_channel() {
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
    let (_base, _state, gw, music) = spawn_app_with_gateway_media(llm).await;

    explicit_request(&_state, &gw, 1, "播放第一名的歌").await;
    let (media_id, text) = gw.until_media_play().await;
    assert!(
        text.contains("晚风信笺"),
        "the media play says what it is: {text}"
    );
    assert_eq!(
        music.count("stream:t01"),
        1,
        "the song was fetched once, from the library"
    );

    // The song is a stream: chunks arrive after the PlayBegin, and the stream
    // is closed (PlayEnd) when the decoded audio runs out.
    gw.until(|f| matches!(f, CoreFrame::PlayChunk { .. })).await;
    gw.until(|f| matches!(f, CoreFrame::PlayEnd { .. })).await;
    let _ = media_id;

    let (voice_id, _) = gw.until_any_voice_play().await;
    gw.confirm_play(&voice_id).await;
}

/// Stop is honest: the stop request goes out, the receipt confirms, and the
/// pipeline's output record disappears.
#[tokio::test]
async fn stop_device_stops_outputs_and_cancels_task() {
    // Delayed answer keeps the task running long enough to be cancelled.
    let llm = FakeLlm::new(vec![Scripted::Delayed {
        delay_ms: 1500,
        chunks: vec!["慢慢想".into()],
    }]);
    let (base, _state, gw) = spawn_app_with_gateway(llm).await;

    explicit_request(&_state, &gw, 1, "想久一点").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let stop: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/devices/dot2-d/stop"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        stop["task_cancel_requested"], true,
        "running task must be cancelled"
    );

    // Cancellation is cooperative: the loop observes the flag when its
    // in-flight model call yields. Wait it out, then assert honestly.
    let mut status = String::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let tasks: Value = reqwest::get(format!("{base}/api/v0/tasks"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let any_running = tasks["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["status"] == "running");
        if !any_running {
            status = tasks["tasks"][0]["status"].as_str().unwrap().to_string();
            break;
        }
    }
    assert_eq!(status, "cancelled", "task must end cancelled, not running");
}

/// Full-duplex request on a half-only device: requested recorded, negotiated
/// stays half — visible in the mode snapshot with the honest reason.
#[tokio::test]
async fn mode_downgrade_is_honest() {
    let llm = FakeLlm::new(vec![]);
    let (base, _state, gw) = spawn_app_with_gateway(llm).await;

    let _put: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({ "duplex_requested": "full", "proactive_speech": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // The gateway receives the join-time mode_set (persisted default) and
    // then the PUT-driven one; assert the FULL request reaches the wire.
    let frame = gw
        .until(|f| matches!(f, CoreFrame::ModeSet { duplex_requested, .. } if duplex_requested == "full"))
        .await;
    assert!(matches!(frame, CoreFrame::ModeSet { .. }));
    gw.send(&GatewayFrame::ModeReport {
        device: "dot2-d".into(),
        duplex: hm_core::gateway::DuplexState {
            capability: "half".into(),
            requested: "full".into(),
            negotiated: "half".into(),
            effective: "half".into(),
        },
        aec: hm_core::gateway::AecState {
            owner: "core".into(),
            reference: "available".into(),
        },
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mode: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let d = &mode["devices"][0];
    assert_eq!(d["duplex"]["requested"], "full");
    assert_eq!(
        d["duplex"]["negotiated"], "half",
        "must not fake full on a half link"
    );
    assert_eq!(d["duplex"]["effective"], "half");

    // Invalid values are rejected.
    let bad = reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({ "duplex_requested": "telepathy" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

/// Late receipts (play_id nobody waits for) land in the diagnostic feed and
/// change nothing.
#[tokio::test]
async fn late_receipt_is_recorded_only() {
    let llm = FakeLlm::new(vec![]);
    let (base, _state, gw) = spawn_app_with_gateway(llm).await;
    gw.send(&GatewayFrame::PlayReceipt {
        task_id: "P-late-404".into(),
        stage: "device_confirmed".into(),
        detail: Some("from a previous generation".into()),
        ts: hm_core::id::now_ms(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let devices: Value = reqwest::get(format!("{base}/api/v0/devices"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let recent = devices["recent"].as_array().unwrap();
    assert!(recent
        .iter()
        .any(|r| r["kind"] == "play_receipt" && r["play_id"] == "P-late-404"));
    let mode: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let outputs = mode["devices"][0]["outputs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        outputs.is_empty(),
        "late receipt must not create an output record"
    );
}

/// Reconnect: generation bumps, the device is online again, and a fresh
/// utterance still works.
#[tokio::test]
async fn reconnect_bumps_generation_and_recovers() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["恢复了".into()],
    }]);
    let (base, _state, gw) = spawn_app_with_gateway(llm).await;

    let before: Value = reqwest::get(format!("{base}/api/v0/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gen_before = before["gateway"]["generation"].as_u64().unwrap();

    // Drop the connection (drops write half + reader).
    let gw2 = gw.clone();
    drop(gw2);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let gw = {
        // Reconnect on the same port: fetch it from the snapshot.
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
    let after: Value = reqwest::get(format!("{base}/api/v0/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(after["gateway"]["generation"].as_u64().unwrap() > gen_before);
    assert_eq!(after["gateway"]["devices"][0]["online"], true);

    explicit_request(&_state, &gw, 1, "还在吗").await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;
}

/// Volume goes through the same management path; the receipt updates the log.
#[tokio::test]
async fn volume_set_round_trip() {
    let llm = FakeLlm::new(vec![]);
    let (base, _state, gw) = spawn_app_with_gateway(llm).await;

    let v: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/devices/dot2-d/volume"))
        .json(&json!({ "value": 0.4 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["state"], "submitted");

    let frame = gw.until(|f| matches!(f, CoreFrame::VolumeSet { .. })).await;
    match frame {
        CoreFrame::VolumeSet { value, .. } => assert_eq!(value, 0.4),
        f => panic!("{f:?}"),
    }

    let bad = reqwest::Client::new()
        .post(format!("{base}/api/v0/devices/dot2-d/volume"))
        .json(&json!({ "value": 1.5 }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400, "out-of-range volume rejected at the API");
}

/// No output device → music.play FAILS honestly. There is no simulated sink
/// any more (R1e): "played" is a device fact, and a household with no speaker
/// must not be told a silent story.
#[tokio::test]
async fn music_play_without_a_device_says_so() {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(FakeLlm::new(vec![
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
                chunks: vec!["没有可用的音箱，放不了。".into()],
            },
        ]))),
        Some(Arc::new(MockTts::default())),
        None,
        None,
    );
    let fake = common::spawn_fake_music().await;
    common::configure_music(&state.store, &fake, true, false, true, true);
    enable_follow_window(&state);
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/v0/chat"))
        .json(&json!({ "text": "放首歌" }))
        .send()
        .await
        .unwrap();
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut events = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim().to_string();
            buf.drain(..=nl);
            if let Some(payload) = line.strip_prefix("data:") {
                if let Ok(v) = serde_json::from_str::<Value>(payload.trim()) {
                    events.push(v);
                }
            }
        }
    }
    let tool = events
        .iter()
        .find(|e| e["type"] == "tool_result" && e["tool"] == "music.play")
        .expect("music.play result event");
    assert_eq!(tool["status"], "failed", "no device = no play: {tool:?}");
    assert_eq!(tool["evidence"], "observed");
    assert!(
        tool["summary"].as_str().unwrap_or("").contains("NO_DEVICE"),
        "the honest code reaches the stream: {tool:?}"
    );
    // Nothing was even fetched: the failure happens before the stream opens.
    assert_eq!(fake.count("stream:"), 0);
    let answer = events
        .iter()
        .find(|e| e["type"] == "answer")
        .expect("final answer");
    assert!(answer["text"].as_str().unwrap().contains("放不了"));
}

/// Barge-in: a second utterance while a voice play is still running stops
/// the playback (Stop frame on the wire) and cancels the old task.
#[tokio::test]
async fn barge_in_stops_active_play_and_cancels_task() {
    // First answer is long enough that its TTS play is still running when
    // the second utterance lands; second answer finishes the new turn.
    let long_answer = "这是".to_string() + &"很长".repeat(400); // ~12 s of audio
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec![long_answer],
        },
        Scripted::Text {
            chunks: vec!["好的".into()],
        },
    ]);
    let (_base, _state, gw) = spawn_app_with_gateway(llm).await;

    explicit_request(&_state, &gw, 1, "讲个长故事").await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;

    // The play is running (mock TTS made a long clip). A new utterance
    // must trigger a Stop for it on the wire.
    explicit_request(&_state, &gw, 2, "停，换个短的").await;
    let stop = gw
        .until(|f| matches!(f, CoreFrame::Stop { task_id, .. } if task_id == &play_id))
        .await;
    assert!(matches!(stop, CoreFrame::Stop { flush: true, .. }));

    // The gateway confirms the stop (device-side truth), then the new turn
    // produces its own short voice play.
    gw.send(&GatewayFrame::StopReceipt {
        task_id: play_id.clone(),
        stage: "device_confirmed".into(),
        played_ms: Some(0),

        consumed_ms: None,
        progress_basis: None,
        detail: None,
    })
    .await;
    let (play2, _) = gw.until_play("voice").await;
    gw.confirm_play(&play2).await;
}

/// Unconfirmed play: a gateway that never sends a terminal receipt makes
/// music.play report unknown after the bounded budget — never success.
#[tokio::test]
async fn unconfirmed_gateway_play_is_unknown() {
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
            chunks: vec!["状态我不确定。".into()],
        },
    ]);
    let (base, _state, gw, _music) = spawn_app_with_gateway_media(llm).await;

    explicit_request(&_state, &gw, 1, "放第一名的歌").await;
    // Receive the media play but NEVER confirm it.
    let (media_id, _) = gw.until_media_play_opts(false).await;
    let _ = media_id;

    // The turn's answer eventually completes; the tool result must be
    // unknown, and the trace records it with the honest evidence level.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let traces: Value = reqwest::get(format!("{base}/api/v0/trace?limit=5"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let done = traces["traces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["input_text"] == "放第一名的歌" && t["status"] != "running");
        if done {
            let d = reqwest::get(format!(
                "{base}/api/v0/trace/{}",
                traces["traces"][0]["id"].as_str().unwrap()
            ))
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
            let play = d["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["kind"] == "tool_result" && e["payload"]["tool"] == "music.play")
                .expect("music.play in trace");
            assert_eq!(
                play["payload"]["evidence"], "Unknown",
                "unconfirmed play must stay unknown"
            );
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "turn did not finish in time"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Real EOF teardown (not supersede): the gateway drops the socket; devices
/// go offline, and a fresh connection restores service with a new generation.
#[tokio::test]
async fn eof_disconnect_then_reconnect() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["第一轮".into()],
        },
        Scripted::Text {
            chunks: vec!["第二轮".into()],
        },
    ]);
    let (base, _state, gw) = spawn_app_with_gateway(llm).await;

    explicit_request(&_state, &gw, 1, "你好").await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;

    let status: Value = reqwest::get(format!("{base}/api/v0/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gen1 = status["gateway"]["generation"].as_u64().unwrap();
    assert_eq!(status["gateway"]["devices"][0]["online"], true);

    // Gateway drops the socket entirely.
    gw.close().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let status: Value = reqwest::get(format!("{base}/api/v0/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        status["gateway"]["connected"], false,
        "EOF must mark the gateway offline"
    );
    assert_eq!(
        status["gateway"]["devices"][0]["online"], false,
        "device must go offline on EOF"
    );

    // Reconnect and finish another turn.
    let port: u16 = status["gateway"]["bind_addr"]
        .as_str()
        .unwrap()
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let gw2 = connect_fake_gateway(port).await;
    let status: Value = reqwest::get(format!("{base}/api/v0/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(status["gateway"]["generation"].as_u64().unwrap() > gen1);

    explicit_request(&_state, &gw2, 1, "还在吗").await;
    let (play2, _) = gw2.until_play("voice").await;
    gw2.confirm_play(&play2).await;
}

/// R1f: a TTS failure caused by OUR OWN cancel/stop is expected, not an
/// error to show. The user saw the ring flash red after 退下; only a turn
/// that is still valid may paint `error`.
#[tokio::test]
async fn a_cancelled_tts_failure_does_not_flash_the_ring_red() {
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let calls = Arc::new(std::sync::Mutex::new(0usize));
    let tts = Arc::new(FailingGatedTts {
        gate: gate_rx,
        calls: calls.clone(),
    });
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["这是一段回答。".into()],
    }]);
    let (_base, state, gw) = spawn_mic_app_stream_tts(llm, "", tts).await;
    explicit_request(&state, &gw, 1, "讲个故事").await;

    // The turn is alive and parked inside synthesis.
    let mut task_id = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(30)).await;
        if *calls.lock().unwrap() > 0 {
            task_id = state.voice.mode_snapshot()["devices"][0]["current_task"]
                .as_str()
                .map(str::to_string);
            if task_id.is_some() {
                break;
            }
        }
    }
    task_id.expect("the turn must be current and synthesizing");
    // The console's stop: bumps the delivery epoch (request_cancel cannot
    // flag a task whose model call already finished, which is exactly this
    // case — the epoch is the authoritative signal).
    state.voice.stop_device("dot2-d");
    gate_tx.send(true).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;

    let log = state.voice.recent(400);
    let failed = log
        .iter()
        .find(|e| e["kind"] == "tts_failed")
        .unwrap_or_else(|| panic!("tts_failed must be recorded: {log:?}"));
    assert_eq!(
        failed["cancelled"], true,
        "the failure belongs to a cancelled turn: {failed:?}"
    );
    let states: Vec<&str> = log
        .iter()
        .filter(|e| e["kind"] == "assistant_state")
        .filter_map(|e| e["state"].as_str())
        .collect();
    assert!(
        states.first() != Some(&"error"),
        "a cancelled failure must not paint the ring red, saw {states:?}"
    );
}

/// Reproduces the user-reported race:
///   1. turn 1 answers ("旧答案-AAA"); its TTS synthesis PARKS on a gate —
///      the model is done (task already Completed) but no play was sent;
///   2. the user shouts stop through the DEVICE entry (action button);
///   3. a new utterance starts turn 2 ("新答案-BBB"), its synthesis parks;
///   4. only then is the OLD synthesis released (returns after the new
///      round began), and finally the new one.
///
/// Acceptance: zero play commands for the old answer; the new answer plays
/// exactly once with the right text; the old turn's TurnEnd does not clear
/// the new turn's state (current_task still belongs to turn 2 right after
/// the old synthesis returns).
#[tokio::test]
async fn stop_during_pending_synthesis_suppresses_old_answer() {
    const OLD: &str = "旧答案-AAA";
    const NEW: &str = "新答案-BBB";

    let (old_gate_tx, old_gate_rx) = tokio::sync::watch::channel(false);
    let (new_gate_tx, new_gate_rx) = tokio::sync::watch::channel(false);
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tts = Arc::new(GatedTts {
        gates: vec![(OLD, old_gate_rx), (NEW, new_gate_rx)],
        calls: calls.clone(),
    });
    let tts_calls = calls.clone();

    let (base, _state, gw) = spawn_app_with_tts(
        FakeLlm::new(vec![
            Scripted::Text {
                chunks: vec![OLD.to_string()],
            },
            Scripted::Text {
                chunks: vec![NEW.to_string()],
            },
        ]),
        tts as Arc<dyn hm_core::speech::TtsClient>,
    )
    .await;

    // 1) Turn 1: answer synthesized → parked. Model done = trace not running.
    explicit_request(&_state, &gw, 1, "第一句").await;
    wait_until_sync("tts call 1 started", || {
        tts_calls.lock().unwrap().len() >= 1
    })
    .await;

    // 2) Stop through the NORMAL DEVICE entry (action button).
    gw.send(&GatewayFrame::Button {
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 2,
        action: "action_press".into(),
    })
    .await;
    // 3) Confirm the system processed the stop before continuing.
    //    (model-done check folded here: the task already left "running"
    //    BEFORE the stop, per the trace list)
    {
        let base = base.clone();
        wait_until_some("stop processed", move || {
            let base = base.clone();
            Box::pin(async move {
                let t: Value = reqwest::get(format!("{base}/api/v0/trace?limit=5"))
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                let done = t["traces"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|x| x["input_text"] == "第一句" && x["status"] != "running");
                if !done {
                    return None;
                }
                let d: Value = reqwest::get(format!("{base}/api/v0/voice/log"))
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                let stopped = d["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["kind"] == "stop_result");
                stopped.then_some(())
            })
        })
        .await;
    }

    // 4) Turn 2 starts; its synthesis parks too. Wait until the pipeline
    //    sees the new task (current_task != null again).
    explicit_request(&_state, &gw, 3, "第二句").await;
    wait_until_sync("tts call 2 started", || {
        tts_calls.lock().unwrap().len() >= 2
    })
    .await;
    let new_task = {
        let base = base.clone();
        wait_until_some("turn 2 current_task set", move || {
            let base = base.clone();
            Box::pin(async move {
                let m: Value = reqwest::get(format!("{base}/api/v0/mode"))
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                m["devices"][0]["current_task"]
                    .as_str()
                    .map(|s| s.to_string())
            })
        })
        .await
    };

    // 5) Release the OLD synthesis — it returns AFTER the new round began.
    old_gate_tx.send(true).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Acceptance 1: the old answer must produce ZERO play commands. Watch
    // the wire for a grace window so a suppressed play cannot hide by timing.
    let early = collect_plays(&gw, Duration::from_millis(700), 0).await;
    let old_plays = early
        .iter()
        .filter(|(_, m)| m["text"].as_str().map(|t| t.contains(OLD)).unwrap_or(false))
        .count();
    assert_eq!(
        old_plays, 0,
        "old answer must produce ZERO play commands after the stop (gateway saw: {early:?})"
    );

    // Acceptance 3: the old turn's tail must not clear the new turn's state.
    let m: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        m["devices"][0]["current_task"].as_str(),
        Some(new_task.as_str()),
        "old turn's TurnEnd must not clear the new turn's current_task"
    );

    // Acceptance 2: the new answer plays exactly once, with the right text —
    // counted to the NEW turn's end, not to the first play frame (§0.3 item 3).
    new_gate_tx.send(true).unwrap();
    let nt = new_task.clone();
    let base_for_done = base.clone();
    let plays = collect_plays_until(&gw, move || {
        let base = base_for_done.clone();
        let nt = nt.clone();
        async move {
            let d: Value = reqwest::get(format!("{base}/api/v0/voice/log"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            d["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "turn_end" && e["task"].as_str() == Some(nt.as_str()))
        }
    })
    .await;
    let new_plays: Vec<_> = plays
        .iter()
        .filter(|(_, m)| m["text"].as_str().map(|t| t.contains(NEW)).unwrap_or(false))
        .collect();
    assert_eq!(
        new_plays.len(),
        1,
        "new answer must play exactly once, collected to turn end (gateway saw: {plays:?})"
    );
    for (play_id, _) in &new_plays {
        gw.confirm_play(play_id).await;
    }
}

/// §0.3 unfinished item 1: the stop arrives through the UTTERANCE entry — a
/// new spoken demand both interrupts (barge-in) AND opens a new turn — not
/// the action button. The old answer's synthesis returns AFTER the new round
/// began. Acceptance, with the two turns' play ownership distinguished:
///   - the OLD answer produces ZERO play commands;
///   - the old turn's TurnEnd does not clear the new turn's current_task;
///   - the NEW answer plays EXACTLY once, collected to the new turn's end;
///   - the pipeline log shows the suppression (epoch path) and attributes
///     the spoken play to the NEW task.
#[tokio::test]
async fn utterance_stop_during_pending_synthesis_new_answer_plays_once() {
    const OLD: &str = "旧答案-CCC";
    const NEW: &str = "新答案-DDD";

    let (old_gate_tx, old_gate_rx) = tokio::sync::watch::channel(false);
    let (new_gate_tx, new_gate_rx) = tokio::sync::watch::channel(false);
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tts = Arc::new(GatedTts {
        gates: vec![(OLD, old_gate_rx), (NEW, new_gate_rx)],
        calls: calls.clone(),
    });
    let tts_calls = calls.clone();

    let (base, _state, gw) = spawn_app_with_tts(
        FakeLlm::new(vec![
            Scripted::Text {
                chunks: vec![OLD.to_string()],
            },
            Scripted::Text {
                chunks: vec![NEW.to_string()],
            },
        ]),
        tts as Arc<dyn hm_core::speech::TtsClient>,
    )
    .await;

    // 1) Turn 1 answers OLD; its synthesis parks (model already done).
    explicit_request(&_state, &gw, 1, "第一句").await;
    wait_until_sync("tts call 1 started", || {
        tts_calls.lock().unwrap().len() >= 1
    })
    .await;
    let old_task = {
        let base = base.clone();
        wait_until_some("turn 1 current_task set", move || {
            let base = base.clone();
            Box::pin(async move {
                let m: Value = reqwest::get(format!("{base}/api/v0/mode"))
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                m["devices"][0]["current_task"]
                    .as_str()
                    .map(|s| s.to_string())
            })
        })
        .await
    };

    // 2) The VOICE stop entry: a new utterance barge-ins the old round and
    //    starts turn 2 in the same event. Its synthesis parks too.
    explicit_request(&_state, &gw, 2, "停，换个问题").await;
    wait_until_sync("tts call 2 started", || {
        tts_calls.lock().unwrap().len() >= 2
    })
    .await;
    let old_task_for_wait = old_task.clone();
    let new_task = {
        let base = base.clone();
        wait_until_some("turn 2 current_task set", move || {
            let base = base.clone();
            let old = old_task_for_wait.clone();
            Box::pin(async move {
                let m: Value = reqwest::get(format!("{base}/api/v0/mode"))
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                let cur = m["devices"][0]["current_task"]
                    .as_str()
                    .map(|s| s.to_string());
                (cur.as_deref() != Some(old.as_str()))
                    .then_some(cur)
                    .flatten()
            })
        })
        .await
    };

    // 3) Release the OLD synthesis — it returns AFTER the new round began.
    old_gate_tx.send(true).unwrap();
    {
        // Wait for the old turn's tail (TurnEnd) to be processed, so the
        // guard assertion below is not racing the pipeline itself.
        let base = base.clone();
        let ot = old_task.clone();
        wait_until_some("old turn_end recorded", move || {
            let base = base.clone();
            let ot = ot.clone();
            Box::pin(async move {
                let d: Value = reqwest::get(format!("{base}/api/v0/voice/log"))
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                d["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["kind"] == "turn_end" && e["task"].as_str() == Some(ot.as_str()))
                    .then_some(())
            })
        })
        .await;
    }

    // Acceptance: the old turn's TurnEnd must not clear the new turn's task.
    {
        let m: Value = reqwest::get(format!("{base}/api/v0/mode"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            m["devices"][0]["current_task"].as_str(),
            Some(new_task.as_str()),
            "old turn's TurnEnd must not clear the new turn's current_task"
        );
    }

    // 4) Release the NEW synthesis; collect plays to the new turn's end.
    new_gate_tx.send(true).unwrap();
    let nt = new_task.clone();
    let base_for_done = base.clone();
    let plays = collect_plays_until(&gw, move || {
        let base = base_for_done.clone();
        let nt = nt.clone();
        async move {
            let d: Value = reqwest::get(format!("{base}/api/v0/voice/log"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            d["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "turn_end" && e["task"].as_str() == Some(nt.as_str()))
        }
    })
    .await;

    let old_plays: Vec<_> = plays
        .iter()
        .filter(|(_, m)| m["text"].as_str().map(|t| t.contains(OLD)).unwrap_or(false))
        .collect();
    let new_plays: Vec<_> = plays
        .iter()
        .filter(|(_, m)| m["text"].as_str().map(|t| t.contains(NEW)).unwrap_or(false))
        .collect();
    assert_eq!(
        old_plays.len(),
        0,
        "old answer must produce ZERO play commands (gateway saw: {plays:?})"
    );
    assert_eq!(
        new_plays.len(),
        1,
        "new answer must play exactly once, collected to turn end (gateway saw: {plays:?})"
    );
    for (play_id, _) in &new_plays {
        gw.confirm_play(play_id).await;
    }

    // Ownership in the pipeline log: the spoken play belongs to the NEW
    // task, the old one was suppressed by the stop epoch — recorded, not
    // inferred from the wire.
    let log: Value = reqwest::get(format!("{base}/api/v0/voice/log"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let events = log["events"].as_array().unwrap();
    let played = events
        .iter()
        .find(|e| e["kind"] == "tts_played")
        .expect("tts_played for the new answer");
    assert_eq!(
        played["task"].as_str(),
        Some(new_task.as_str()),
        "spoken play must be attributed to the new turn's task"
    );
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "tts_suppressed_after_stop_epoch"),
        "the old answer's suppression must be visible in the log"
    );
    assert!(
        !events
            .iter()
            .any(|e| e["kind"] == "tts_played" && e["task"].as_str() == Some(old_task.as_str())),
        "no play may be attributed to the old task"
    );
}

/// §0.3 unfinished item 2: the concurrency window between the last
/// stop-epoch check and the play submission. A user stop that lands INSIDE
/// that window must still win: on the wire, the old answer either never
/// plays, or its play is immediately followed by a Stop for that same
/// play_id (the submission happened; the stop reaches it). The one-shot
/// pipeline hook parks speak() exactly in the window so the stop COMPLETES
/// there — deterministic, not timing-luck.
#[tokio::test]
async fn stop_landing_in_submit_window_still_reaches_the_play() {
    const OLD: &str = "窗口答案-EEE";
    const FRESH: &str = "后续正常-FFF";

    let (old_gate_tx, old_gate_rx) = tokio::sync::watch::channel(false);
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tts = Arc::new(GatedTts {
        gates: vec![(OLD, old_gate_rx)],
        calls: calls.clone(),
    });
    let tts_calls = calls.clone();

    let (base, state, gw) = spawn_app_with_tts(
        FakeLlm::new(vec![
            Scripted::Text {
                chunks: vec![OLD.to_string()],
            },
            Scripted::Text {
                chunks: vec![FRESH.to_string()],
            },
        ]),
        tts as Arc<dyn hm_core::speech::TtsClient>,
    )
    .await;

    // 1) Turn 1 answers OLD; its synthesis parks (model already done).
    explicit_request(&state, &gw, 1, "第一句").await;
    wait_until_sync("tts call 1 started", || {
        tts_calls.lock().unwrap().len() >= 1
    })
    .await;

    // 2) Arm the race-window hook: when speak() passes its epoch check and
    //    parks in the window, fire a REAL device stop and let it complete.
    let gw_hook = gw.clone();
    let base_hook = base.clone();
    state.voice.set_before_play_submit_hook(Arc::new(move || {
        let gw = gw_hook.clone();
        let base = base_hook.clone();
        Box::pin(async move {
            gw.send(&GatewayFrame::Button {
                device: "dot2-d".into(),
                ts: hm_core::id::now_ms(),
                seq: 2,
                action: "action_press".into(),
            })
            .await;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let d: Value = reqwest::get(format!("{base}/api/v0/voice/log"))
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if d["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["kind"] == "stop_result")
                {
                    return;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "stop did not complete inside the race window"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    }));

    // 3) Release the synthesis: speak passes the epoch check (stop has NOT
    //    happened yet), parks in the window, the stop COMPLETES there, then
    //    speak continues and submits.
    old_gate_tx.send(true).unwrap();

    // 4) Collect every core frame until turn 1's tail (turn_end) is
    //    processed — then judge the invariant on the wire, not by luck.
    let frames = collect_frames_until(&gw, || async {
        let d: Value = reqwest::get(format!("{base}/api/v0/voice/log"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        d["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "turn_end")
    })
    .await;

    let old_play_ids: Vec<String> = frames
        .iter()
        .filter_map(|f| match f {
            CoreFrame::Play {
                task_id,
                text: Some(t),
                ..
            } if t.contains(OLD) => Some(task_id.clone()),
            _ => None,
        })
        .collect();
    let stopped_ids: Vec<String> = frames
        .iter()
        .filter_map(|f| match f {
            CoreFrame::Stop { task_id, .. } => Some(task_id.clone()),
            _ => None,
        })
        .collect();
    assert!(
        old_play_ids.is_empty() || old_play_ids.iter().all(|id| stopped_ids.contains(id)),
        "a stop that raced the submission must still reach the old answer's play \
         (plays: {old_play_ids:?}, stops: {stopped_ids:?}, frames: {frames:?})"
    );

    // 5) The system keeps working after the race: a fresh turn speaks once.
    explicit_request(&state, &gw, 3, "再说一句").await;
    let (fresh_id, meta) = gw.until_play("voice").await;
    assert_eq!(meta["text"].as_str(), Some(FRESH));
    gw.confirm_play(&fresh_id).await;
}

/// Diagnostics (M3): the three entrances keep their natures separate.
/// Passive: pure observed state. Active voice test: audible probe with real
/// receipts. Repair: mode projection resend.
#[tokio::test]
async fn diagnostics_passive_active_and_repair_behaviors() {
    let (base, _state, gw) = spawn_app_with_gateway(FakeLlm::new(vec![])).await;
    let client = reqwest::Client::new();

    // Passive snapshot: observed facts only, connected gateway visible.
    let d: Value = client
        .get(format!("{base}/api/v0/diagnostics"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(d["gateway"]["connected"], true);
    assert_eq!(d["gateway"]["devices"][0]["stable_id"], "dot2-d");
    assert!(
        d["services"]["llm"]["provider"].is_string(),
        "service states are part of the passive view"
    );
    assert!(d["voice_counts"].is_object() && d["gateway_counts"].is_object());
    assert!(d["note"].as_str().unwrap().contains("被动"));

    // Active voice-chain test: audible play; confirm it WHILE the probe is
    // waiting for its receipt (spawned, else the awaited response would
    // time out before anyone confirms — exactly the honest-unknown path).
    let probe_post = {
        let client = client.clone();
        let base = base.clone();
        tokio::spawn(async move {
            client
                .post(format!("{base}/api/v0/diagnostics/tests/voice_play"))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        })
    };
    let (probe_id, meta) = gw.until_play("voice").await;
    assert_eq!(meta["text"].as_str(), Some("语音链路测试"));
    gw.confirm_play(&probe_id).await;
    let active = probe_post.await.unwrap();
    assert_eq!(active["ok"], true, "{active:?}");
    assert_eq!(active["stage"], "device_confirmed");
    assert!(
        active["synth_ms"].is_u64() && active["total_ms"].is_u64(),
        "timings are real observations"
    );

    // The probe is visible in the pipeline log (audit trail).
    let log: Value = client
        .get(format!("{base}/api/v0/voice/log"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        log["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "diag_voice_play_test"),
        "the active test must be recorded"
    );

    // Repair: re-send the persisted mode request — ModeSet on the wire only.
    let repair: Value = client
        .post(format!("{base}/api/v0/diagnostics/repair/reapply_mode"))
        .json(&json!({ "device": "dot2-d" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(repair["state"], "submitted");
    assert_eq!(repair["requested"], "half");
    let frame = gw
        .until(|f| matches!(f, CoreFrame::ModeSet { duplex_requested, .. } if duplex_requested == "half"))
        .await;
    assert!(matches!(frame, CoreFrame::ModeSet { .. }));

    // Unknown device on repair is a 404, not a silent success.
    let missing = client
        .post(format!("{base}/api/v0/diagnostics/repair/reapply_mode"))
        .json(&json!({ "device": "ghost" }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}
