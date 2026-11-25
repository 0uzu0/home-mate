//! Contract v2 follow-ups surfaced by the live desk test (2026-11-16):
//! #2 a neighbour's cue is not a user request, #3 a stop-type tool must not
//! cancel the turn that called it, #4 the spotter's sensitivity is the
//! household's to tune.
use super::*;

fn pipeline() -> Arc<VoicePipeline> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    VoicePipeline::new(
        DeviceHub::new(),
        store.clone(),
        Arc::new(TaskRegistry::new(store)),
        Arc::new(|| Err("isolated: no agent".into())),
        Arc::new(|| Err("isolated: no TTS".into())),
        Arc::new(|| Err("isolated: no ASR".into())),
        std::env::temp_dir().join(id::next("hm-v2")),
    )
}

/// The handler requires a known device (the identity gate reads its state).
fn ensure_device(p: &Arc<VoicePipeline>, device: &str) {
    p.states
        .lock()
        .unwrap()
        .entry(device.to_string())
        .or_default();
}

fn transcript(device: &str, text: &str, input_id: u64) -> MicTranscript {
    MicTranscript {
        leased: false,
        device: device.into(),
        result: Ok(text.into()),
        duration_ms: 800,
        input_id,
        capture_seq: input_id,
        window_epoch: 0,
        control_only: false,
        wake_sliced: false,
        ticket: crate::mic::CaptureProvenance::legacy(0),
        enqueued_at: std::time::Instant::now(),
    }
}

/// Contract v2 #2: a device 30 cm away hears the neighbour's wake ack and
/// transcribes it. That is NOT a request — the utterance was never the
/// person's (live 2026-11-16 08:31:17: dot2-d answered dot2-b's 「来了」 with
/// a 1.58 s answer).
#[tokio::test]
async fn a_neighbours_cue_is_not_a_request() {
    let p = pipeline();
    ensure_device(&p, "dot2-d");
    p.seed_house_cue("dot2-b", "来了", id::now_ms());
    p.handle_mic_transcript(transcript("dot2-d", "来了。", 1))
        .await;
    {
        let states = p.states.lock().unwrap();
        let st = states.get("dot2-d");
        assert!(
            st.map(|s| s.current_task.is_none() && s.outputs.is_empty())
                .unwrap_or(true),
            "the neighbour's cue must not start a turn"
        );
    }
    assert!(
        p.recent(50)
            .iter()
            .any(|e| e["kind"] == "mic_foreign_cue_echo"),
        "the drop must be attributed: {:?}",
        p.recent(10)
    );
}

/// The rule is deliberately narrow: OUR OWN cue is the existing path, a longer
/// sentence that merely contains the phrase is the person's, and a cue from
/// outside the window is stale.
#[tokio::test]
async fn a_cue_echo_rule_stays_narrow() {
    let p = pipeline();
    // Same device: `strip_cue_echo`/`last_cue` owns it, not this rule.
    p.seed_house_cue("dot2-d", "在呢", id::now_ms());
    assert!(p.foreign_cue_echo("dot2-d", "在呢").is_none());

    // A sentence that CONTAINS the phrase is not an echo of it.
    p.seed_house_cue("dot2-b", "来了", id::now_ms());
    assert!(p.foreign_cue_echo("dot2-d", "来了今天几号").is_none());

    // Older than the window: the person may well be talking now.
    p.seed_house_cue("dot2-b", "嗯哼", id::now_ms() - FOREIGN_CUE_WINDOW_MS - 1);
    assert!(p.foreign_cue_echo("dot2-d", "嗯哼。").is_none());

    // Punctuation and spaces are what ASR adds; they do not change the word.
    p.seed_house_cue("dot2-b", "拜拜啦", id::now_ms());
    assert!(p.foreign_cue_echo("dot2-d", "拜拜啦！").is_some());
}

/// Contract v2 #2, the wire half: a peer tells us what it said (each device
/// runs its own core, so nothing else could know).
#[test]
fn a_peer_cue_reaches_the_house_pane() {
    let p = pipeline();
    let now = id::now_ms();
    p.note_peer_cue("dot2-b", "来了", now);
    assert!(p.foreign_cue_echo("dot2-d", "来了。").is_some());
    // Junk is refused rather than stored: empty, too long, no device.
    p.note_peer_cue("", "在呢", now);
    p.note_peer_cue("dot2-b", "", now);
    p.note_peer_cue(
        "dot2-b",
        "这是一句非常非常非常长的所谓提示语超过十六个字",
        now,
    );
    assert!(p.foreign_cue_echo("dot2-d", "在呢").is_none());
    assert!(p
        .foreign_cue_echo("dot2-d", "这是一句非常非常非常长的所谓提示语超过十六个字")
        .is_none());
}

/// Contract v2 #3: a stop-type TOOL stops the content, not the turn that
/// asked. The general stop (person's 「停止播放」) must keep its full
/// invalidation — this only pins the tool variant.
#[test]
fn a_tool_stop_keeps_its_own_turn_alive() {
    let p = pipeline();
    let device = "dot2-b";
    {
        let mut states = p.states.lock().unwrap();
        let st = states.entry(device.to_string()).or_default();
        st.stop_epoch = 7;
        st.participation.session.open(id::now_ms());
        let request = st
            .participation
            .begin_request(st.participation.identity(), id::now_ms(), false)
            .unwrap();
        st.participation.answer_started(request, "answer");
        // The program being read (a previous turn) plus this turn's own
        // half-spoken answer.
        st.live_program = Some(Program {
            origin_task: Some("T-program".into()),
            ..Program::default()
        });
        st.outputs
            .insert("P-program".into(), output("T-program", "answer"));
        st.outputs
            .insert("P-answer".into(), output("T-turn", "answer"));
        st.current_task = Some("T-turn".into());
    }
    let cancel = std::sync::atomic::AtomicBool::new(false);
    assert!(
        p.stop_program_for_tool(device, Some("T-turn"), &cancel),
        "the program existed, so the tool did stop something"
    );
    let states = p.states.lock().unwrap();
    let st = states.get(device).unwrap();
    assert!(
        st.live_program.is_none(),
        "the bookmark goes (stop = discard)"
    );
    assert_eq!(
        st.outputs.get("P-program").map(|r| r.stage.as_str()),
        Some("stopping"),
        "the program's play is stopped by name"
    );
    assert_eq!(
        st.outputs.get("P-answer").map(|r| r.stage.as_str()),
        Some("submitted"),
        "the turn's own audio is untouched"
    );
    assert_eq!(st.stop_epoch, 7, "no epoch bump: the turn is not 'barged'");
    assert_eq!(
        st.current_task.as_deref(),
        Some("T-turn"),
        "the turn's task is not cancelled"
    );
    assert!(
        st.participation.request_running(),
        "the turn's request survives, so its fact can be spoken"
    );
}

/// A tool stop on a device with nothing to stop must say so (the tool turns
/// this into a failed call instead of claiming 「已停止」).
#[test]
fn a_tool_stop_with_nothing_to_stop_is_a_no_op() {
    let p = pipeline();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    assert!(!p.stop_program_for_tool("dot2-b", Some("T-turn"), &cancel));
}

/// Contract v2 #4: the sensitivity is a validated household setting.
#[test]
fn the_spotter_sensitivity_is_validated() {
    for bad in [0.0_f64, 0.04, 0.95, f64::NAN] {
        let patch = FollowWindowPatch {
            kws_threshold: Some(bad),
            ..Default::default()
        };
        assert!(
            patch.validate().is_err(),
            "threshold {bad} must be rejected"
        );
    }
    for bad in [0.0_f64, 0.4, 6.5, f64::NAN] {
        let patch = FollowWindowPatch {
            kws_score: Some(bad),
            ..Default::default()
        };
        assert!(patch.validate().is_err(), "score {bad} must be rejected");
    }
    let ok = FollowWindowPatch {
        kws_threshold: Some(0.18),
        kws_score: Some(3.5),
        ..Default::default()
    };
    ok.validate().unwrap();
    // A persisted snapshot goes through the same rules (restore path).
    let mut s = VoiceSettings::default();
    s.kws_threshold = 0.99;
    assert!(VoicePipeline::validate_settings(&s).is_err());
    s.kws_threshold = default_kws_threshold();
    VoicePipeline::validate_settings(&s).unwrap();
}

fn output(task: &str, kind: &str) -> OutputRecord {
    OutputRecord {
        answer_text: String::new(),
        request: None,
        task_id: Some(task.into()),
        channel: "voice".into(),
        kind: kind.into(),
        content_ref: String::new(),
        stage: "submitted".into(),
        started_ms: 0,
        stage_ms: 0,
        end_sent_ms: None,
        expected_ms: 0,
        synth_ms: 0,
    }
}

#[tokio::test]
async fn near_end_presence_updates_mic_level_snapshot_and_attaches_to_asr_claim() {
    let pipe = crate::voice::tests::phase3_event_pump_tests::test_pipeline(DeviceHub::new());
    let dev = "dot2-d";
    pipe.handle(crate::gateway::DeviceEvent::NearEndPresence {
        device: dev.into(),
        ts_ms: id::now_ms(),
        duration_ms: 240,
        rms_dbfs: Some(-22.5),
        noise_floor_dbfs: Some(-52.0),
        snr_db: Some(29.5),
        clipped: false,
        echo_health: Some("healthy".into()),
        profile_version: Some("v2".into()),
    })
    .await;

    let claim = pipe.build_claim(dev, id::now_ms() as i64, None, "你好高达");
    assert_eq!(claim.rms_dbfs, Some(-22.5));
    assert_eq!(claim.snr_db, Some(29.5));
    assert!(!claim.clipped);
    assert_eq!(claim.echo_health.as_deref(), Some("healthy"));
    assert_eq!(claim.profile_version.as_deref(), Some("v2"));
}

