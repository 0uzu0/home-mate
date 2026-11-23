//! Issue #222 regression tests for media wake-takeover follow-ups (P2-1, P2-3, P2-4, P2-6).
use super::*;
use crate::tool::music::{MediaSession, MediaState, MusicRuntime, Source, Track};
use tokio::io::AsyncWriteExt;

async fn pipeline_with_connected_gateway(
    dev: &str,
) -> (Arc<VoicePipeline>, Arc<MusicRuntime>, tokio::net::TcpStream) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let pipe = VoicePipeline::new(
        DeviceHub::new(),
        store.clone(),
        Arc::new(TaskRegistry::new(store)),
        Arc::new(|| Err("isolated: no agent".into())),
        Arc::new(|| Ok(Arc::new(crate::speech::MockTts::default()))),
        Arc::new(|| Err("isolated: no ASR".into())),
        std::env::temp_dir().join(id::next("hm-222")),
    );
    let music = MusicRuntime::new(pipe.store.clone(), pipe.hub.clone());
    pipe.attach_music(&music);

    let mut ctrl = pipe.hub.subscribe_control();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv_hub = pipe.hub.clone();
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let _ = srv_hub.serve_connection(stream).await;
        }
    });
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let hello = crate::gateway::GatewayFrame::Hello {
        protocol: 2,
        min_protocol: 0,
        supported_versions: vec![0, 1, 2],
        gateway: "gw-222".into(),
        gateway_version: "0.1.0".into(),
        devices: vec![crate::gateway::HelloDevice {
            stable_id: dev.into(),
            model: "AEOBC".into(),
            firmware: Some("hm-firmware 0.0.53".into()),
            capabilities: vec![
                "audio_out".into(),
                "audio_in".into(),
                "play_stream".into(),
                "stop".into(),
            ],
            audio_profile: String::new(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
    };
    let mut line = serde_json::to_string(&hello).unwrap();
    line.push('\n');
    client.write_all(line.as_bytes()).await.unwrap();
    client.flush().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(crate::gateway::DeviceEvent::DeviceJoined { device, .. }) = ctrl.recv().await
            {
                if device.stable_id == dev {
                    break;
                }
            }
        }
    })
    .await
    .expect("DeviceJoined");
    pipe.states.lock().unwrap().entry(dev.into()).or_default();
    (pipe, music, client)
}

fn radio_session(state: MediaState, updated_ms: u64, play_id: Option<String>) -> MediaSession {
    MediaSession {
        tracks: vec![Track {
            source: Source::Radio,
            id: "yes933".into(),
            title: "Yes933".into(),
            artist: String::new(),
            binding: "radio|local".into(),
        }],
        index: 0,
        state,
        position_ms: 9_000,
        play_id,
        started_ms: updated_ms,
        base_ms: 0,
        updated_ms,
        repeat: crate::tool::music::RepeatMode::Off,
    }
}

