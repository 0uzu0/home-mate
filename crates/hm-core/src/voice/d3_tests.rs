use super::*;
#[test]
fn d3_prompt_fields_and_zero_credits_roundtrip() {
    let settings: VoiceSettings = serde_json::from_value({
        let mut value = serde_json::to_value(VoiceSettings::default()).unwrap();
        value["persona_prompt"] = json!("称呼我老板");
        value["system_prompt_override"] = json!("自定义骨架");
        value["followup_credits"] = json!(0);
        value
    })
    .unwrap();
    let value = serde_json::to_value(&settings).unwrap();
    assert_eq!(value["persona_prompt"], "称呼我老板");
    assert_eq!(value["system_prompt_override"], "自定义骨架");
    VoicePipeline::validate_settings(&settings).unwrap();
}

/// These suites exercise the PARTICIPATION MECHANISM (tails, credits,
/// generations), so they run with the follow window ON. The product default
/// has been "off" since 2026-11-15 (an answer ends the conversation); the
/// mechanism is what the old cases are about, and testing it through the
/// product default would only re-test the switch.
fn with_follow_window(p: Arc<VoicePipeline>) -> Arc<VoicePipeline> {
    p.settings.lock().unwrap().follow_window_enabled = true;
    p.project_follow_window_policy();
    p
}

fn pipeline() -> Arc<VoicePipeline> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let p = VoicePipeline::new(
        DeviceHub::new(),
        store.clone(),
        Arc::new(TaskRegistry::new(store)),
        Arc::new(|| Err("isolated: no agent".into())),
        Arc::new(|| Err("isolated: no TTS".into())),
        Arc::new(|| Err("isolated: no ASR".into())),
        std::env::temp_dir().join(id::next("hm-d3-unit")),
    );
    with_follow_window(p)
}

#[tokio::test]
async fn d3_old_model_close_cannot_borrow_a_new_generation_or_request() {
    for new_generation in [false, true] {
        let pipe = pipeline();
        let identity = pipe.open_participation("test", CreditGrant::Full);
        let old = pipe.reserve_turn("test", identity).unwrap();
        let old_scope = pipe.states.lock().unwrap()["test"]
            .participation
            .request_cancel_scope(old.request)
            .unwrap();
        if new_generation {
            pipe.close_session("test", Some(old.request.generation))
                .unwrap();
            pipe.open_participation("test", CreditGrant::Full);
        } else {
            let identity = pipe.states.lock().unwrap()["test"].participation.identity();
            pipe.reserve_turn("test", identity).unwrap();
        }
        let sessions = crate::session::SessionStore::new(pipe.store.clone());
        sessions.ensure("dev:test").unwrap();
        sessions
            .append("dev:test", "user", "New request history")
            .unwrap();
        let before = {
            let states = pipe.states.lock().unwrap();
            let st = &states["test"];
            (
                st.participation.identity(),
                st.participation.snapshot(),
                st.stop_epoch,
                st.outputs.keys().cloned().collect::<Vec<_>>(),
            )
        };
        let flag = old_scope.flag();
        // This reaches the execution boundary itself, not just registry's
        // generic pre-cancel guard.
        assert_eq!(
            pipe.retreat_session_guarded(
                "test",
                "",
                "model",
                SessionCloseGuard::Model {
                    flag,
                    confirm: false,
                    call_id: "old-call".into()
                }
            )
            .unwrap_err(),
            "stale_request"
        );
        let after = {
            let states = pipe.states.lock().unwrap();
            let st = &states["test"];
            (
                st.participation.identity(),
                st.participation.snapshot(),
                st.stop_epoch,
                st.outputs.keys().cloned().collect::<Vec<_>>(),
            )
        };
        assert_eq!(after, before);
        assert_eq!(
            sessions.history("dev:test", 100).unwrap()[0]
                .content
                .as_deref(),
            Some("New request history")
        );
        assert!(!pipe
            .recent(100)
            .iter()
            .any(|e| e["kind"] == "session_close_tool"));
    }
}