#[tokio::test]
async fn w2_non_owner_retreat_ignored_silently_and_concurrent_rooms_stay_independent() {
    let pipe = pipeline();
    let cfg = crate::config::ConfigService::new(pipe.store.clone());
    cfg.save_device_meta("dot2-a", "客厅", "客厅", "test")
        .unwrap();
    cfg.save_device_meta("dot2-b", "走廊", "走廊", "test")
        .unwrap();
    cfg.save_device_meta("dot2-c", "卧室", "卧室", "test")
        .unwrap();

    ensure_device(&pipe, "dot2-a");
    ensure_device(&pipe, "dot2-b");
    ensure_device(&pipe, "dot2-c");

    let now = id::now_ms();
    let lr = pipe
        .sovereignty
        .claim_or_renew("客厅", "dot2-a", now, 9_000, 240_000);
    let br = pipe
        .sovereignty
        .claim_or_renew("卧室", "dot2-c", now + 3_000, 9_000, 240_000);
    assert_ne!(lr.session_id, br.session_id);
    assert_eq!(pipe.sovereignty.snapshot(now + 4_000).len(), 2);

    // Non-owner dot2-b in hallway hears trailing "退下吧" -> ignored silently!
    let b_id = pipe.states.lock().unwrap()["dot2-b"]
        .participation
        .identity();
    assert!(!pipe.retreat_session_at("dot2-b", "退下吧", "keyword", b_id));
    assert!(pipe
        .recent(50)
        .iter()
        .any(|e| e["kind"] == "retreat_ignored_not_owner" && e["device"] == "dot2-b"));

    // Owner dot2-a in living room can retreat its own session; bedroom remains active.
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut("dot2-a").unwrap();
        st.participation
            .activate(
                st.participation.identity(),
                FollowWindowPolicy::default(),
                CreditGrant::Full,
                now,
            )
            .unwrap();
    }
    let a_id = pipe.states.lock().unwrap()["dot2-a"]
        .participation
        .identity();
    assert!(pipe.retreat_session_at("dot2-a", "退下吧", "keyword", a_id));
    assert!(!pipe
        .sovereignty
        .has_active_voice_window("客厅", "dot2-a", now + 4_000));
    assert!(pipe
        .sovereignty
        .has_active_voice_window("卧室", "dot2-c", now + 4_000));
}

#[tokio::test]
async fn w2_88_voice_live_and_media_live_prevent_timeout_and_post_media_guard_gates_mic() {
    let pipe = pipeline();
    let dev = "dot2-d";
    ensure_device(&pipe, dev);
    let now = 100_000u64;
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.participation
            .activate(
                st.participation.identity(),
                FollowWindowPolicy::default(),
                CreditGrant::Full,
                now,
            )
            .unwrap();
        st.follow_open_last_tick = true;
        st.outputs.insert(
            "tts-1".into(),
            OutputRecord {
                channel: "voice".into(),
                kind: "answer".into(),
                task_id: None,
                request: None,
                answer_text: "long answer".into(),
                stage: "playing".into(),
                started_ms: now,
                stage_ms: now,
                end_sent_ms: None,
                expected_ms: 0,
                synth_ms: 0,
                content_ref: String::new(),
            },
        );
    }

    // #88.1: 15s later (> 9s follow_window_ms), active TTS keeps session_open == true.
    assert!(pipe.states.lock().unwrap()[dev].session_open(now + 15_000));
    assert!(pipe.session_sweep_at(now + 15_000).is_empty());

    // #88.2: Switch to active media playback (> 9s) -> must NOT timeout or emit farewell.
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.outputs.remove("tts-1");
        st.outputs.insert(
            "media-1".into(),
            OutputRecord {
                channel: "media".into(),
                kind: "media".into(),
                task_id: None,
                request: None,
                answer_text: String::new(),
                stage: "playing".into(),
                started_ms: now,
                stage_ms: now,
                end_sent_ms: None,
                expected_ms: 0,
                synth_ms: 0,
                content_ref: String::new(),
            },
        );
    }
    assert!(pipe.session_sweep_at(now + 20_000).is_empty());
    pipe.follow_window_tone_sweep();
    assert!(!pipe
        .recent(50)
        .iter()
        .any(|e| e["kind"] == "follow_window_close"));

    // #77: 2s post-media guard disables mic follow-window right after media stops.
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.outputs.remove("media-1");
        st.last_media_end_ms = now + 20_000;
        st.participation.last_activity_ms = now + 20_000;
        st.participation.last_session_open_ms = now + 20_000;
        assert!(!st.session_covers(now + 20_500, now + 21_000, now + 21_000));
        assert!(st.session_covers(now + 22_500, now + 23_000, now + 23_000));
    }
}

#[tokio::test]
async fn w2_88_first_packet_timeout_emits_user_visible_notice() {
    struct StalledLlm;
    #[async_trait::async_trait]
    impl crate::llm::LlmClient for StalledLlm {
        async fn chat_stream(
            &self,
            _model: &str,
            _messages: Vec<crate::llm::WireMessage>,
            _tools: &[crate::llm::ToolSpec],
        ) -> Result<crate::llm::LlmStream, crate::llm::LlmError> {
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    let store = Arc::new(Store::open_in_memory().unwrap());
    let hub = DeviceHub::new();
    let tasks = Arc::new(TaskRegistry::new(store.clone()));
    let env_store = store.clone();
    let env_tasks = tasks.clone();
    let env_factory: EnvFactory = Arc::new(move || {
        Ok(Arc::new(AgentEnv {
            llm: Arc::new(StalledLlm),
            model: "test".into(),
            llm_fallback: None,
            registry: Arc::new(crate::tool::Registry::new()),
            sessions: Arc::new(crate::session::SessionStore::new(env_store.clone())),
            tasks: env_tasks.clone(),
            traces: Arc::new(crate::trace::TraceRecorder::new(env_store.clone())),
            memory: crate::memory::MemoryService::new(env_store.clone()),
            working_memory: crate::working_memory::WorkingMemoryStore::new(env_store.clone()),
            shared_memory: None,
            user_md: None,
            scope: Default::default(),
            prompt_preferences: Default::default(),
            assistant_name: crate::ASSISTANT_NAME.into(),
            llm_first_event_timeout: std::time::Duration::from_secs(5),
        }))
    });
    let pipe = VoicePipeline::new(
        hub,
        store,
        tasks,
        env_factory,
        Arc::new(|| Err("isolated: no TTS".into())),
        Arc::new(|| Err("isolated: no ASR".into())),
        std::env::temp_dir().join(id::next("hm-w2-88")),
    );
    let dev = "dot2-d";
    ensure_device(&pipe, dev);
    *pipe.turn_wait_budget.lock().unwrap() = Some((40, 200));

    let now = id::now_ms();
    let (ticket, turn_id) = {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.participation
            .activate(
                st.participation.identity(),
                FollowWindowPolicy::default(),
                CreditGrant::Full,
                now,
            )
            .unwrap();
        let req = st
            .participation
            .begin_request(st.participation.identity(), now, false)
            .unwrap();
        let tid = st.claim_turn();
        (
            TurnTicket {
                request: req,
                stop_epoch: st.stop_epoch,
                context: None,
            },
            tid,
        )
    };
    pipe.clone()
        .run_device_turn_inner(dev.into(), "你好".into(), "test".into(), ticket, turn_id)
        .await;

    let events = pipe.recent(50);
    assert!(events
        .iter()
        .any(|e| e["kind"] == "turn_budget_exceeded" && e["phase"] == "first_answer"));
    assert!(events
        .iter()
        .any(|e| e["kind"] == "turn_first_packet_timeout_notice"));
}

#[tokio::test]
async fn assistant_state_emits_fleet_playback_notice_and_bleed_emits_sovereignty_decision() {
    let pipe = pipeline();
    ensure_device(&pipe, "dot2-a");
    ensure_device(&pipe, "dot2-b");

    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut("dot2-a").unwrap();
        st.outputs.insert(
            "tts-1".into(),
            OutputRecord {
                answer_text: "你好高达播放音乐".into(),
                request: None,
                task_id: None,
                channel: "voice".into(),
                kind: "answer".into(),
                content_ref: String::new(),
                stage: "playing".into(),
                started_ms: id::now_ms(),
                stage_ms: id::now_ms(),
                end_sent_ms: None,
                expected_ms: 1000,
                synth_ms: 50,
            },
        );
    }
    // Transition into "speaking" records active FleetPlaybackNotice
    pipe.push_assistant_state("dot2-a", false);
    assert!(
        pipe.check_fleet_bleed("dot2-b", "你好高达", true, None),
        "peer bleed during active dot2-a speaking should be suppressed"
    );
    let events = pipe.recent(20);
    assert!(events.iter().any(|e| {
        e["kind"] == "sovereignty_decision"
            && e["reason"] == "abstained_bleed"
            && e["room_owner"] == "dot2-a"
    }));

    // Transition out of "speaking" clears active FleetPlaybackNotice
    {
        let mut states = pipe.states.lock().unwrap();
        states.get_mut("dot2-a").unwrap().outputs.clear();
    }
    pipe.push_assistant_state("dot2-a", false);
    assert!(
        !pipe.check_fleet_bleed("dot2-b", "你好高达", true, None),
        "after dot2-a stops speaking, peer should no longer abstain as bleed"
    );
}

#[tokio::test]
async fn music_state_emits_media_fleet_notice_renews_and_clears_on_stop() {
    let pipe = pipeline();
    ensure_device(&pipe, "dot2-a");
    {
        let mut states = pipe.states.lock().unwrap();
        states.get_mut("dot2-a").unwrap().outputs.insert(
            "media-1".into(),
            OutputRecord {
                answer_text: String::new(),
                request: None,
                task_id: None,
                channel: "media".into(),
                kind: "media".into(),
                content_ref: "music:library:t01".into(),
                stage: "playing".into(),
                started_ms: id::now_ms(),
                stage_ms: id::now_ms(),
                end_sent_ms: None,
                expected_ms: 60_000,
                synth_ms: 0,
            },
        );
    }

    // 1. ARB-06: active "music" state maps to FleetPlaybackNotice { channel: "media", active: true }.
    pipe.push_assistant_state("dot2-a", false);
    let notices_1 = pipe.sovereignty.live_playback_notices(id::now_ms());
    assert_eq!(notices_1.len(), 1);
    assert_eq!(notices_1[0].device, "dot2-a");
    assert_eq!(notices_1[0].channel, "media");
    assert!(notices_1[0].active);
    let first_at = notices_1[0].at_ms;

    // 2. ARB-07: continuous media past >15s (three 5s renewal intervals) refreshes notice timestamp
    // and updates preview immediately when track content changes (never indefinitely stale).
    let mut prev_at = first_at;
    for step in 1..=3 {
        {
            let mut states = pipe.states.lock().unwrap();
            let st = states.get_mut("dot2-a").unwrap();
            st.fleet_playback_sent_ms = id::now_ms().saturating_sub(6_000);
            if let Some(out) = st.outputs.get_mut("media-1") {
                out.content_ref = format!("music:library:t0{step}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        pipe.push_assistant_state("dot2-a", false);
        let notices = pipe.sovereignty.live_playback_notices(id::now_ms());
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].at_ms > prev_at,
            "continuous playback (>15s across step {step}) must renew FleetPlaybackNotice.at_ms ({} > {})",
            notices[0].at_ms,
            prev_at
        );
        assert_eq!(
            notices[0].text_preview,
            format!("music:library:t0{step}"),
            "preview must update without staying stale"
        );
        prev_at = notices[0].at_ms;
    }

    // 3. Device disconnect (`on_device_offline`) or clearing active media play retires notice immediately.
    pipe.on_device_offline("dot2-a");
    assert!(pipe
        .sovereignty
        .live_playback_notices(id::now_ms())
        .is_empty());
}

#[tokio::test]
async fn play_finished_resets_tainted_control_only_mic_echo_segment() {
    let pipe = pipeline();
    let dev = "dot2-d";
    let now = id::now_ms();
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.entry(dev.into()).or_default();
        st.outputs.insert(
            "P-ans".into(),
            OutputRecord {
                answer_text: "想听哪方面的呢？".into(),
                request: None,
                task_id: Some("T-ans".into()),
                channel: "voice".into(),
                kind: "answer".into(),
                content_ref: String::new(),
                stage: "device_confirmed".into(),
                started_ms: now,
                stage_ms: now,
                end_sent_ms: None,
                expected_ms: 14_000,
                synth_ms: 400,
            },
        );
    }
    let loud = vec![10_000i16; 1280];
    for i in 0..6u64 {
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
        pipe.mic_arc(dev).is_some_and(|arc| {
            let p = arc.lock().unwrap();
            p.in_speech() && p.open_provenance().spans.iter().any(|s| s.control_only)
        }),
        "expected an open control_only mic echo segment during voice playback"
    );

    pipe.handle(crate::gateway::DeviceEvent::PlayFinished {
        device: dev.into(),
        play_id: "P-ans".into(),
        played_ms: 13_769,
        consumed_ms: Some(13_613),
        progress_basis: Some("hw_delay".into()),
        ts: now + 13_769,
    })
    .await;

    assert!(
        !pipe.mic_arc(dev).unwrap().lock().unwrap().in_speech(),
        "tainted control_only mic segment must be reset on voice PlayFinished"
    );
    assert!(
        pipe.recent(10)
            .iter()
            .any(|e| e["kind"] == "mic_echo_segment_reset_on_play_end" && e["play_id"] == "P-ans"),
        "expected mic_echo_segment_reset_on_play_end event"
    );
}

#[tokio::test]
async fn confirmation_reply_bypasses_mic_filler_ignored_when_close_confirmation_pending() {
    let pipe = pipeline();
    let dev = "dot2-d";
    let now = id::now_ms();
    let _ident = pipe.open_participation(dev, CreditGrant::Full);

    let mk = |input_id: u64, text: &str| MicTranscript {
        ticket: crate::mic::CaptureProvenance::default(),
        leased: false,
        device: dev.into(),
        result: Ok(text.into()),
        duration_ms: 500,
        input_id,
        capture_seq: input_id,
        window_epoch: 0,
        control_only: false,
        wake_sliced: false,
        enqueued_at: std::time::Instant::now(),
    };

    // Without pending confirmation, "是的。" is dropped by `mic_filler_ignored`.
    pipe.handle_mic_transcript(mk(1, "是的。")).await;
    assert!(
        pipe.recent(10)
            .iter()
            .any(|e| e["kind"] == "mic_filler_ignored" && e["text"] == "是的。"),
        "idle/normal conversation must still filter bare filler '是的。'"
    );

    // With `close_confirmation` pending in the active follow-up window, "是的。"
    // must NOT be dropped by `mic_filler_ignored` (reproduces dot2-d "拜拜" -> "是的。").
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        let gen = st.participation.generation();
        st.participation.close_confirmation = Some(super::participation::RequestTicket {
            generation: gen,
            request_id: 1,
            kind: super::participation::RequestKind::Followup,
        });
        st.participation.session.since_ms = now;
    }
    let before_ignored = pipe
        .recent(50)
        .iter()
        .filter(|e| e["kind"] == "mic_filler_ignored")
        .count();
    pipe.handle_mic_transcript(mk(2, "是的。")).await;
    let after_ignored = pipe
        .recent(50)
        .iter()
        .filter(|e| e["kind"] == "mic_filler_ignored")
        .count();
    assert_eq!(
        before_ignored, after_ignored,
        "pending close_confirmation must exempt '是的。' from mic_filler_ignored"
    );
}