/// P2-1: hot-path `feed_chunk` during live hub media must check `media_pause_in_flight`
/// without emitting `media_session_expired`, while still marking fresh pause-takeover
/// chunks as `wake_ack_overlap` when `wake_ack` is playing.
#[tokio::test]
async fn feed_chunk_during_live_media_is_side_effect_free_on_music_sessions() {
    let dev = "dot2-d";
    let (pipe, music, _gw) = pipeline_with_connected_gateway(dev).await;
    let notes: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = notes.clone();
    music.set_event_sink(Arc::new(move |kind: &str, _| {
        sink.lock().unwrap().push(kind.to_string());
    }));

    let _ticket = pipe
        .hub
        .play_begin(dev, "media", 16_000, Some("ext".into()))
        .unwrap();
    let expired_at = id::now_ms() - crate::tool::music::MEDIA_SESSION_TTL_MS - 2_000;
    music.seed_media_session_for_test(dev, radio_session(MediaState::Paused, expired_at, None));

    let loud = vec![8_000i16; 1280];
    let now = id::now_ms();
    for i in 0..4u64 {
        pipe.feed_chunk(
            dev.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    assert!(
        notes
            .lock()
            .unwrap()
            .iter()
            .all(|k| k != "media_session_expired"),
        "hot-path feed_chunk must not emit media_session_expired: {:?}",
        notes.lock().unwrap()
    );
    assert!(
        pipe.mic.lock().unwrap()[dev]
            .open_provenance()
            .spans
            .iter()
            .all(|s| s.control_only && !s.wake_ack_overlap),
        "expired paused session past flush window must still treat live hub media as playing"
    );

    // Fresh in-flight pause + active wake_ack MUST flip new chunks to wake_ack_overlap=true.
    let fresh = id::now_ms();
    music.seed_media_session_for_test(dev, radio_session(MediaState::Paused, fresh, None));
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.entry(dev.into()).or_default();
        st.wake_ack_play = Some("P-ack".into());
        st.outputs.insert(
            "P-ack".into(),
            OutputRecord {
                answer_text: "在呢".into(),
                request: None,
                task_id: None,
                channel: "voice".into(),
                kind: "ack".into(),
                content_ref: "ack".into(),
                stage: "streaming".into(),
                started_ms: fresh,
                stage_ms: fresh,
                end_sent_ms: None,
                expected_ms: 400,
                synth_ms: 0,
            },
        );
    }
    pipe.feed_chunk(
        dev.into(),
        loud,
        16_000,
        1,
        fresh + 80,
        crate::gateway::InputProvenance::default(),
    );
    assert!(
        pipe.mic.lock().unwrap()[dev]
            .open_provenance()
            .spans
            .last()
            .is_some_and(|s| s.control_only && s.wake_ack_overlap),
        "fresh media_pause_in_flight in feed_chunk must mark wake-ack overlap true"
    );
}

/// P2-3 & P1-3: when media is `Paused` with an in-flight `StopReceipt`:
/// - unwoken (`Idle`) `暂停` ignores the `via == "transcript"` shortcut and drops as `mic_ignored_unwoken`;
/// - woken (`Listening`) `暂停` enters `in_conversation` -> `handle_stop_intent_at(..., "followup", ...)`
///   and still runs `already_paused: true` (`close_window_preserving_paused_media`), never reaching the model;
/// - actively `Playing` media still triggers the `via == "transcript"` shortcut.
#[tokio::test]
async fn unwoken_pause_transcript_ignores_paused_media_with_inflight_hub_stop() {
    let dev = "dot2-d";
    let (pipe, music, _gw) = pipeline_with_connected_gateway(dev).await;
    let now = id::now_ms();
    let ticket = pipe
        .hub
        .play_begin(dev, "media", 16_000, Some("Yes933".into()))
        .unwrap();
    music.seed_media_session_for_test(
        dev,
        radio_session(MediaState::Paused, now, Some(ticket.play_id.clone())),
    );

    let mk = |input_id: u64| {
        let epoch = pipe
            .states
            .lock()
            .unwrap()
            .get(dev)
            .map_or(0, |s| s.stop_epoch);
        MicTranscript {
            leased: false,
            device: dev.into(),
            result: Ok("暂停。".into()),
            duration_ms: 600,
            input_id,
            capture_seq: input_id,
            window_epoch: epoch,
            control_only: false,
            wake_sliced: false,
            ticket: crate::mic::CaptureProvenance::legacy(0),
            enqueued_at: std::time::Instant::now(),
        }
    };

    // 1) Idle/unwoken: ignored as unwoken, no pause_intent and no model dispatch.
    pipe.handle_mic_transcript(mk(1)).await;
    assert!(
        !pipe.recent(20).iter().any(|e| e["kind"] == "pause_intent"),
        "unwoken speech during already-paused media must not emit pause_intent"
    );
    assert!(
        pipe.recent(20)
            .iter()
            .any(|e| e["kind"] == "mic_ignored_unwoken"),
        "unwoken speech during already-paused media must drop as mic_ignored_unwoken"
    );

    // 2) Open follow-up window: `already_paused` path runs via "followup" and closes window to Idle.
    let _ident = pipe.open_participation(dev, CreditGrant::Full);
    {
        let mut states = pipe.states.lock().unwrap();
        states.get_mut(dev).unwrap().participation.opened_by = Some("keyword");
    }
    music.seed_media_session_for_test(dev, radio_session(MediaState::Paused, id::now_ms(), None));
    pipe.handle_mic_transcript(mk(2)).await;
    assert_eq!(
        pipe.states.lock().unwrap()[dev].participation.session.state,
        SessionState::Idle,
        "in-window '暂停' while media is Paused + StopReceipt in flight must still close the follow window"
    );
    assert!(
        pipe.recent(20).iter().any(|e| {
            e["kind"] == "pause_intent" && e["via"] == "followup" && e["already_paused"] == true
        }),
        "in-window '暂停' must hit already_paused via followup without reaching the model"
    );

    // 3) Flipping the session to Playing with the same hub play MUST trigger transcript pause_intent.
    music.seed_media_session_for_test(dev, radio_session(MediaState::Playing, id::now_ms(), None));
    pipe.handle_mic_transcript(mk(3)).await;
    assert!(
        pipe.recent(20)
            .iter()
            .any(|e| e["kind"] == "pause_intent" && e["via"] == "transcript"),
        "actively playing media must still trigger transcript pause_intent"
    );
}

/// P2-4: while media is `Paused` and its hub play `StopReceipt` is still in flight,
/// `mode_snapshot`, `push_assistant_state`, `start_input`, `play_filler`,
/// `session_sweep_at`, and `follow_window_tone_sweep` must all treat media as NOT live.
#[tokio::test]
async fn paused_media_with_inflight_hub_play_unblocks_cues_sweeps_mode_and_start_input() {
    let dev = "dot2-d";
    let (pipe, music, _gw) = pipeline_with_connected_gateway(dev).await;
    let now = id::now_ms();
    let _ticket = pipe
        .hub
        .play_begin(dev, "media", 16_000, Some("Yes933".into()))
        .unwrap();
    music.seed_media_session_for_test(dev, radio_session(MediaState::Paused, now, None));

    let ident = pipe.open_participation(dev, CreditGrant::Full);
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.participation.follow_window.enabled = true;
        st.participation.follow_window.ms = 5_000;
        st.follow_open_last_tick = true;
    }

    // 1) mode_snapshot & push_assistant_state see listening/idle, not active "music".
    pipe.push_assistant_state(dev, false);
    assert!(
        pipe.sovereignty
            .live_playback_notices(id::now_ms())
            .is_empty(),
        "paused media with in-flight hub stop must not advertise active media fleet notice"
    );
    let snap = pipe.mode_snapshot();
    let dev_snap = snap["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == dev)
        .unwrap();
    assert_eq!(dev_snap["input_window_open"], true);
    assert_ne!(dev_snap["activity"], "music");

    // 2) start_input admits speech as "session" (not "pending" blocked by stale hub_live.media).
    {
        let mut mic_proc = crate::mic::MicProcessor::new(crate::mic::VadConfig::default());
        mic_proc.feed_with_provenance(
            vec![12_000i16; 1280],
            16_000,
            crate::mic::CaptureProvenance::legacy(1280),
        );
        pipe.mic.lock().unwrap().insert(dev.into(), mic_proc);
    }
    pipe.start_input(dev);
    assert_eq!(
        pipe.states.lock().unwrap()[dev]
            .input
            .as_ref()
            .map(|i| i.eligibility),
        Some("session"),
        "start_input must admit follow-up speech as session when media is already Paused"
    );
    pipe.states.lock().unwrap().get_mut(dev).unwrap().input = None;
    pipe.mic.lock().unwrap().remove(dev);

    // 3) session_sweep_at: Playing media keeps session Listening; Paused media times it out.
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        let past = id::now_ms().saturating_sub(15_000);
        let policy = FollowWindowPolicy {
            enabled: true,
            ms: 5_000,
            ..Default::default()
        };
        st.participation
            .activate(st.participation.identity(), policy, CreditGrant::Full, past)
            .unwrap();
    }
    music.seed_media_session_for_test(dev, radio_session(MediaState::Playing, id::now_ms(), None));
    assert!(
        pipe.session_sweep_at(id::now_ms()).is_empty(),
        "actively playing media must keep session open during session_sweep_at"
    );
    music.seed_media_session_for_test(dev, radio_session(MediaState::Paused, id::now_ms(), None));
    assert_eq!(
        pipe.session_sweep_at(id::now_ms()),
        vec![dev.to_string()],
        "paused media with in-flight hub stop must allow session_sweep_at to close expired session"
    );

    // 4) follow_window_tone_sweep: Playing media keeps session Listening; Paused media closes
    // the expired window to Idle AND dispatches the boundary cue immediately (not parked as busy).
    pipe.settings.lock().unwrap().follow_window_tone = true;
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        let past = id::now_ms().saturating_sub(15_000);
        let policy = FollowWindowPolicy {
            enabled: true,
            ms: 5_000,
            ..Default::default()
        };
        st.participation
            .activate(st.participation.identity(), policy, CreditGrant::Full, past)
            .unwrap();
        st.follow_open_last_tick = true;
    }
    music.seed_media_session_for_test(dev, radio_session(MediaState::Playing, id::now_ms(), None));
    pipe.follow_window_tone_sweep();
    assert_eq!(
        pipe.states.lock().unwrap()[dev].participation.session.state,
        SessionState::Listening,
        "actively playing media must still prevent follow-window timeout"
    );

    music.seed_media_session_for_test(dev, radio_session(MediaState::Paused, id::now_ms(), None));
    pipe.follow_window_tone_sweep();
    {
        let states = pipe.states.lock().unwrap();
        let st = states.get(dev).unwrap();
        assert_eq!(st.participation.session.state, SessionState::Idle);
        assert!(
            st.follow_close_pending_at.is_none(),
            "closing boundary cue must not remain parked as busy when media is already Paused"
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let events = pipe.recent(30);
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "follow_window_close" && e["skipped"] != "busy"),
        "follow_window_tone_sweep must dispatch follow_window_close instead of parking as busy: {events:?}"
    );
    let active = pipe.active_content(dev);
    assert!(
        !active.voice && !active.media,
        "active_content (used by cue_may_start and play_filler) must clear paused media"
    );

    // 5) play_filler: blocked by Playing media (`other_play_live`), allowed when Paused.
    pipe.settings.lock().unwrap().filler.mode = FillerMode::Interrupt;
    std::fs::create_dir_all(pipe.wake_ack_dir()).unwrap();
    std::fs::write(
        pipe.wake_ack_dir().join("manifest.json"),
        r#"{"voice":"","files":{"稍等":"filler.wav"}}"#,
    )
    .unwrap();
    std::fs::write(
        pipe.wake_ack_dir().join("filler.wav"),
        b"RIFF\x00\x00\x00\x00WAVE",
    )
    .unwrap();
    let ident2 = pipe.open_participation(dev, CreditGrant::Full);
    let req = {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        let _ = ident;
        st.participation
            .begin_request(ident2, id::now_ms(), false)
            .unwrap()
    };
    music.seed_media_session_for_test(dev, radio_session(MediaState::Playing, id::now_ms(), None));
    assert!(
        !pipe.play_filler(dev, "thinking", "稍等", &None, Some(req)),
        "actively playing media must still skip interrupt-mode filler"
    );
    music.seed_media_session_for_test(dev, radio_session(MediaState::Paused, id::now_ms(), None));
    let played = pipe.play_filler(dev, "thinking", "稍等", &None, Some(req));
    assert!(
        played,
        "play_filler must not be blocked by paused media whose StopReceipt is still in flight"
    );
}