#[test]
fn d3_phase_and_deadline_project_only_authoritative_answer_tail() {
    let mut owner = Participation::default();
    assert_eq!(owner.snapshot()["phase"], "closed");
    owner
        .activate(
            owner.identity(),
            FollowWindowPolicy::default(),
            CreditGrant::Full,
            1000,
        )
        .unwrap();
    assert_eq!(owner.snapshot()["phase"], "active");
    assert!(
        owner.snapshot()["tail_deadline_ms"].is_null(),
        "first-question timeout is not an answer tail"
    );
    let request = owner.begin_request(owner.identity(), 1100, true).unwrap();
    owner.answer_started(request, "answer");
    owner.finish_request(request, 1200);
    assert_eq!(owner.snapshot()["phase"], "active");
    owner.answer_ended(request, "answer", 49000);
    assert_eq!(owner.snapshot()["phase"], "tail");
    assert_eq!(owner.snapshot()["tail_deadline_ms"], 58000);
    owner.close(owner.identity(), CloseReason::ExplicitExit, 50000);
    assert_eq!(owner.snapshot()["phase"], "closed");
    assert!(owner.snapshot()["tail_deadline_ms"].is_null());
}

#[tokio::test]
async fn stop_device_locked_clears_live_program_and_pause_current_preserves_bookmark() {
    let pipe = pipeline();
    let dev = "test";
    let now = id::now_ms();
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.entry(dev.into()).or_default();
        st.live_program = Some(Program {
            text: "第一段长故事内容。第二段长故事内容。".into(),
            cursor: 0,
            complete: true,
            resumable: true,
            created_ms: now,
            ..Default::default()
        });
    }
    assert!(pipe.program_controllable(dev));
    let dev_snap = |p: &VoicePipeline| {
        p.mode_snapshot()["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["device"] == dev)
            .unwrap()
            .clone()
    };
    assert!(!dev_snap(&pipe)["program"]["live"].is_null());

    // 1) stop_device (via stop_device_locked) must clear st.live_program (#240).
    let _ = pipe.stop_device(dev);
    assert!(
        !pipe.program_controllable(dev),
        "stop_device_locked must clear live_program"
    );
    assert!(dev_snap(&pipe)["program"]["live"].is_null());
    assert!(pipe.states.lock().unwrap()[dev].live_program.is_none());

    // 2) pause_current takes live_program before stop_device_locked and saves paused_program (#240).
    let music = crate::tool::music::MusicRuntime::new(pipe.store.clone(), pipe.hub.clone());
    pipe.attach_music(&music);
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.live_program = Some(Program {
            text: "续播测试第一句。续播测试第二句。".into(),
            cursor: 0,
            complete: true,
            resumable: true,
            created_ms: now,
            ..Default::default()
        });
    }
    assert_eq!(pipe.pause_current(dev, "unit_pause", None), Some("program"));
    {
        let states = pipe.states.lock().unwrap();
        let st = &states[dev];
        assert!(st.live_program.is_none());
        assert!(st.paused_program.is_some());
    }
}