#[tokio::test]
async fn confirmation_table_expiry_gates_mic_filler_bypass() {
    let pipe = pipeline();
    let dev = "dot2-d";
    let now = id::now_ms();
    let _ident = pipe.open_participation(dev, CreditGrant::Full);
    let sid = format!("dev:{dev}");
    let mk = |input_id: u64, text: &str| MicTranscript {
        ticket: crate::mic::CaptureProvenance::default(),
        leased: false,
        device: dev.into(),
        result: Ok(text.into()),
        duration_ms: 500,
        input_id,
        capture_seq: input_id,
        window_epoch: 0,
        control_only: false,
        wake_sliced: false,
        enqueued_at: std::time::Instant::now(),
    };

    // An EXPIRED pending row in `confirmations` must NOT disable `mic_filler_ignored`.
    pipe.store
        .with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO sessions (id, created_ms, title) VALUES (?1, ?2, '')",
                rusqlite::params![sid, now as i64],
            )?;
            c.execute(
                "INSERT INTO confirmations \
                 (id, task_id, session_id, requester, actor_key, boot_id, catalog_revision, operation_revision, request_digest, state, revision, data_epoch, session_generation, created_ms, expires_ms, action_json) \
                 VALUES ('CF-EXP', 'T-EXP', ?1, 'voice', 'device', 'boot1', '1', '1', 'd', 'pending', 1, 1, 1, ?2, ?3, '{}')",
                rusqlite::params![sid, (now - 60_000) as i64, (now - 1_000) as i64],
            )?;
            Ok(())
        })
        .unwrap();
    pipe.handle_mic_transcript(mk(1, "是的。")).await;
    let after_expired = pipe
        .recent(50)
        .iter()
        .filter(|e| e["kind"] == "mic_filler_ignored")
        .count();
    assert_eq!(
        after_expired, 1,
        "expired pending confirmation row must NOT bypass mic_filler_ignored"
    );

    // An unexpired pending confirmation for the current device session MUST bypass mic_filler_ignored.
    pipe.store
        .with(|c| {
            c.execute(
                "INSERT INTO confirmations \
                 (id, task_id, session_id, requester, actor_key, boot_id, catalog_revision, operation_revision, request_digest, state, revision, data_epoch, session_generation, created_ms, expires_ms, action_json) \
                 VALUES ('CF-LIVE', 'T-LIVE', ?1, 'voice', 'device', 'boot1', '1', '1', 'd2', 'pending', 1, 1, 1, ?2, ?3, '{}')",
                rusqlite::params![sid, now as i64, (now + 60_000) as i64],
            )?;
            Ok(())
        })
        .unwrap();
    pipe.handle_mic_transcript(mk(2, "好的。")).await;
    let after_live = pipe
        .recent(50)
        .iter()
        .filter(|e| e["kind"] == "mic_filler_ignored")
        .count();
    assert_eq!(
        after_live, after_expired,
        "unexpired pending confirmation row must exempt '好的。' from mic_filler_ignored"
    );
}

#[tokio::test]
async fn wake_during_already_open_session_updates_since_ms_and_already_paused_closes_window() {
    let pipe = pipeline();
    let dev = "dot2-d";
    let id1 = pipe.open_participation(dev, CreditGrant::Full);
    let first_since = {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.participation.opened_by = Some("keyword");
        st.participation.session.since_ms
    };
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;

    // 1. Finishing an answer + request in an open follow-up window (e.g.
    // assistant TTS "好的，正在播放...") advances `last_session_open_ms` via
    // `settle`, which must NOT advance `session.since_ms` (preserving
    // `blocks_passive_capture` during media!).
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.participation.follow_window.enabled = true;
        st.participation.follow_window.credits = Some(3);
        let now = id::now_ms();
        let req = st.participation.begin_request(id1, now, false).unwrap();
        st.participation.answer_started(req, "tts-1");
        st.participation.answer_ended(req, "tts-1", now + 10);
        st.participation.finish_request(req, now + 10);
        assert_eq!(
            st.participation.session.state,
            super::SessionState::Listening
        );
        assert_eq!(
            st.participation.last_session_open_ms,
            now + 10,
            "settle must advance last_session_open_ms to answer end"
        );
        assert_eq!(
            st.participation.session.since_ms, first_since,
            "settle must NOT advance session.since_ms"
        );
    }

    // 2. Re-waking while session is already Listening MUST advance `session.since_ms`.
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let mut id2 = pipe.open_participation(dev, CreditGrant::Full);
    let second_since = pipe
        .states
        .lock()
        .unwrap()
        .get(dev)
        .unwrap()
        .participation
        .session
        .since_ms;
    assert!(
        second_since > first_since,
        "re-activating an already-open session must advance session.since_ms ({second_since} > {first_since})"
    );

    // 3. When a program/media bookmark is already paused (e.g. paused by the wake word)
    // and the user says "暂停", `handle_stop_intent_at` must close the follow-up window
    // to Idle while keeping the paused bookmark intact and recording `already_paused: true`.
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.paused_program = Some(super::Program {
            text: "很久很久以前，有一只小兔子。".into(),
            created_ms: id::now_ms(),
            resumable: true,
            ..Default::default()
        });
    }
    let mut debit = true;
    let handled =
        pipe.handle_stop_intent_at(dev, "暂停。", "followup", Some(&mut id2), Some(&mut debit));
    assert!(handled, "'暂停。' must be handled by handle_stop_intent_at");
    {
        let states = pipe.states.lock().unwrap();
        let st = states.get(dev).unwrap();
        assert_eq!(
            st.participation.session.state,
            super::SessionState::Idle,
            "saying '暂停' when already paused must close the follow-up listening window"
        );
        assert!(
            st.paused_program.is_some(),
            "paused bookmark must be preserved when closing window on already_paused"
        );
    }
    assert!(
        pipe.recent(10).iter().any(|e| {
            e["kind"] == "pause_intent" && e["paused"] == true && e["already_paused"] == true
        }),
        "expected pause_intent with already_paused: true"
    );
}