/// P2-6: `capture_is_wake_ack_only` only exempts `play.channel == "media"` on `DeviceHub`
/// when `MusicRuntime` has a `MediaState::Paused` session within `MEDIA_PAUSE_FLUSH_WINDOW_MS`.
#[tokio::test]
async fn capture_is_wake_ack_only_bounds_hub_media_exemption_to_fresh_paused_flush_window() {
    let dev = "dot2-d";
    let (pipe, music, _gw) = pipeline_with_connected_gateway(dev).await;
    let now = id::now_ms();
    let _media = pipe
        .hub
        .play_begin(dev, "media", 16_000, Some("Yes933".into()))
        .unwrap();
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.wake_ack_play = Some("P-ack".into());
        st.outputs.insert(
            "P-ack".into(),
            OutputRecord {
                answer_text: "在呢".into(),
                request: None,
                task_id: None,
                channel: "voice".into(),
                kind: "ack".into(),
                content_ref: "ack".into(),
                stage: "streaming".into(),
                started_ms: now,
                stage_ms: now,
                end_sent_ms: None,
                expected_ms: 400,
                synth_ms: 0,
            },
        );
    }

    // a) No MediaSession at all -> orphan/external hub media play is NOT exempted.
    assert!(!pipe.capture_is_wake_ack_only(dev, true, false, now + 100));

    // b) Active MediaState::Playing -> NOT exempted even if caller passed media_playing=false.
    music.seed_media_session_for_test(dev, radio_session(MediaState::Playing, now, None));
    assert!(!pipe.capture_is_wake_ack_only(dev, true, false, now + 100));

    // c) Fresh MediaState::Paused within MEDIA_PAUSE_FLUSH_WINDOW_MS -> exempted!
    music.seed_media_session_for_test(dev, radio_session(MediaState::Paused, now, None));
    assert!(pipe.capture_is_wake_ack_only(dev, true, false, now + 500));

    // d) Stale MediaState::Paused past MEDIA_PAUSE_FLUSH_WINDOW_MS -> exemption expires!
    let past_flush = now + MusicRuntime::MEDIA_PAUSE_FLUSH_WINDOW_MS + 50;
    assert!(!pipe.capture_is_wake_ack_only(dev, true, false, past_flush));
}