#[tokio::test]
async fn speak_program_interrupted_by_stop_epoch_clears_live_program() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::AsyncWriteExt;
    let store = Arc::new(Store::open_in_memory().unwrap());
    let states_holder: Arc<std::sync::Mutex<Option<Arc<VoicePipeline>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let holder_for_tts = states_holder.clone();
    let bumped = Arc::new(AtomicBool::new(false));
    let bumped_for_tts = bumped.clone();

    struct EpochBumpTts {
        holder: Arc<std::sync::Mutex<Option<Arc<VoicePipeline>>>>,
        bumped: Arc<AtomicBool>,
    }
    #[async_trait::async_trait]
    impl crate::speech::TtsClient for EpochBumpTts {
        async fn synthesize(
            &self,
            _text: &str,
        ) -> Result<crate::speech::TtsAudio, crate::speech::SpeechError> {
            if !self.bumped.swap(true, Ordering::SeqCst) {
                if let Some(p) = self.holder.lock().unwrap().as_ref() {
                    let mut st = p.states.lock().unwrap();
                    let dev_st = st.entry("test".into()).or_default();
                    // Simulate stop_epoch bumped mid-synthesis without clearing live_program
                    // to directly exercise speak_program's interrupted branch cleanup (#240).
                    dev_st.stop_epoch += 1;
                }
            }
            crate::speech::MockTts::default().synthesize(_text).await
        }
        fn provider(&self) -> &str {
            "epoch-bump"
        }
    }

    let pipe = VoicePipeline::new(
        DeviceHub::new(),
        store.clone(),
        Arc::new(TaskRegistry::new(store)),
        Arc::new(|| Err("isolated: no agent".into())),
        Arc::new(move || {
            Ok(Arc::new(EpochBumpTts {
                holder: holder_for_tts.clone(),
                bumped: bumped_for_tts.clone(),
            }))
        }),
        Arc::new(|| Err("isolated: no ASR".into())),
        std::env::temp_dir().join(id::next("hm-d3-speak-prog")),
    );
    *states_holder.lock().unwrap() = Some(pipe.clone());

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
        gateway: "gw-d3".into(),
        gateway_version: "0.1.0".into(),
        devices: vec![crate::gateway::HelloDevice {
            stable_id: "test".into(),
            model: "AEOBC".into(),
            firmware: Some("hm-firmware 0.0.53".into()),
            capabilities: vec!["audio_out".into(), "audio_in".into(), "play_stream".into()],
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
                if device.stable_id == "test" {
                    break;
                }
            }
        }
    })
    .await
    .expect("DeviceJoined");

    let _ = pipe.open_participation("test", CreditGrant::Full);
    let epoch = {
        let mut st = pipe.states.lock().unwrap();
        st.entry("test".into()).or_default().stop_epoch
    };
    let prog = Program {
        text: "第一句很长的故事。第二句很长的故事。".into(),
        cursor: 0,
        complete: true,
        resumable: true,
        origin_task: Some("T-prog-240".into()),
        ..Default::default()
    };
    pipe.clone()
        .speak_program("test".into(), prog, epoch, None)
        .await;

    {
        let st = pipe.states.lock().unwrap();
        let dev_st = &st["test"];
        assert!(
            dev_st.live_program.is_none(),
            "interrupted speak_program must clear stale live_program even after stop_epoch bump"
        );
    }
    let interrupted = pipe
        .recent(20)
        .into_iter()
        .find(|e| e["kind"] == "program_interrupted")
        .expect("program_interrupted event");
    assert_eq!(interrupted["bookmark_kept"], json!(false));
}

#[tokio::test]
async fn reflex_next_stop_and_music_blocks_transcript_respect_lock_order() {
    use crate::tool::music::{MediaSession, MediaState, MusicRuntime, RepeatMode, Source, Track};
    let pipe = pipeline();
    let dev = "test";
    let now = id::now_ms();
    let music = MusicRuntime::new(pipe.store.clone(), pipe.hub.clone());
    pipe.attach_music(&music);
    let ident = pipe.open_participation(dev, CreditGrant::Full);

    music.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks: vec![
                Track {
                    source: Source::Library,
                    id: "t1".into(),
                    title: "第一首".into(),
                    artist: "歌手".into(),
                    binding: "lib|1".into(),
                },
                Track {
                    source: Source::Library,
                    id: "t2".into(),
                    title: "第二首".into(),
                    artist: "歌手".into(),
                    binding: "lib|2".into(),
                },
            ],
            index: 0,
            state: MediaState::Playing,
            position_ms: 0,
            play_id: Some("P-m1".into()),
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::Off,
        },
    );
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.live_program = Some(Program {
            text: "测试节目".into(),
            resumable: true,
            ..Default::default()
        });
    }

    // music_blocks_transcript releases voice.states before querying music.blocks_passive_capture (#232).
    let _ = pipe.music_blocks_transcript(dev, 1, 500);

    // KeywordIntent::Next releases voice.states before checking media_present & registering control (#232).
    let next_res = pipe
        .run_reflex_intent(
            dev,
            "下一首",
            interaction::KeywordIntent::Next,
            "keyword",
            Some(ident),
        )
        .expect("Next intent should succeed with active media");
    assert_eq!(next_res["action"], "next");

    // KeywordIntent::Stop discards live_program, stops device under lock, and stops media outside lock (#232).
    let stop_res = pipe
        .run_reflex_intent(
            dev,
            "停止播放",
            interaction::KeywordIntent::Stop,
            "keyword",
            Some(ident),
        )
        .expect("Stop intent should succeed");
    assert_eq!(stop_res["program_discarded"], json!(true));
    assert!(pipe.states.lock().unwrap()[dev].live_program.is_none());
}