#[tokio::test]
async fn wake_claim_derives_snr_from_keyword_rms_and_legacy_self_heard_retreat_during_tts_is_ignored(
) {
    let pipe = pipeline();
    let dev = "dot2-d";
    // 1. Simulate a periodic NearEndPresence frame during a quiet/ambient window
    // (raw RMS -42.11 dBFS against -47.64 dBFS noise floor -> snr_db 5.53 < 6.0).
    pipe.handle(crate::gateway::DeviceEvent::NearEndPresence {
        device: dev.into(),
        ts_ms: id::now_ms(),
        duration_ms: 1000,
        rms_dbfs: Some(-42.11),
        noise_floor_dbfs: Some(-47.64),
        snr_db: Some(5.53),
        clipped: false,
        echo_health: Some("healthy".into()),
        profile_version: Some("dot2-array-postaec-v1".into()),
    })
    .await;
    // When KWS fires on "你好小智" at raw -21.06 dBFS, `build_claim` must derive
    // `snr_db` from `-21.06 - (-47.64) = 26.58 dB` (>= 6.0 dB), not keep 5.53 dB.
    let claim = pipe.build_claim(dev, id::now_ms() as i64, Some(-21.06), "你好小智");
    assert!(
        claim.snr_db.unwrap_or(0.0) > 25.0,
        "wake claim must derive snr_db from keyword rms_dbfs against noise floor, got {:?}",
        claim.snr_db
    );

    // 2. While a live voice TTS stream (`channel == "voice"`) is playing on real
    // gateway hardware (`InputOrigin::Legacy`), a `self_heard: true` KWS hit on
    // "停止播放" must be ignored as `self_heard_retreat_overlap` instead of
    // cutting off the assistant's own TTS answer with "拜拜啦".
    let id1 = pipe.open_participation(dev, CreditGrant::Full);
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        let req = st
            .participation
            .begin_request(id1, id::now_ms(), false)
            .unwrap();
        st.outputs.insert(
            "tts-live".into(),
            super::OutputRecord {
                answer_text: "好的，如需停止播放请随时告诉我。".into(),
                request: Some(req),
                task_id: None,
                channel: "voice".into(),
                kind: "answer".into(),
                content_ref: "tts".into(),
                stage: "accepted".into(),
                started_ms: id::now_ms(),
                stage_ms: id::now_ms(),
                end_sent_ms: None,
                expected_ms: 0,
                synth_ms: 0,
            },
        );
    }
    pipe.on_keyword(
        dev,
        "停止播放",
        id::now_ms(),
        Some(1000),
        true,
        crate::gateway::InputProvenance::default(),
    );
    assert_eq!(
        pipe.states.lock().unwrap()[dev].participation.close_reason,
        None,
        "self_heard legacy retreat matching active voice TTS text must not close the session"
    );
    assert!(
        pipe.recent(10)
            .iter()
            .any(|e| e["kind"] == "keyword_ignored" && e["reason"] == "self_heard_retreat_overlap"),
        "expected keyword_ignored with self_heard_retreat_overlap"
    );

    // Serial 92 (dot2-d 16:04:34): even when the live TTS answer text does NOT
    // contain the retreat phrase, `self_heard=true` during active voice output
    // must still be ignored so KWS echo hallucinations never kill the session.
    pipe.states
        .lock()
        .unwrap()
        .get_mut(dev)
        .unwrap()
        .outputs
        .get_mut("tts-live")
        .unwrap()
        .answer_text = "明天沈阳晴，零下三度。".into();
    pipe.on_keyword(
        dev,
        "停止播放",
        id::now_ms(),
        Some(1000),
        true,
        crate::gateway::InputProvenance::default(),
    );
    assert_eq!(
        pipe.states.lock().unwrap()[dev].participation.close_reason,
        None,
        "self_heard retreat during unrelated active voice TTS must be ignored"
    );
    // A genuine user retreat (`self_heard=false`) still closes the session.
    pipe.on_keyword(
        dev,
        "停止播放",
        id::now_ms(),
        Some(1000),
        false,
        crate::gateway::InputProvenance::default(),
    );
    assert_eq!(
        pipe.states.lock().unwrap()[dev].participation.close_reason,
        Some(super::CloseReason::ExplicitExit),
        "non-self_heard retreat during active voice TTS must close the session"
    );
}

#[tokio::test]
async fn gateway_disconnect_emits_device_left_first_and_cleans_voice_state() {
    use tokio::io::AsyncWriteExt;

    let pipe = pipeline();
    let hub = pipe.hub.clone();
    let mut ctrl = hub.subscribe_control();
    pipe.start();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv_hub = hub.clone();
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
        gateway: "gw-drop-test".into(),
        gateway_version: "0.1.0".into(),
        devices: vec![crate::gateway::HelloDevice {
            stable_id: "dot2-drop".into(),
            model: "AEOBC".into(),
            firmware: Some("hm-firmware 0.0.44".into()),
            capabilities: vec!["audio_out".into(), "audio_in".into()],
            audio_profile: String::new(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
        auth_token: None,
    };
    let mut line = serde_json::to_string(&hello).unwrap();
    line.push('\n');
    client.write_all(line.as_bytes()).await.unwrap();
    client.flush().await.unwrap();

    // Wait for DeviceJoined so the pipeline has registered the device.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(crate::gateway::DeviceEvent::DeviceJoined { device, .. }) = ctrl.recv().await
            {
                if device.stable_id == "dot2-drop" {
                    break;
                }
            }
        }
    })
    .await
    .expect("DeviceJoined should arrive");

    // Seed active output, running task, mic_active=true, acoustics_reconciled=true.
    let task = pipe.tasks.create("sess-drop").unwrap();
    let task_id = task.id.clone();
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.entry("dot2-drop".into()).or_default();
        st.outputs.insert(
            "out-1".into(),
            super::OutputRecord {
                channel: "voice".into(),
                kind: "answer".into(),
                task_id: Some(task_id.clone()),
                request: None,
                answer_text: "hello".into(),
                stage: "playing".into(),
                started_ms: id::now_ms(),
                stage_ms: id::now_ms(),
                end_sent_ms: None,
                expected_ms: 0,
                synth_ms: 0,
                content_ref: String::new(),
            },
        );
        st.current_task = Some(task_id.clone());
        st.mic_active = Some(true);
        st.acoustics_reconciled = true;
        st.kws_reconciled = true;
    }

    // Drop the TCP connection to simulate a gateway disconnect.
    drop(client);

    let mut events = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while events.len() < 2 {
            match ctrl.recv().await {
                Ok(crate::gateway::DeviceEvent::DeviceLeft { device }) if device == "dot2-drop" => {
                    events.push("device_left");
                }
                Ok(crate::gateway::DeviceEvent::GatewayDisconnected { .. }) => {
                    events.push("gateway_disconnected");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("DeviceLeft and GatewayDisconnected should both arrive");
    assert_eq!(
        events,
        vec!["device_left", "gateway_disconnected"],
        "DeviceLeft must precede GatewayDisconnected"
    );

    // Wait briefly for VoicePipeline pump to process DeviceLeft.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let clean = {
                let states = pipe.states.lock().unwrap();
                states.get("dot2-drop").is_some_and(|st| {
                    st.outputs.is_empty()
                        && st.current_task.is_none()
                        && st.mic_active == Some(false)
                        && !st.acoustics_reconciled
                        && !st.kws_reconciled
                })
            };
            if clean {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("VoicePipeline must clean up state on gateway disconnect");

    assert!(
        task.is_cancelled(),
        "current task must be cancelled on disconnect"
    );
}

/// D1 (#217): a play that ends mid-echo leaves an UNCOMMITTED onset behind
/// (fewer loud blocks than `start_blocks`, so nothing was segmented yet). The
/// gateway's PlayFinished must clear that run, or the first real syllable
/// afterwards is glued to our own reverb and the sentence never opens.
///
/// Drives the real device-event entrance (`handle(PlayFinished)`), not the
/// mic helper: the clearing itself lives in the pump.
#[tokio::test]
async fn play_finished_clears_an_uncommitted_echo_onset_before_the_next_sentence() {
    use crate::gateway::{DeviceEvent, InputProvenance};
    let pipe = pipeline();
    let dev = "dot2-d";
    let now = id::now_ms();
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.entry(dev.into()).or_default();
        st.outputs.insert(
            "P-echo".into(),
            OutputRecord {
                answer_text: "这段故事还要讲一会儿。".into(),
                request: None,
                task_id: Some("T-echo".into()),
                channel: "voice".into(),
                kind: "answer".into(),
                content_ref: String::new(),
                stage: "device_confirmed".into(),
                started_ms: now,
                stage_ms: now,
                end_sent_ms: None,
                expected_ms: 9_000,
                synth_ms: 300,
            },
        );
    }
    // Two loud blocks: below start_blocks (3), so the echo stays an onset run
    // in preroll and never becomes a segment.
    let loud = vec![10_000i16; 1280];
    for i in 0..2u64 {
        pipe.feed_chunk(
            dev.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            InputProvenance::default(),
        );
    }
    assert!(
        pipe.mic_arc(dev).is_some_and(|arc| {
            let p = arc.lock().unwrap();
            !p.in_speech() && p.onset_pending()
        }),
        "expected an uncommitted echo onset during voice playback"
    );

    pipe.handle(DeviceEvent::PlayFinished {
        device: dev.into(),
        play_id: "P-echo".into(),
        played_ms: 8_900,
        consumed_ms: Some(8_800),
        progress_basis: Some("hw_delay".into()),
        ts: now + 8_900,
    })
    .await;

    assert!(
        pipe.mic_arc(dev).is_some_and(|arc| {
            let p = arc.lock().unwrap();
            !p.onset_pending() && !p.in_speech()
        }),
        "PlayFinished must clear the uncommitted echo run"
    );

    // The person speaks immediately: three loud blocks open a segment, and the
    // hangover closes it as a normal sentence.
    for i in 0..3u64 {
        pipe.feed_chunk(
            dev.into(),
            loud.clone(),
            16_000,
            1,
            now + 9_000 + i * 80,
            InputProvenance::default(),
        );
    }
    assert!(
        pipe.mic_arc(dev)
            .is_some_and(|arc| arc.lock().unwrap().in_speech()),
        "real speech right after PlayFinished must open a segment"
    );
    let quiet = vec![0i16; 1280];
    for i in 0..8u64 {
        pipe.feed_chunk(
            dev.into(),
            quiet.clone(),
            16_000,
            1,
            now + 9_300 + i * 80,
            InputProvenance::default(),
        );
    }
    assert!(
        pipe.recent(50)
            .iter()
            .any(|e| e["kind"] == "mic_segment_closed"),
        "the sentence after the cleared echo must close as a segment: {:?}",
        pipe.recent(8)
    );
}

fn retreat_provenance() -> crate::gateway::InputProvenance {
    crate::gateway::InputProvenance::default()
}

/// Serial 92: on Legacy links, any active live_program suppresses `self_heard`
/// retreat words regardless of whether the text contains the spotted phrase.
#[test]
fn retreat_overlap_matches_the_live_program_text() {
    let p = pipeline();
    let dev = "dot2-d";
    let now = id::now_ms();
    {
        let mut states = p.states.lock().unwrap();
        let st = states.entry(dev.into()).or_default();
        st.live_program = Some(Program {
            text: "如果想停下来，就说停止播放。".into(),
            created_ms: now,
            resumable: true,
            ..Default::default()
        });
    }
    assert!(p.retreat_is_own_overlap(dev, "停止播放", &retreat_provenance()));
    assert!(p.retreat_is_own_overlap(dev, "退下", &retreat_provenance()));
}

/// Serial 92: a locally spoken cue within 5 s suppresses any `self_heard`
/// retreat word on Legacy links (KWS can hallucinate a retreat from cue echo).
#[test]
fn retreat_overlap_matches_a_recent_last_cue() {
    let p = pipeline();
    let dev = "dot2-d";
    let now = id::now_ms();
    {
        let mut states = p.states.lock().unwrap();
        let st = states.entry(dev.into()).or_default();
        st.last_cue = Some(("停止播放".into(), now - 4_000));
    }
    assert!(p.retreat_is_own_overlap(dev, "停止播放", &retreat_provenance()));
    assert!(p.retreat_is_own_overlap(dev, "退下", &retreat_provenance()));
}

/// Serial 92 regression (dot2-d 2026-11-21 16:04:34): when `self_heard=true`
/// and a voice TTS output is live whose text does NOT contain the retreat
/// phrase, "退下吧" must be ignored instead of closing the session.
#[tokio::test]
async fn self_heard_retreat_during_unrelated_live_voice_does_not_close_session() {
    let p = pipeline();
    let dev = "dot2-d";
    let now = id::now_ms();
    let ident = p.open_participation(dev, CreditGrant::Full);
    {
        let mut states = p.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        let req = st.participation.begin_request(ident, now, false).unwrap();
        st.outputs.insert(
            "tts-cui".into(),
            OutputRecord {
                answer_text: "崔永元的崔加一个单人旁就是催促的催字。".into(),
                request: Some(req),
                task_id: None,
                channel: "voice".into(),
                kind: "answer".into(),
                content_ref: "tts".into(),
                stage: "streaming".into(),
                started_ms: now,
                stage_ms: now,
                end_sent_ms: None,
                expected_ms: 0,
                synth_ms: 0,
            },
        );
    }
    assert_eq!(
        p.reflex_intent("停止播放"),
        Some(super::interaction::KeywordIntent::Retreat),
        "test precondition: 停止播放 must be a configured Retreat word"
    );
    p.on_keyword(dev, "停止播放", now, Some(1374), true, retreat_provenance());
    assert_eq!(
        p.states.lock().unwrap()[dev].participation.close_reason,
        None,
        "self_heard=true retreat during active voice playback must not close session"
    );
    assert!(
        p.recent(20)
            .iter()
            .any(|e| e["kind"] == "keyword_ignored" && e["reason"] == "self_heard_retreat_overlap"),
        "expected self_heard_retreat_overlap ignore event"
    );
}

/// D2: past five seconds a cue is old news — the retreat phrase is the
/// person's and must act (no veto from a stale echo).
#[test]
fn retreat_overlap_ignores_a_stale_last_cue() {
    let p = pipeline();
    let dev = "dot2-d";
    let now = id::now_ms();
    {
        let mut states = p.states.lock().unwrap();
        let st = states.entry(dev.into()).or_default();
        st.last_cue = Some(("停止播放".into(), now - 5_001));
    }
    assert!(!p.retreat_is_own_overlap(dev, "停止播放", &retreat_provenance()));
}

/// D2: an idle device has nothing to retreat from. The trace must not claim
/// the self-heard hit was "accepted" and then ignore it for no session.
#[tokio::test]
async fn an_idle_retreat_does_not_log_self_heard_accepted() {
    let p = pipeline();
    let dev = "dot2-d";
    ensure_device(&p, dev);
    assert!(!p.has_active_session_or_playback(dev));
    p.on_keyword(
        dev,
        "停止播放",
        id::now_ms(),
        Some(1_000),
        true,
        retreat_provenance(),
    );
    let events = p.recent(50);
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "keyword_ignored"
                && e["reason"] == "reflex_precondition:no_session"),
        "the idle hit must be recorded as no_session: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e["kind"] == "keyword_self_heard_accepted"),
        "no acceptance may be logged when the retreat does nothing: {events:?}"
    );
}