/// P1-2: if `pause_now` sends `Stop` and `StopReceipt` times out / is lost (hub
/// `media` play never retires), once `MEDIA_PAUSE_FLUSH_WINDOW_MS` (3s) elapses,
/// `active_content` / `effective_hub_live` must stop folding `media` to `false`
/// and `session_sweep_at` / `follow_window_tone_sweep` must NOT Timeout-close
/// the session (#88.2 invariant).
#[tokio::test]
async fn lost_stop_receipt_past_flush_window_restores_live_media_and_blocks_timeout_close() {
    let dev = "dot2-d";
    let (pipe, music, _gw) = pipeline_with_connected_gateway(dev).await;
    let music_notes: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = music_notes.clone();
    music.set_event_sink(Arc::new(move |kind: &str, payload: serde_json::Value| {
        sink.lock().unwrap().push((kind.to_string(), payload));
    }));

    let now = id::now_ms();
    let ticket = pipe
        .hub
        .play_begin(dev, "media", 16_000, Some("Yes933".into()))
        .unwrap();
    music.seed_media_session_for_test(
        dev,
        radio_session(MediaState::Playing, now, Some(ticket.play_id.clone())),
    );

    // Production `pause_now`: mock gateway never replies with `StopReceipt`.
    assert!(music.pause_now(dev, "wake_takeover").is_some());
    assert!(
        !pipe.active_content(dev).media && !pipe.effective_hub_live(dev).media,
        "inside the 3s flush window, in-flight paused media is optimistically subtracted"
    );

    // Wait past `pause_now`'s 2s settle timeout AND the 3s `MEDIA_PAUSE_FLUSH_WINDOW_MS`.
    tokio::time::sleep(std::time::Duration::from_millis(
        MusicRuntime::MEDIA_PAUSE_FLUSH_WINDOW_MS + 150,
    ))
    .await;
    assert!(
        music_notes
            .lock()
            .unwrap()
            .iter()
            .any(|(k, v)| k == "media_pause_receipt" && v["flushed"] == false),
        "expected unconfirmed media_pause_receipt after 2s settle timeout"
    );
    assert!(
        pipe.active_content(dev).media && pipe.effective_hub_live(dev).media,
        "past 3s with unretired hub media play, active_content and effective_hub_live must report media=true"
    );

    // Seed an expired follow-up listening session: sweeps must NOT Timeout-close it while media is still playing!
    pipe.settings.lock().unwrap().follow_window_tone = true;
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.entry(dev.into()).or_default();
        let past = id::now_ms().saturating_sub(15_000);
        let policy = FollowWindowPolicy {
            enabled: true,
            ms: 5_000,
            ..Default::default()
        };
        st.participation
            .activate(st.participation.identity(), policy, CreditGrant::Full, past)
            .unwrap();
        st.follow_open_last_tick = true;
    }
    assert!(
        pipe.session_sweep_at(id::now_ms()).is_empty(),
        "unconfirmed pause past 3s must block session_sweep_at Timeout close"
    );
    pipe.follow_window_tone_sweep();
    {
        let states = pipe.states.lock().unwrap();
        let st = states.get(dev).unwrap();
        assert_eq!(
            st.participation.session.state,
            SessionState::Listening,
            "unconfirmed pause past 3s must block follow_window_tone_sweep Timeout close"
        );
        assert!(st.follow_close_pending_at.is_none());
    }
    assert!(
        !pipe
            .recent(20)
            .iter()
            .any(|e| e["kind"] == "follow_window_close"),
        "no boundary farewell cue may be emitted when StopReceipt failed and media play is still live"
    );
}