#[tokio::test]
async fn pure_punctuation_transcripts_are_ignored_without_consuming_followup_while_single_char_words_survive(
) {
    for noise in ["", "   ", "。", "，", "？", "！", "…", "...", "，。？！、"] {
        assert!(
            !crate::mic::has_speech_chars(noise),
            "pure punctuation/whitespace {noise:?} must have no speech chars"
        );
    }
    for valid in ["好", "停", "行", "嗯", "1", "a", "。好。"] {
        assert!(
            crate::mic::has_speech_chars(valid),
            "single-char or valid utterance {valid:?} must have speech chars"
        );
    }

    let pipe = pipeline();
    let dev = "dot2-e";
    let now = id::now_ms();
    let ident = pipe.open_participation(dev, CreditGrant::Full);
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        // Complete first request so device is in the post-answer follow-up window with 1 credit.
        let req1 = st.participation.begin_request(ident, now, true).unwrap();
        st.participation.answer_started(req1, "answer");
        st.participation.finish_request(req1, now + 10);
        st.participation.answer_ended(req1, "answer", now + 20);
        st.participation.followup_credits = 1;
    }

    let make_tx = |input_id: u64, text: &str| {
        let window_epoch = pipe.states.lock().unwrap()[dev].stop_epoch;
        MicTranscript {
            leased: false,
            device: dev.into(),
            result: Ok(text.into()),
            duration_ms: 560,
            input_id,
            capture_seq: input_id,
            window_epoch,
            control_only: false,
            wake_sliced: false,
            ticket: crate::mic::CaptureProvenance::legacy(0),
            enqueued_at: std::time::Instant::now(),
        }
    };

    // 1) Pure punctuation ("。", "，？！") in followup window must be dropped as empty:
    // no mic_utterance, no request_started, no credit debit, no watermark advance.
    pipe.handle_mic_transcript(make_tx(1, "。")).await;
    pipe.handle_mic_transcript(make_tx(2, "，？！")).await;
    pipe.handle(crate::gateway::DeviceEvent::Utterance {
        device: dev.into(),
        ts: id::now_ms(),
        seq: 1,
        source: "asr".into(),
        text: "。".into(),
        provenance: crate::gateway::InputProvenance::default(),
    })
    .await;

    {
        let states = pipe.states.lock().unwrap();
        let st = &states[dev];
        assert_eq!(st.mic_counters.segment_empty, 3);
        assert_eq!(st.mic_counters.followup_turns, 0);
        assert_eq!(
            st.participation.followup_credits, 1,
            "pure punctuation must not consume followup credit"
        );
        assert_eq!(
            st.last_transcript_input, 0,
            "pure punctuation must not advance input watermark"
        );
    }
    assert!(
        !pipe.recent(30).iter().any(|e| matches!(
            e["kind"].as_str(),
            Some("mic_utterance" | "request_started")
        )),
        "pure punctuation must never trigger mic_utterance or request_started"
    );

    // 2) Single-char command "停" in the same followup window is NOT dropped and executes stop_intent.
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.live_program = Some(Program {
            text: "正在讲的故事。".into(),
            resumable: true,
            ..Default::default()
        });
    }
    pipe.handle_mic_transcript(make_tx(3, "停")).await;
    assert!(
        pipe.recent(20).iter().any(|e| e["kind"] == "stop_intent"),
        "single-char command '停' must execute stop_intent"
    );
    assert!(pipe.states.lock().unwrap()[dev].live_program.is_none());

    // 3) Single-char confirmation "好" when awaiting close confirmation is NOT dropped and reaches mic_utterance.
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        st.participation.followup_credits = 1;
        let gen = st.participation.generation();
        st.participation.close_confirmation = Some(crate::voice::participation::RequestTicket {
            generation: gen,
            request_id: 1,
            kind: crate::voice::participation::RequestKind::First,
        });
    }
    pipe.handle_mic_transcript(make_tx(4, "好")).await;
    assert!(
        pipe.recent(20)
            .iter()
            .any(|e| e["kind"] == "mic_utterance" && e["text"] == "好"),
        "single-char '好' under pending confirmation must reach mic_utterance"
    );
}