#[tokio::test]
async fn media_takeover_wake_ack_preserves_followup_across_play_finished() {
    use crate::tool::music::{MediaSession, MediaState, MusicRuntime, Source, Track};
    use tokio::io::AsyncWriteExt;

    struct FixedAsr;
    #[async_trait::async_trait]
    impl crate::speech::AsrClient for FixedAsr {
        async fn transcribe(&self, _wav: &[u8]) -> Result<String, crate::speech::SpeechError> {
            Ok("现在在放什么".into())
        }
        fn provider(&self) -> &str {
            "fixed"
        }
    }

    let store = Arc::new(Store::open_in_memory().unwrap());
    let pipe = VoicePipeline::new(
        DeviceHub::new(),
        store.clone(),
        Arc::new(TaskRegistry::new(store)),
        Arc::new(|| Err("isolated: no agent".into())),
        Arc::new(|| Err("isolated: no TTS".into())),
        Arc::new(|| Ok(Arc::new(FixedAsr))),
        std::env::temp_dir().join(id::next("hm-takeover")),
    );
    let mut mic_rx = pipe.mic_rx.lock().unwrap().take().unwrap();
    let dev = "dot2-d";
    let now = id::now_ms();
    let music = MusicRuntime::new(pipe.store.clone(), pipe.hub.clone());
    pipe.attach_music(&music);

    // Connect a real mock gateway so hub.active_plays holds a live "media" play without StopReceipt.
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
        gateway: "gw-takeover".into(),
        gateway_version: "0.1.0".into(),
        devices: vec![crate::gateway::HelloDevice {
            stable_id: dev.into(),
            model: "AEOBC".into(),
            firmware: Some("hm-firmware 0.0.53".into()),
            capabilities: vec!["audio_out".into(), "audio_in".into(), "play_stream".into()],
            audio_profile: String::new(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
        auth_token: None,
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

    let _media_ticket = pipe
        .hub
        .play_begin(dev, "media", 16_000, Some("Yes933".into()))
        .unwrap();
    music.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks: vec![Track {
                source: Source::Radio,
                id: "yes933".into(),
                title: "Yes933".into(),
                artist: String::new(),
                binding: "radio|local".into(),
            }],
            index: 0,
            state: MediaState::Playing,
            position_ms: 0,
            play_id: Some(_media_ticket.play_id.clone()),
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: crate::tool::music::RepeatMode::Off,
        },
    );

    // 1) User speaks wake word while radio is actively playing -> control_only=true, wake_ack_overlap=false.
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
    assert!(pipe.mic_arc(dev).is_some_and(|arc| {
        let p = arc.lock().unwrap();
        p.in_speech()
            && p.open_provenance()
                .spans
                .iter()
                .all(|s| s.control_only && !s.wake_ack_overlap)
    }));

    // 2) Production keyword_wake pauses radio (hub media play still active!) and transfers+promotes capture.
    pipe.on_keyword(
        dev,
        "你好高达",
        now + 320,
        Some(320),
        false,
        crate::gateway::InputProvenance::default(),
    );
    assert!(pipe
        .hub
        .active_plays_for(dev)
        .iter()
        .any(|(_, p)| p.channel == "media"));
    assert!(pipe
        .recent(20)
        .iter()
        .any(|e| e["kind"] == "wake_capture_transferred"));
    assert!(
        pipe.mic_arc(dev).unwrap().lock().unwrap().open_provenance().spans
            .iter().all(|s| s.control_only && s.wake_ack_overlap),
        "production restore_wake_capture_locked must promote all pre-wake media spans to wake_ack_overlap"
    );

    // 3) wake_ack plays while stopping hub media play is still registered -> capture_is_wake_ack_only is true.
    pipe.settings.lock().unwrap().tail_blind_ms = 80;
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.entry(dev.into()).or_default();
        st.wake_ack_play = Some("P-ack".into());
        st.outputs.insert(
            "P-ack".into(),
            super::OutputRecord {
                answer_text: "在呢".into(),
                request: None,
                task_id: None,
                channel: "voice".into(),
                kind: "ack".into(),
                content_ref: "ack".into(),
                stage: "streaming".into(),
                started_ms: now + 320,
                stage_ms: now + 320,
                end_sent_ms: None,
                expected_ms: 400,
                synth_ms: 0,
            },
        );
    }
    assert!(
        pipe.capture_is_wake_ack_only(dev, true, false, id::now_ms()),
        "in-flight stopping hub media play must not block capture_is_wake_ack_only"
    );
    for i in 4..8u64 {
        pipe.feed_chunk(
            dev.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }

    // 4) wake_ack finishes: PlayFinished arms real tail_blind_ms + wake_ack_tail_until_ms,
    // speech inside the tail stays wake_ack_overlap, and natural expiry un-gates post-tail speech.
    pipe.handle(crate::gateway::DeviceEvent::PlayFinished {
        device: dev.into(),
        play_id: "P-ack".into(),
        played_ms: 400,
        consumed_ms: Some(380),
        progress_basis: Some("hw_delay".into()),
        ts: now + 720,
    })
    .await;
    assert!(pipe.mic_arc(dev).unwrap().lock().unwrap().in_speech());
    assert!(!pipe
        .recent(20)
        .iter()
        .any(|e| e["kind"] == "mic_echo_segment_reset_on_play_end"));
    {
        let states = pipe.states.lock().unwrap();
        let st = states.get(dev).unwrap();
        let check_now = id::now_ms();
        assert!(
            st.self_play_blind_until_ms > check_now,
            "PlayFinished must arm self_play_blind_until_ms"
        );
        assert!(
            st.wake_ack_tail_until_ms > check_now,
            "PlayFinished must arm wake_ack_tail_until_ms"
        );
    }
    pipe.feed_chunk(
        dev.into(),
        loud.clone(),
        16_000,
        1,
        now + 8 * 80,
        crate::gateway::InputProvenance::default(),
    );
    assert!(
        pipe.mic_arc(dev)
            .unwrap()
            .lock()
            .unwrap()
            .open_provenance()
            .spans
            .last()
            .is_some_and(|s| s.control_only && s.wake_ack_overlap),
        "speech inside the armed post-ack tail window must carry wake_ack_overlap"
    );
    tokio::time::sleep(std::time::Duration::from_millis(160)).await;
    for i in 9..12u64 {
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
        pipe.mic_arc(dev).unwrap().lock().unwrap().open_provenance().spans
            .last().is_some_and(|s| !s.control_only),
        "after natural tail expiry, speech with un-acked hub media pause must no longer be control_only"
    );
    assert!(
        pipe.mic_arc(dev)
            .unwrap()
            .lock()
            .unwrap()
            .open_provenance()
            .allows_wake_ack_followup(),
        "promoted + post-ack speech must pass allows_wake_ack_followup"
    );

    // 5) Real hangover closes VAD segment -> leased InputLease dispatches FixedAsr -> reaches mic_utterance.
    let quiet = vec![0i16; 1280];
    for i in 12..20u64 {
        pipe.feed_chunk(
            dev.into(),
            quiet.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    let t = tokio::time::timeout(std::time::Duration::from_secs(2), mic_rx.recv())
        .await
        .expect("ASR transcript")
        .expect("mic_tx open");
    assert!(
        t.leased && t.control_only && t.window_epoch == 1 && t.ticket.allows_wake_ack_followup()
    );
    pipe.handle_mic_transcript(t).await;
    let events = pipe.recent(40);
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "mic_utterance" && e["text"] == "现在在放什么"),
        "expected follow-up question to reach mic_utterance: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e["kind"].as_str(),
            Some(
                "mic_ignored_unwoken"
                    | "mic_control_only_rejected"
                    | "transcript_stale_after_epoch"
            )
        )),
        "follow-up must not be dropped by epoch, control_only, or media gate: {events:?}"
    );
}