#[test]
fn promote_wake_capture_does_not_exempt_uncommitted_echo_preroll() {
    let mut p = crate::mic::MicProcessor::new(crate::mic::VadConfig::default());
    let mut prov = crate::mic::CaptureProvenance::legacy(1280);
    prov.spans[0].control_only = true;
    let _ = p.feed_with_provenance(vec![10_000i16; 1280], 16_000, prov);
    assert!(!p.in_speech());
    p.promote_wake_capture();
    assert!(
        p.clear_tainted_onset(),
        "uncommitted echo preroll must remain clearable after promote_wake_capture"
    );
}

#[tokio::test]
async fn mixed_real_speech_and_control_only_segment_is_not_reset_on_play_end() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let pipe = VoicePipeline::new(
        DeviceHub::new(),
        store.clone(),
        Arc::new(TaskRegistry::new(store)),
        Arc::new(|| Err("isolated: no agent".into())),
        Arc::new(|| Err("isolated: no TTS".into())),
        Arc::new(|| Err("isolated: no ASR".into())),
        std::env::temp_dir().join(id::next("hm-mixed")),
    );
    let dev = "dot2-d";
    let now = id::now_ms();
    let loud = vec![10_000i16; 1280];

    for i in 0..4u64 {
        pipe.feed_chunk(
            dev.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    assert!(pipe
        .mic
        .lock()
        .unwrap()
        .get(dev)
        .is_some_and(|p| p.in_speech()));

    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.entry(dev.into()).or_default();
        st.outputs.insert(
            "P-mid".into(),
            OutputRecord {
                answer_text: "提示".into(),
                request: None,
                task_id: None,
                channel: "voice".into(),
                kind: "answer".into(),
                content_ref: "tts".into(),
                stage: "streaming".into(),
                started_ms: now + 320,
                stage_ms: now + 320,
                end_sent_ms: None,
                expected_ms: 400,
                synth_ms: 0,
            },
        );
    }
    for i in 4..6u64 {
        pipe.feed_chunk(
            dev.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }

    pipe.handle(crate::gateway::DeviceEvent::PlayFinished {
        device: dev.into(),
        play_id: "P-mid".into(),
        played_ms: 400,
        consumed_ms: Some(380),
        progress_basis: Some("hw_delay".into()),
        ts: now + 560,
    })
    .await;

    assert!(
        pipe.mic.lock().unwrap().get(dev).unwrap().in_speech(),
        "segment containing real non-control_only speech must not be wiped on PlayFinished"
    );
    assert!(
        !pipe
            .recent(20)
            .iter()
            .any(|e| e["kind"] == "mic_echo_segment_reset_on_play_end"),
        "mixed real-speech segment must not emit mic_echo_segment_reset_on_play_end"
    );
}

// --- Serial 91-A: deterministic wake = pause + continue_match + non-swallow ---

#[test]
fn continue_match_rejects_negated_continue_phrases_and_keeps_dont_stop() {
    let s = VoiceSettings::default();
    let words = &s.stop_intents.continue_words;
    for negated in [
        "不要你继续放",
        "别继续放了",
        "不用接着讲",
        "不想继续",
        "不能继续放",
        "不许继续",
    ] {
        assert!(
            !continue_match(negated, words),
            "negated continue phrase must hand off to model: {negated}"
        );
    }
    for positive in ["不要停", "别停", "不用停", "继续放吧", "继续", "接着讲"] {
        assert!(
            continue_match(positive, words),
            "positive continue / don't-stop phrase must still match: {positive}"
        );
    }
}

#[tokio::test]
async fn handle_stop_intent_does_not_swallow_continue_when_nothing_resumable() {
    let dev = "dot2-d";
    let (pipe, music, _gw) = pipeline_with_connected_gateway(dev).await;
    let mut ident = pipe.open_participation(dev, CreditGrant::Full);

    assert!(
        !pipe.handle_stop_intent_at(dev, "继续放吧", "followup", Some(&mut ident), None),
        "continue_match with no paused program or paused media must return false so the model can reply"
    );
    assert!(
        pipe.recent(20)
            .iter()
            .any(|e| e["kind"] == "continue_intent" && e["said"] == "继续放吧"),
        "continue_intent must still be recorded"
    );

    music.seed_media_session_for_test(dev, radio_session(MediaState::Paused, id::now_ms(), None));
    assert!(
        pipe.handle_stop_intent_at(dev, "继续放吧", "followup", Some(&mut ident), None),
        "continue_match with a paused media session must consume locally to resume"
    );
}

#[tokio::test]
async fn wake_takeover_pause_survives_same_frame_pump_forget_race() {
    let dev = "dot2-d";
    let (pipe, music, _gw) = pipeline_with_connected_gateway(dev).await;
    let music_notes: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = music_notes.clone();
    music.set_event_sink(Arc::new(move |kind: &str, payload: serde_json::Value| {
        sink.lock().unwrap().push((kind.to_string(), payload));
    }));

    let now = id::now_ms();
    let ticket = pipe
        .hub
        .play_begin(dev, "media", 16_000, Some("晴天".into()))
        .unwrap();
    let play_id = ticket.play_id.clone();
    music.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks: vec![Track {
                source: Source::Library,
                id: "t1".into(),
                title: "晴天".into(),
                artist: "周杰伦".into(),
                binding: "lib".into(),
            }],
            index: 0,
            state: MediaState::Playing,
            position_ms: 9_908,
            play_id: Some(play_id.clone()),
            started_ms: now,
            base_ms: 9_908,
            updated_ms: now,
            repeat: crate::tool::music::RepeatMode::Off,
        },
    );

    let ident = pipe.open_participation(dev, CreditGrant::Full);
    let res = pipe.honor_wake_word(dev, "asr_wake", ident, None);
    assert!(res.is_some(), "honor_wake_word must succeed");

    // Simulate same-frame pump exit invoking forget() after stop-all.
    music.forget(dev, &play_id);

    let sess = music
        .media_session(dev)
        .expect("session must survive same-frame pump forget() race");
    assert_eq!(sess.state, MediaState::Paused);
    assert!(sess.play_id.is_none());
    assert!(sess.position_ms >= 9_908);

    let notes = music_notes.lock().unwrap();
    assert!(
        notes.iter().any(|(k, _)| k == "media_paused"),
        "expected media_paused event: {notes:?}"
    );
    assert!(
        !notes.iter().any(|(k, _)| k == "media_session_ended"),
        "media_session_ended must NOT fire when wake paused the session: {notes:?}"
    );
}