/// Home Assistant actions (`home.set`, e.g. turning off lights or closing
/// curtains) must auto-exit the voice session (`SessionState::Idle`,
/// `CloseReason::SingleRequest`) once the confirmation speech finishes even
/// when `follow_window_enabled = true` and `followup_credits = Some(2)`, and
/// must not stream or append trailing model follow-up questions.
#[tokio::test]
async fn home_action_turn_auto_exits_followup_window_and_drops_trailing_model_question() {
    use crate::config::{ConfigService, HomeBackend, HomeConfig, HomeEntity};
    use crate::llm::{
        Finish, LlmClient, LlmError, LlmEvent, LlmStream, ToolSpec, WireMessage, WireToolCall,
    };
    use axum::{
        routing::{get, post},
        Router,
    };

    struct ScriptedHomeLlm {
        call_no: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl LlmClient for ScriptedHomeLlm {
        async fn chat_stream(
            &self,
            _model: &str,
            _messages: Vec<WireMessage>,
            tools: &[ToolSpec],
        ) -> Result<LlmStream, LlmError> {
            let n = self
                .call_no
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                let wire = tools
                    .iter()
                    .find(|t| t.canonical_name == "home.set")
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| "home.set".into());
                let tc = WireToolCall {
                    id: "call_home_1".into(),
                    name: wire,
                    arguments: r#"{"entity":"客厅灯","state":"off"}"#.into(),
                };
                Ok(Box::pin(futures::stream::iter(vec![LlmEvent::End {
                    finish: Finish::ToolCalls,
                    content: String::new(),
                    tool_calls: vec![tc],
                }])))
            } else {
                Ok(Box::pin(futures::stream::iter(vec![
                    LlmEvent::Delta("已经为您把客厅灯关好了。还需要我做点别的吗？".into()),
                    LlmEvent::End {
                        finish: Finish::Stop,
                        content: "已经为您把客厅灯关好了。还需要我做点别的吗？".into(),
                        tool_calls: Vec::new(),
                    },
                ])))
            }
        }
    }

    // Spawn a minimal fake Home Assistant supporting turn_off + state readback.
    let ha_app = Router::new()
        .route(
            "/api/services/light/turn_off",
            post(|| async { axum::Json(json!([])) }),
        )
        .route(
            "/api/states/light.living",
            get(|| async {
                axum::Json(json!({
                    "entity_id": "light.living",
                    "state": "off",
                    "attributes": { "friendly_name": "客厅灯" }
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ha_base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, ha_app).await;
    });

    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let cfg_svc = ConfigService::new(store.clone());
    cfg_svc
        .save_home(
            HomeConfig {
                backend: HomeBackend::HomeAssistant,
                base_url: ha_base,
                entities: vec![HomeEntity {
                    name: "客厅灯".into(),
                    entity: "light.living".into(),
                }],
                timeout_ms: 2_000,
                retries: 0,
                entity_source: crate::config::EntitySource::Api,
            },
            "test",
        )
        .unwrap();
    cfg_svc.save_home_token(Some("ha-token")).unwrap();

    let tasks = Arc::new(TaskRegistry::new(store.clone()));
    let mut reg = crate::tool::Registry::new();
    let home_rt = crate::tool::home::HomeRuntime::new(store.clone());
    reg.register(Arc::new(crate::tool::home::HomeSetTool::new(home_rt)))
        .unwrap();
    let env_arc = Arc::new(AgentEnv {
        llm: Arc::new(ScriptedHomeLlm {
            call_no: std::sync::atomic::AtomicUsize::new(0),
        }),
        model: "test".into(),
        llm_fallback: None,
        registry: Arc::new(reg),
        sessions: Arc::new(crate::session::SessionStore::new(store.clone())),
        tasks: tasks.clone(),
        traces: Arc::new(crate::trace::TraceRecorder::new(store.clone())),
        memory: crate::memory::MemoryService::new(store.clone()),
        working_memory: crate::working_memory::WorkingMemoryStore::new(store.clone()),
        shared_memory: None,
        user_md: None,
        scope: Default::default(),
        prompt_preferences: Default::default(),
        assistant_name: crate::ASSISTANT_NAME.into(),
        llm_first_event_timeout: std::time::Duration::from_secs(5),
    });
    let pipe = VoicePipeline::new(
        DeviceHub::new(),
        store,
        tasks,
        Arc::new(move || Ok(env_arc.clone())),
        Arc::new(|| Err("isolated: no TTS".into())),
        Arc::new(|| Err("isolated: no ASR".into())),
        std::env::temp_dir().join(id::next("hm-d3-home-auto-exit")),
    );

    // Enable follow-up window with 2 credits on the speaker.
    {
        let mut s = pipe.settings.lock().unwrap();
        s.follow_window_enabled = true;
        s.followup_credits = Some(2);
        s.follow_window_ms = 15_000;
    }
    pipe.project_follow_window_policy();

    let dev = "test";
    let identity = pipe.open_participation(dev, CreditGrant::Full);
    assert_eq!(
        pipe.states.lock().unwrap()[dev]
            .participation
            .followup_credits,
        2
    );
    let ticket = pipe
        .reserve_turn_with_credit(dev, identity, false)
        .expect("reserve first turn");

    pipe.clone()
        .run_device_turn_at(dev.into(), "把客厅灯关了".into(), "voice".into(), ticket)
        .await;

    // Drain any submitted voice answer output via PlayFinished if still present.
    let active_outputs: Vec<String> = pipe.states.lock().unwrap()[dev]
        .outputs
        .keys()
        .cloned()
        .collect();
    for play_id in active_outputs {
        pipe.handle(crate::gateway::DeviceEvent::PlayFinished {
            device: dev.into(),
            play_id,
            played_ms: 800,
            consumed_ms: Some(800),
            progress_basis: None,
            ts: id::now_ms(),
        })
        .await;
    }

    let st = &pipe.states.lock().unwrap()[dev];
    assert_eq!(
        st.participation.session.state,
        SessionState::Idle,
        "session must auto-exit to Idle after executing a Home Assistant action"
    );
    assert_eq!(
        st.participation.close_reason,
        Some(crate::voice::participation::CloseReason::SingleRequest),
        "home action auto-exit must close with SingleRequest reason"
    );
    assert_eq!(
        st.participation.followup_credits, 0,
        "auto-exited session must clear remaining followup credits"
    );

    let sessions = crate::session::SessionStore::new(pipe.store.clone());
    let sid = sessions.device_session_id(dev).unwrap();
    let hist = sessions.history(&sid, 20).unwrap();
    let last_assistant = hist
        .iter()
        .rev()
        .find(|m| m.role == "assistant" && m.tool_calls.is_none())
        .and_then(|m| m.content.as_deref())
        .unwrap_or("");
    assert_eq!(
        last_assistant, "客厅灯已关闭。",
        "trailing model question must not be appended to a pure home.set turn"
    );
}

/// #241 P2-A/B/C: `requested_duplex` recovers from a poisoned `settings` lock,
/// `LoopbackProbeGuard` removes `<HM_LOOP_DIR>/arm` on drop/cancel, and
/// `restore_settings` projects `media_trim_db` to `DeviceHub` and triggers ack
/// re-synthesis when cue phrases change.
#[tokio::test]
async fn voice_mode_requested_duplex_probe_guard_and_restore_projections_241() {
    let pipe = pipeline();
    // 1) LoopbackProbeGuard RAII cleanup on drop (simulating mid-await cancellation).
    let loop_dir = std::env::temp_dir().join(id::next("hm-loop-guard-241"));
    std::env::set_var("HM_LOOP_DIR", &loop_dir);
    let arm_path = loop_dir.join("arm");
    {
        let _guard = VoicePipeline::arm_loopback_probe_guard();
        assert!(
            arm_path.exists(),
            "arm file must exist while LoopbackProbeGuard is alive"
        );
    }
    assert!(
        !arm_path.exists(),
        "Dropping LoopbackProbeGuard must remove <HM_LOOP_DIR>/arm"
    );
    let _ = std::fs::remove_dir_all(&loop_dir);

    // 2) restore_settings projects media_trim_db to DeviceHub.
    let mut restored = VoiceSettings::default();
    restored.media_trim_db = -6.5;
    restored.retreat_ack = "好嘞退下了".into();
    restored
        .duplex_requested
        .insert("dot2-d".into(), "full".into());
    pipe.restore_settings(restored, "test-241")
        .expect("restore_settings");
    assert!(
        (pipe.hub.media_trim_db() - (-6.5)).abs() < 1e-6,
        "restore_settings must project media_trim_db to DeviceHub"
    );
    assert_eq!(pipe.requested_duplex("dot2-d"), "full");

    // 3) requested_duplex uses lock_or_recover and survives a poisoned settings mutex.
    let pipe_poison = pipe.clone();
    let _ = std::thread::spawn(move || {
        let _g = pipe_poison.settings.lock().unwrap();
        panic!("intentional settings lock poison for #241 test");
    })
    .join();
    assert_eq!(pipe.requested_duplex("dot2-d"), "full");
    assert_eq!(pipe.requested_duplex("unknown-dev"), "half");
}