#[tokio::test]
async fn bathroom_dot2a_just_ended_presence_and_misrecognized_bare_wake_regressions() {
    let pipe = pipeline();
    let dev = "dot2a";
    let peer = "dot2-c";
    ensure_device(&pipe, dev);
    ensure_device(&pipe, peer);
    let now = id::now_ms();

    // 1. Speech NearEndPresence (-26.0 dBFS, SNR 32.0) followed by a trailing `just_ended`
    // quiet frame (-54.0 dBFS, SNR 4.0 < MIN_SNR_DB) must preserve the -26.0 dBFS speech
    // peak in both `mic_levels` and `arbiter.passive_presence`.
    pipe.handle(crate::gateway::DeviceEvent::NearEndPresence {
        device: dev.into(),
        ts_ms: now,
        duration_ms: 960,
        rms_dbfs: Some(-26.0),
        noise_floor_dbfs: Some(-58.0),
        snr_db: Some(32.0),
        clipped: false,
        echo_health: Some("healthy".into()),
        profile_version: Some("dot2-array-postaec-v1".into()),
    })
    .await;
    pipe.handle(crate::gateway::DeviceEvent::NearEndPresence {
        device: dev.into(),
        ts_ms: now + 80,
        duration_ms: 1040,
        rms_dbfs: Some(-54.0),
        noise_floor_dbfs: Some(-58.0),
        snr_db: Some(4.0),
        clipped: false,
        echo_health: Some("healthy".into()),
        profile_version: Some("dot2-array-postaec-v1".into()),
    })
    .await;
    let snap = pipe.fresh_mic_level(dev).expect("fresh mic level");
    assert_eq!(snap.rms_dbfs, Some(-26.0));
    assert_eq!(snap.snr_db, Some(32.0));
    let passive = pipe
        .arbiter
        .passive_presence_for(dev)
        .expect("passive presence preserved");
    assert_eq!(passive.rms_dbfs, Some(-26.0));

    // 2. Peer `dot2-c` records passive presence on an unwoken bleed segment ("我在说拜拜啊。"),
    // then ASR dismisses it as `mic_ignored_unwoken` -> `passive_presence` is cleared immediately.
    pipe.handle(crate::gateway::DeviceEvent::NearEndPresence {
        device: peer.into(),
        ts_ms: now,
        duration_ms: 1200,
        rms_dbfs: Some(-35.0),
        noise_floor_dbfs: Some(-55.0),
        snr_db: Some(20.0),
        clipped: false,
        echo_health: Some("healthy".into()),
        profile_version: Some("dot2-array-postaec-v1".into()),
    })
    .await;
    assert!(pipe.arbiter.passive_presence_for(peer).is_some());
    pipe.handle_mic_transcript(transcript(peer, "我在说拜拜啊。", 1))
        .await;
    assert!(
        pipe.arbiter.passive_presence_for(peer).is_none(),
        "dismissed unwoken transcript must clear peer passive_presence"
    );

    // 3. Any KWS wake-transferred segment (short like "还显不显示？" at 1600 ms or
    // long like "游戏画面，看到没有..." at 12000 ms) that contains NO wake word in
    // the ASR transcript (`c == text`) is rejected by `kws_wake_rejected_by_asr`
    // and silently retreats the session to Idle without playing wake_ack or calling the LLM.
    let loud = vec![6000i16; 1280];
    for i in 0..4u64 {
        pipe.feed_chunk(
            dev.into(),
            loud.clone(),
            16_000,
            1,
            now + 200 + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    pipe.on_keyword(
        dev,
        "你好高达",
        now + 520,
        Some(320),
        false,
        crate::gateway::InputProvenance::default(),
    );
    let transferred_id = {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        assert_eq!(st.window_opener, Some("keyword"));
        let id = st
            .last_wake_transferred_input_id
            .expect("transferred input id");
        let input = st.input.as_mut().expect("transferred input lease");
        input.recognizing = true;
        st.mic_counters.asr_inflight += 1;
        id
    };
    let mut t = transcript(dev, "还显不显示？", transferred_id);
    t.leased = true;
    t.window_epoch = 1;
    t.duration_ms = 1600;
    pipe.handle_mic_transcript(t).await;
    let events = pipe.recent(30);
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "kws_wake_rejected_by_asr" && e["device"] == dev),
        "short unconfirmed wake-transferred segment must be rejected by ASR secondary confirmation: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e["kind"] == "request_started" || e["kind"] == "wake_ack"),
        "rejected short KWS false wake must not play wake_ack or start an LLM turn: {events:?}"
    );

    let bedroom = "dot2-bedroom";
    ensure_device(&pipe, bedroom);
    for i in 0..4u64 {
        pipe.feed_chunk(
            bedroom.into(),
            loud.clone(),
            16_000,
            1,
            now + 1000 + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    pipe.on_keyword(
        bedroom,
        "你好高达",
        now + 1320,
        Some(320),
        false,
        crate::gateway::InputProvenance::default(),
    );
    let peer_transferred_id = {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(bedroom).unwrap();
        assert_eq!(st.window_opener, Some("keyword"));
        let id = st
            .last_wake_transferred_input_id
            .expect("peer transferred id");
        let input = st.input.as_mut().expect("peer transferred input lease");
        input.recognizing = true;
        st.mic_counters.asr_inflight += 1;
        id
    };
    let mut tv_t = transcript(
        bedroom,
        "游戏画面，看到没有？就是不知道什么时候咱们这里开始推送这样的版本。据说这个看起来像实际接近的画面，全部是通过摄像头采集画面之后，再用算法像游戏画面一样把。",
        peer_transferred_id,
    );
    tv_t.leased = true;
    tv_t.window_epoch = 1;
    tv_t.duration_ms = 12_000;
    pipe.handle_mic_transcript(tv_t).await;
    let peer_events = pipe.recent(30);
    assert!(
        peer_events
            .iter()
            .any(|e| e["kind"] == "kws_wake_rejected_by_asr" && e["device"] == bedroom),
        "unconfirmed long wake-transferred segment must emit kws_wake_rejected_by_asr: {peer_events:?}"
    );
    assert!(
        !peer_events.iter().any(|e| e["kind"] == "request_started"),
        "rejected KWS false wake must not start an LLM turn: {peer_events:?}"
    );
    assert_eq!(
        pipe.states
            .lock()
            .unwrap()
            .get(bedroom)
            .unwrap()
            .participation
            .session
            .state,
        SessionState::Idle,
        "rejected KWS false wake must silently close the session back to Idle"
    );
}

#[tokio::test]
async fn xiaoai_3tier_wake_suspect_drop_mid_segment_bare_wake_and_oneshot_slice() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ScriptedAsr {
        calls: Arc<AtomicUsize>,
        replies: Arc<Mutex<VecDeque<Result<String, String>>>>,
    }
    #[async_trait::async_trait]
    impl crate::speech::AsrClient for ScriptedAsr {
        async fn transcribe(&self, _wav: &[u8]) -> Result<String, crate::speech::SpeechError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(String::new()))
            {
                Ok(text) => Ok(text),
                Err(msg) => Err(crate::speech::SpeechError::Transport(msg)),
            }
        }
        fn provider(&self) -> &str {
            "scripted-test-asr"
        }
    }

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    let asr_calls = Arc::new(AtomicUsize::new(0));
    let asr_replies = Arc::new(Mutex::new(VecDeque::from([
        Ok("现在几点了".into()), // d4: oneshot sliced command
        Ok("咳。".into()),       // d7: oneshot sliced filler
        Ok(String::new()),       // d8: oneshot sliced empty/unusable
        Ok("明天天气".into()),   // d10: pre-arbitration cursor survival
        Ok("据我所知。".into()), // d11: HM_WAKE_TIMELINE_SLICE=0 rollback
    ])));
    let asr_calls_f = asr_calls.clone();
    let asr_replies_f = asr_replies.clone();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let hub = DeviceHub::new();
    let music = crate::tool::music::MusicRuntime::new(store.clone(), hub.clone());
    let pipe = VoicePipeline::new(
        hub,
        store.clone(),
        Arc::new(TaskRegistry::new(store)),
        Arc::new(|| Err("isolated: no agent".into())),
        Arc::new(|| Err("isolated: no TTS".into())),
        Arc::new(move || {
            Ok(Arc::new(ScriptedAsr {
                calls: asr_calls_f.clone(),
                replies: asr_replies_f.clone(),
            }))
        }),
        std::env::temp_dir().join(id::next("hm-xiaoai-3tier")),
    );
    pipe.attach_music(&music);
    {
        let mut s = pipe.settings.lock().unwrap();
        s.follow_window_enabled = true;
        s.proactive_asr_wake = false;
        s.wake_words = vec!["你好小智".into()];
    }
    pipe.project_follow_window_policy();
    let mut mic_rx = pipe.mic_rx.lock().unwrap().take().expect("mic_rx");

    let now = id::now_ms();
    let loud = vec![10_000i16; 1280];
    let quiet = vec![0i16; 1280];

    // 1) Tier-3 Suspect drop with proactive_asr_wake=false:
    //    zero session activation, zero ASR call, kws_suspect_wake_dropped emitted.
    let d1 = "dot2-suspect";
    ensure_device(&pipe, d1);
    for i in 0..6u64 {
        pipe.feed_chunk(
            d1.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    pipe.on_keyword_at(
        d1,
        "你好小智",
        now + 480,
        Some(480),
        false,
        crate::gateway::InputProvenance::default(),
        Some(-22.0),
        Some(40),
        WakeAcoustics {
            tier: Some("suspect".into()),
            wake_dur_ms: Some(180),
            max_gap_ms: Some(420),
            l2_passed: Some(false),
            wake_end_cursor: None,
        },
        false,
    );
    assert_eq!(
        pipe.states.lock().unwrap()[d1].participation.session.state,
        SessionState::Idle,
        "suspect KWS wake must not open a session"
    );
    assert_eq!(
        pipe.states.lock().unwrap()[d1]
            .mic_counters
            .kws_suspect_dropped,
        1
    );
    for i in 6..18u64 {
        pipe.feed_chunk(
            d1.into(),
            quiet.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    assert_eq!(
        asr_calls.load(Ordering::SeqCst),
        0,
        "suspect wake with proactive_asr_wake=false must skip ASR completely"
    );
    assert!(
        pipe.recent(30).iter().any(|e| {
            e["kind"] == "kws_suspect_wake_dropped"
                && e["device"] == d1
                && e["reason"] == "l2_or_duration_rejected"
        }),
        "must emit kws_suspect_wake_dropped"
    );

    // 2) Tier-3 Mid-segment idle drop (e.g. 2.4 s TV monologue before a real KWS hit):
    let d2 = "dot2-midseg";
    ensure_device(&pipe, d2);
    for i in 0..30u64 {
        pipe.feed_chunk(
            d2.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    pipe.on_keyword_at(
        d2,
        "你好小智",
        now + 2400,
        Some(2400),
        false,
        crate::gateway::InputProvenance::default(),
        Some(-20.0),
        Some(40),
        WakeAcoustics {
            tier: Some("real".into()),
            wake_dur_ms: Some(720),
            max_gap_ms: Some(120),
            l2_passed: Some(true),
            wake_end_cursor: None,
        },
        false,
    );
    assert_eq!(
        pipe.states.lock().unwrap()[d2].participation.session.state,
        SessionState::Idle,
        "mid-segment idle KWS trigger (>800 ms pre-wake speech) must be dropped silently"
    );
    assert!(
        pipe.recent(30).iter().any(|e| {
            e["kind"] == "kws_suspect_wake_dropped"
                && e["device"] == d2
                && e["reason"] == "mid_segment_wake_trigger"
        }),
        "must emit mid_segment_wake_trigger"
    );

    // 3) Tier-2 wakeup-normal (Bare Wake): real hit + silence -> skip ASR, confirm locally.
    let d3 = "dot2-bare";
    ensure_device(&pipe, d3);
    for i in 0..9u64 {
        pipe.feed_chunk(
            d3.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    pipe.on_keyword_at(
        d3,
        "你好小智",
        now + 720,
        Some(720),
        false,
        crate::gateway::InputProvenance::default(),
        Some(-20.0),
        Some(0),
        WakeAcoustics {
            tier: Some("real".into()),
            wake_dur_ms: Some(720),
            max_gap_ms: Some(120),
            l2_passed: Some(true),
            wake_end_cursor: None,
        },
        false,
    );
    assert_eq!(
        pipe.states.lock().unwrap()[d3].participation.session.state,
        SessionState::Listening
    );
    for i in 9..20u64 {
        pipe.feed_chunk(
            d3.into(),
            quiet.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    assert_eq!(
        asr_calls.load(Ordering::SeqCst),
        0,
        "verified bare wake must NOT call ASR"
    );
    assert_eq!(
        pipe.states.lock().unwrap()[d3]
            .mic_counters
            .bare_wake_asr_skipped,
        1
    );
    assert_eq!(
        pipe.states.lock().unwrap()[d3].participation.session.state,
        SessionState::Listening,
        "bare wake must keep Listening follow-up window open"
    );
    assert!(
        pipe.recent(30)
            .iter()
            .any(|e| e["kind"] == "kws_bare_wake_confirmed" && e["device"] == d3),
        "must emit kws_bare_wake_confirmed"
    );

    // Verify mode_snapshot() exposes both new counters on live devices.
    let snap = pipe.mode_snapshot();
    let devs = snap["devices"].as_array().expect("devices array");
    let snap_d1 = devs.iter().find(|d| d["device"] == d1).expect("d1 in snap");
    assert_eq!(snap_d1["mic"]["counters"]["kws_suspect_dropped"], json!(1));
    let snap_d3 = devs.iter().find(|d| d["device"] == d3).expect("d3 in snap");
    assert_eq!(
        snap_d3["mic"]["counters"]["bare_wake_asr_skipped"],
        json!(1)
    );

    // 4) Tier-2 wakeup-oneshot (CommandSliced): real hit + 6 command blocks ->
    //    sliced audio sent to ASR, wake-free command "现在几点了" admitted directly.
    let d4 = "dot2-oneshot";
    ensure_device(&pipe, d4);
    for i in 0..8u64 {
        pipe.feed_chunk(
            d4.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    pipe.on_keyword_at(
        d4,
        "你好小智",
        now + 640,
        Some(640),
        false,
        crate::gateway::InputProvenance::default(),
        Some(-20.0),
        Some(0),
        WakeAcoustics {
            tier: Some("real".into()),
            wake_dur_ms: Some(640),
            max_gap_ms: Some(120),
            l2_passed: Some(true),
            wake_end_cursor: None,
        },
        false,
    );
    for i in 8..14u64 {
        pipe.feed_chunk(
            d4.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    for i in 14..26u64 {
        pipe.feed_chunk(
            d4.into(),
            quiet.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    let tx_msg = tokio::time::timeout(std::time::Duration::from_secs(2), mic_rx.recv())
        .await
        .expect("oneshot ASR transcript timeout")
        .expect("oneshot ASR transcript");
    assert!(
        tx_msg.wake_sliced,
        "oneshot transcript must be marked wake_sliced=true"
    );
    assert_eq!(asr_calls.load(Ordering::SeqCst), 1);
    pipe.handle_mic_transcript(tx_msg).await;
    let evs = pipe.recent(40);
    assert!(
        evs.iter()
            .any(|e| e["kind"] == "kws_oneshot_sliced_admitted" && e["device"] == d4),
        "sliced oneshot command must be admitted without wake word in transcript: {evs:?}"
    );
    assert!(
        !evs.iter()
            .any(|e| e["kind"] == "kws_wake_rejected_by_asr" && e["device"] == d4),
        "sliced oneshot command must NOT be rejected by kws_wake_rejected_by_asr"
    );

    // 5) Orthogonal proactive_asr_wake=true: suspect wake still drops before arbitration,
    //    while the idle VAD segment stays `asr_fallback` and wakes ONLY if ASR matches wake word.
    {
        pipe.settings.lock().unwrap().proactive_asr_wake = true;
    }
    let d5 = "dot2-suspect-fallback";
    ensure_device(&pipe, d5);
    for i in 0..6u64 {
        pipe.feed_chunk(
            d5.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    pipe.on_keyword_at(
        d5,
        "你好小智",
        now + 480,
        Some(480),
        false,
        crate::gateway::InputProvenance::default(),
        Some(-22.0),
        Some(40),
        WakeAcoustics {
            tier: Some("suspect".into()),
            wake_dur_ms: Some(180),
            max_gap_ms: Some(420),
            l2_passed: Some(false),
            wake_end_cursor: None,
        },
        false,
    );
    assert_eq!(
        pipe.states.lock().unwrap()[d5].participation.session.state,
        SessionState::Idle,
        "suspect wake must stay idle before ASR even when proactive_asr_wake=true"
    );
    assert_eq!(
        pipe.states.lock().unwrap()[d5]
            .input
            .as_ref()
            .map(|i| i.eligibility),
        Some("asr_fallback"),
        "when proactive_asr_wake=true, idle segment remains eligible for ASR fallback"
    );
    {
        pipe.settings.lock().unwrap().proactive_asr_wake = false;
    }

    // 6) Non-wake reflex intent (Retreat) is unaffected by kws_tier="suspect".
    let d6 = "dot2-retreat";
    ensure_device(&pipe, d6);
    pipe.open_participation(d6, CreditGrant::Full);
    pipe.claim_sovereignty(d6, now);
    pipe.on_keyword_at(
        d6,
        "停止播放",
        now + 500,
        Some(500),
        false,
        crate::gateway::InputProvenance::default(),
        Some(-18.0),
        Some(20),
        WakeAcoustics {
            tier: Some("suspect".into()),
            wake_dur_ms: Some(220),
            max_gap_ms: Some(120),
            l2_passed: Some(false),
            wake_end_cursor: None,
        },
        false,
    );
    assert_eq!(
        pipe.states.lock().unwrap()[d6].participation.close_reason,
        Some(participation::CloseReason::ExplicitExit),
        "Retreat reflex must execute regardless of kws_tier=suspect"
    );

    // 7) Real end-to-end CommandSliced + post-wake filler ("咳。") consumes keyword window
    //    and confirms bare wake instead of leaving the user silent.
    let d7 = "dot2-sliced-filler";
    ensure_device(&pipe, d7);
    for i in 0..8u64 {
        pipe.feed_chunk(
            d7.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    pipe.on_keyword_at(
        d7,
        "你好小智",
        now + 640,
        Some(640),
        false,
        crate::gateway::InputProvenance::default(),
        Some(-20.0),
        Some(0),
        WakeAcoustics {
            tier: Some("real".into()),
            wake_dur_ms: Some(640),
            max_gap_ms: Some(120),
            l2_passed: Some(true),
            wake_end_cursor: None,
        },
        false,
    );
    for i in 8..12u64 {
        pipe.feed_chunk(
            d7.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    for i in 12..24u64 {
        pipe.feed_chunk(
            d7.into(),
            quiet.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    let filler_tx = tokio::time::timeout(std::time::Duration::from_secs(2), mic_rx.recv())
        .await
        .expect("filler ASR transcript timeout")
        .expect("filler ASR transcript");
    assert!(filler_tx.wake_sliced);
    pipe.handle_mic_transcript(filler_tx).await;
    assert!(
        pipe.recent(30).iter().any(|e| {
            e["kind"] == "kws_bare_wake_confirmed"
                && e["device"] == d7
                && e["reason"] == "post_wake_filler_only"
        }),
        "post-wake filler on wake_sliced segment must confirm bare wake"
    );

    // 8) Real end-to-end CommandSliced + empty/unusable ASR still triggers wake ack fallback.
    let d8 = "dot2-sliced-empty";
    ensure_device(&pipe, d8);
    for i in 0..8u64 {
        pipe.feed_chunk(
            d8.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    pipe.on_keyword_at(
        d8,
        "你好小智",
        now + 640,
        Some(640),
        false,
        crate::gateway::InputProvenance::default(),
        Some(-20.0),
        Some(0),
        WakeAcoustics {
            tier: Some("real".into()),
            wake_dur_ms: Some(640),
            max_gap_ms: Some(120),
            l2_passed: Some(true),
            wake_end_cursor: None,
        },
        false,
    );
    for i in 8..12u64 {
        pipe.feed_chunk(
            d8.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    for i in 12..24u64 {
        pipe.feed_chunk(
            d8.into(),
            quiet.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    let empty_tx = tokio::time::timeout(std::time::Duration::from_secs(2), mic_rx.recv())
        .await
        .expect("empty ASR transcript timeout")
        .expect("empty ASR transcript");
    assert!(empty_tx.wake_sliced);
    pipe.handle_mic_transcript(empty_tx).await;
    tokio::task::yield_now().await;
    assert!(
        pipe.recent(30).iter().any(|e| {
            e["kind"] == "mic_segment_empty" && e["device"] == d8 && e["wake_sliced"] == json!(true)
        }),
        "empty post-wake slice must record wake_sliced=true"
    );
    assert_eq!(
        pipe.states.lock().unwrap()[d8].window_opener,
        None,
        "empty post-wake slice must consume keyword window to fire wake ack fallback"
    );

    // 9) Playback-active wakes (both `suspect` and `real` with >800 ms pre-wake echo)
    //    must NOT be blocked by Tier-3 PreWakeup drop, and media takeover must NOT arm wake_slice.
    let d9 = "dot2-media-wake";
    ensure_device(&pipe, d9);
    music.seed_media_session_for_test(
        d9,
        crate::tool::music::MediaSession {
            tracks: vec![crate::tool::music::Track {
                source: crate::tool::music::Source::Library,
                id: "m1".into(),
                title: "夜曲".into(),
                artist: "周杰伦".into(),
                binding: "lib".into(),
            }],
            index: 0,
            state: crate::tool::music::MediaState::Playing,
            position_ms: 5_000,
            play_id: None,
            started_ms: now,
            base_ms: 5_000,
            updated_ms: now,
            repeat: crate::tool::music::RepeatMode::Off,
        },
    );
    for i in 0..20u64 {
        pipe.feed_chunk(
            d9.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    pipe.on_keyword_at(
        d9,
        "你好小智",
        now + 1600,
        Some(1600),
        false,
        crate::gateway::InputProvenance::default(),
        Some(-18.0),
        Some(20),
        WakeAcoustics {
            tier: Some("suspect".into()),
            wake_dur_ms: Some(320),
            max_gap_ms: Some(150),
            l2_passed: Some(false),
            wake_end_cursor: None,
        },
        false,
    );
    assert_eq!(
        pipe.media_state(d9),
        Some(crate::tool::music::MediaState::Paused),
        "playback-active suspect wake must NOT be dropped; it must pause media and wake"
    );
    assert_eq!(
        pipe.states.lock().unwrap()[d9].participation.session.state,
        SessionState::Listening
    );
    // Also verify playback-active real wake with >800 ms pre-wake audio is NOT dropped
    // and does NOT arm wake_cut_cursor on media takeover.
    let d9b = "dot2-media-real";
    ensure_device(&pipe, d9b);
    music.seed_media_session_for_test(
        d9b,
        crate::tool::music::MediaSession {
            tracks: vec![crate::tool::music::Track {
                source: crate::tool::music::Source::Library,
                id: "m2".into(),
                title: "稻香".into(),
                artist: "周杰伦".into(),
                binding: "lib".into(),
            }],
            index: 0,
            state: crate::tool::music::MediaState::Playing,
            position_ms: 8_000,
            play_id: None,
            started_ms: now,
            base_ms: 8_000,
            updated_ms: now,
            repeat: crate::tool::music::RepeatMode::Off,
        },
    );
    for i in 0..30u64 {
        pipe.feed_chunk(
            d9b.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    pipe.on_keyword_at(
        d9b,
        "你好小智",
        now + 2400,
        Some(2400),
        false,
        crate::gateway::InputProvenance::default(),
        Some(-18.0),
        Some(20),
        WakeAcoustics {
            tier: Some("real".into()),
            wake_dur_ms: Some(700),
            max_gap_ms: Some(120),
            l2_passed: Some(true),
            wake_end_cursor: None,
        },
        false,
    );
    assert_eq!(
        pipe.media_state(d9b),
        Some(crate::tool::music::MediaState::Paused),
        "playback-active real wake with >800 ms pre-wake audio must not be blocked"
    );

    // 10) Pre-arbitration cursor capture survives an arbitration delay while post-wake
    //     command blocks arrive during the 200 ms arbitration window.
    let d10 = "dot2-arb-cursor";
    ensure_device(&pipe, d10);
    for i in 0..8u64 {
        pipe.feed_chunk(
            d10.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    let captured_cursor = pipe
        .mic_arc(d10)
        .unwrap()
        .lock()
        .unwrap()
        .current_block_cursor();
    // Simulate 4 post-wake command blocks (320 ms) arriving during the arbitration window
    // BEFORE `on_keyword_at(..., arbitrated=true)` re-enters with the preserved cursor.
    for i in 8..12u64 {
        pipe.feed_chunk(
            d10.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    pipe.on_keyword_at(
        d10,
        "你好小智",
        now + 640,
        Some(640),
        false,
        crate::gateway::InputProvenance::default(),
        Some(-20.0),
        Some(0),
        WakeAcoustics {
            tier: Some("real".into()),
            wake_dur_ms: Some(640),
            max_gap_ms: Some(120),
            l2_passed: Some(true),
            wake_end_cursor: Some(captured_cursor),
        },
        true,
    );
    for i in 12..24u64 {
        pipe.feed_chunk(
            d10.into(),
            quiet.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    let arb_tx = tokio::time::timeout(std::time::Duration::from_secs(2), mic_rx.recv())
        .await
        .expect("arbitrated oneshot transcript timeout")
        .expect("arbitrated oneshot transcript");
    assert!(
        arb_tx.wake_sliced,
        "command blocks arriving during 200 ms arbitration window must still classify as CommandSliced using pre-arbitration cursor"
    );
    pipe.handle_mic_transcript(arb_tx).await;
    assert!(
        pipe.recent(30)
            .iter()
            .any(|e| e["kind"] == "kws_oneshot_sliced_admitted" && e["device"] == d10),
        "arbitrated oneshot command must be admitted via pre-arbitration cursor"
    );

    // 11) Rollback switch HM_WAKE_TIMELINE_SLICE=0 disables Tier-2 slicing and restores
    //     legacy full-segment ASR + kws_wake_rejected_by_asr.
    {
        let _guard = EnvGuard {
            key: "HM_WAKE_TIMELINE_SLICE",
            prev: std::env::var("HM_WAKE_TIMELINE_SLICE").ok(),
        };
        std::env::set_var("HM_WAKE_TIMELINE_SLICE", "0");
        assert!(!wake_timeline_slice_enabled());
        let d11 = "dot2-rollback";
        ensure_device(&pipe, d11);
        for i in 0..9u64 {
            pipe.feed_chunk(
                d11.into(),
                loud.clone(),
                16_000,
                1,
                now + i * 80,
                crate::gateway::InputProvenance::default(),
            );
        }
        pipe.on_keyword_at(
            d11,
            "你好小智",
            now + 720,
            Some(720),
            false,
            crate::gateway::InputProvenance::default(),
            Some(-20.0),
            Some(0),
            WakeAcoustics {
                tier: Some("real".into()),
                wake_dur_ms: Some(720),
                max_gap_ms: Some(120),
                l2_passed: Some(true),
                wake_end_cursor: None,
            },
            false,
        );
        for i in 9..20u64 {
            pipe.feed_chunk(
                d11.into(),
                quiet.clone(),
                16_000,
                1,
                now + i * 80,
                crate::gateway::InputProvenance::default(),
            );
        }
        let rb_tx = tokio::time::timeout(std::time::Duration::from_secs(2), mic_rx.recv())
            .await
            .expect("rollback ASR transcript timeout")
            .expect("rollback ASR transcript");
        assert!(
            !rb_tx.wake_sliced,
            "HM_WAKE_TIMELINE_SLICE=0 must disable wake slicing"
        );
        pipe.handle_mic_transcript(rb_tx).await;
        assert!(
            pipe.recent(30)
                .iter()
                .any(|e| e["kind"] == "kws_wake_rejected_by_asr" && e["device"] == d11),
            "HM_WAKE_TIMELINE_SLICE=0 must restore legacy kws_wake_rejected_by_asr path"
        );
    }

    // 12) Wire backwards-compat: legacy GatewayFrame::Keyword without kws_tier deserializes cleanly.
    let legacy_json = r#"{"type":"keyword","device":"lx06","ts":1000,"seq":1,"keyword":"你好小智","self_heard":false}"#;
    let decoded: crate::gateway::GatewayFrame = serde_json::from_str(legacy_json).unwrap();
    match decoded {
        crate::gateway::GatewayFrame::Keyword {
            kws_tier,
            wake_dur_ms,
            max_gap_ms,
            l2_passed,
            ..
        } => {
            assert_eq!(kws_tier, None);
            assert_eq!(wake_dur_ms, None);
            assert_eq!(max_gap_ms, None);
            assert_eq!(l2_passed, None);
        }
        other => panic!("unexpected frame: {other:?}"),
    }

    // 13) Cross-lock race regression: VAD segment closes inside MicProcessor (in_speech=false,
    //     mic.last_close_ms=Some(now)) while st.mic_last_close_ms is still None (fresh boot).
    //     on_keyword_at must still detect bare_wake_closed and spawn wake_ack_after_keyword_window.
    let d13 = "dot2-race-boot";
    ensure_device(&pipe, d13);
    for i in 0..8u64 {
        pipe.feed_chunk(
            d13.into(),
            loud.clone(),
            16_000,
            1,
            now + i * 80,
            crate::gateway::InputProvenance::default(),
        );
    }
    // Close VAD directly inside MicProcessor WITHOUT letting handle_mic_outcome stamp
    // st.mic_last_close_ms, reproducing the exact 2 ms cross-lock race from dot2-d seq 32..34.
    {
        let arc = pipe.mic_arc(d13).unwrap();
        let mut p = arc.lock().unwrap();
        let mut closed = false;
        for _ in 0..12 {
            if matches!(
                p.feed(quiet.clone(), 16_000),
                crate::mic::FeedOutcome::Segment(Some(_))
            ) {
                closed = true;
                break;
            }
        }
        assert!(closed, "segment must close inside MicProcessor");
        assert!(!p.in_speech());
        assert!(p.last_close_ms().is_some());
    }
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(d13).unwrap();
        st.mic_last_close_ms = None;
        st.input = None;
    }
    pipe.on_keyword_at(
        d13,
        "你好小智",
        id::now_ms(),
        Some(1200),
        false,
        crate::gateway::InputProvenance::default(),
        Some(-22.0),
        Some(480),
        WakeAcoustics {
            tier: Some("real".into()),
            wake_dur_ms: Some(720),
            max_gap_ms: Some(120),
            l2_passed: Some(true),
            wake_end_cursor: None,
        },
        true,
    );
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert!(
        pipe.recent(30)
            .iter()
            .any(|e| e["kind"] == "wake_ack_after_keyword_window" && e["device"] == d13),
        "fresh-boot 2 ms cross-lock race must still trigger wake_ack_after_keyword_window"
    );
}