#[tokio::test]
async fn pause_media_for_takeover_emits_explicit_notice_on_failure_and_handles_already_paused() {
    let dev = "dot2-d";
    let (pipe, music, _gw) = pipeline_with_connected_gateway(dev).await;

    assert!(pipe.pause_media_for_takeover(dev).is_none());
    assert!(
        pipe.recent(20).iter().any(|e| {
            e["kind"] == "media_takeover_pause_failed" && e["notice"] == "刚才那首没能接上"
        }),
        "pause_media_for_takeover failure must not be silent"
    );

    music.seed_media_session_for_test(dev, radio_session(MediaState::Paused, id::now_ms(), None));
    let paused = pipe
        .pause_media_for_takeover(dev)
        .expect("already-paused session must return Some");
    assert_eq!(paused["already_paused"], true);
}

#[tokio::test]
async fn wake_takeover_pauses_story_media_and_bookmarks_live_program_and_uses_effective_hub_live() {
    let dev = "dot2-d";
    let (pipe, music, _gw) = pipeline_with_connected_gateway(dev).await;
    let now = id::now_ms();

    // 1) Story MediaSession takes pause on wake (not stop/discard).
    let ticket = pipe
        .hub
        .play_begin(dev, "media", 16_000, Some("小红帽".into()))
        .unwrap();
    music.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks: vec![Track {
                source: Source::Story,
                id: "s1".into(),
                title: "小红帽".into(),
                artist: String::new(),
                binding: "story|local".into(),
            }],
            index: 0,
            state: MediaState::Playing,
            position_ms: 4_200,
            play_id: Some(ticket.play_id.clone()),
            started_ms: now,
            base_ms: 4_200,
            updated_ms: now,
            repeat: crate::tool::music::RepeatMode::Off,
        },
    );
    // Also attach a live TTS program (story/joke) to verify bookmark preservation.
    let ident = pipe.open_participation(dev, CreditGrant::Full);
    {
        let mut states = pipe.states.lock().unwrap();
        states.get_mut(dev).unwrap().live_program = Some(Program {
            text: "从前有座山。山里有座庙。庙里有个老和尚。".into(),
            cursor: 6,
            resumable: true,
            created_ms: now,
            system_cut: false,
            ..Default::default()
        });
    }

    assert!(pipe.honor_wake_word(dev, "asr_wake", ident, None).is_some());
    music.forget(dev, &ticket.play_id);
    assert_eq!(
        music.media_session(dev).map(|s| s.state),
        Some(MediaState::Paused),
        "Source::Story media must pause on wake"
    );
    assert!(
        pipe.states.lock().unwrap()[dev].paused_program.is_some(),
        "live program (story/joke) must be bookmarked on wake"
    );

    // 2) Second wake while media is already Paused with an unretired hub play inside 3s window:
    // effective_hub_live folds media=false, so honor_wake_word does not emit pause failure.
    let _stale_hub = pipe
        .hub
        .play_begin(dev, "media", 16_000, Some("小红帽".into()))
        .unwrap();
    let ident2 = pipe.states.lock().unwrap()[dev].participation.identity();
    assert!(pipe
        .honor_wake_word(dev, "asr_wake", ident2, None)
        .is_some());
    assert!(
        !pipe
            .recent(30)
            .iter()
            .any(|e| e["kind"] == "media_takeover_pause_failed"),
        "already-paused media within flush window must not trigger spurious pause failure"
    );
}
