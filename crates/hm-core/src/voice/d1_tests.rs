//! Isolated behavior checks against the production state owner and event paths.
use super::*;

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
        Arc::new(|| Err("isolated test: no agent".into())),
        Arc::new(|| Err("isolated test: no TTS".into())),
        Arc::new(|| Err("isolated test: no ASR".into())),
        std::env::temp_dir().join(id::next("hm-d1")),
    );
    with_follow_window(p)
}

fn bookmark(system_cut: bool) -> Program {
    Program {
        text: "A paused story.".into(),
        created_ms: id::now_ms(),
        resumable: true,
        complete: true,
        system_cut,
        ..Default::default()
    }
}

fn answer(request: RequestTicket) -> OutputRecord {
    OutputRecord {
        answer_text: String::new(),
        request: Some(request),
        task_id: None,
        channel: "voice".into(),
        kind: "answer".into(),
        content_ref: String::new(),
        stage: "submitted".into(),
        started_ms: 0,
        stage_ms: 0,
        end_sent_ms: Some(0),
        expected_ms: 0,
        synth_ms: 0,
    }
}

#[tokio::test]
async fn d1_timeout_and_exit_have_separate_history_and_bookmark_effects() {
    for system_cut in [false, true] {
        let pipe = pipeline();
        let identity = pipe.open_participation("test", CreditGrant::Full);
        let sessions = crate::session::SessionStore::new(pipe.store.clone());
        let session = sessions.device_session_id("test").unwrap();
        sessions
            .append(&session, "user", "Keep my history")
            .unwrap();
        {
            let mut states = pipe.states.lock().unwrap();
            let st = states.get_mut("test").unwrap();
            st.paused_program = Some(bookmark(system_cut));
            // Reproduce the ticker order: ordinary sweep happens before tone.
            st.follow_open_last_tick = true;
        }
        assert_eq!(pipe.session_sweep_at(id::now_ms() + 60_000), vec!["test"]);
        assert!(pipe.session_sweep_at(id::now_ms() + 60_001).is_empty());
        assert_eq!(sessions.history(&session, 10).unwrap().len(), 1);
        {
            let states = pipe.states.lock().unwrap();
            let st = &states["test"];
            assert_eq!(st.paused_program.is_some(), !system_cut);
            assert_eq!(st.participation.close_reason, Some(CloseReason::Timeout));
        }
        assert!(pipe.reserve_turn("test", identity).is_none());
        pipe.retreat_session("test", "退下", "test");
        let closed = {
            let states = pipe.states.lock().unwrap();
            let st = &states["test"];
            assert!(st.paused_program.is_none());
            assert_eq!(
                st.participation.close_reason,
                Some(CloseReason::ExplicitExit)
            );
            (st.participation.identity(), st.stop_epoch)
        };
        assert!(sessions.history(&session, 10).unwrap().is_empty());
        pipe.retreat_session("test", "退下", "test");
        let states = pipe.states.lock().unwrap();
        assert_eq!(
            (
                states["test"].participation.identity(),
                states["test"].stop_epoch
            ),
            closed
        );
    }
}

#[tokio::test]
async fn d1_tone_sweep_alone_uses_the_same_timeout_cleanup() {
    let pipe = pipeline();
    pipe.open_participation("test", CreditGrant::Full);
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut("test").unwrap();
        st.participation
            .activate(
                st.participation.identity(),
                FollowWindowPolicy {
                    credits: Some(2),
                    ..Default::default()
                },
                CreditGrant::Full,
                id::now_ms() - 60_000,
            )
            .unwrap();
        st.follow_open_last_tick = true;
        st.paused_program = Some(bookmark(true));
    }
    pipe.follow_window_tone_sweep();
    let states = pipe.states.lock().unwrap();
    assert_eq!(
        states["test"].participation.close_reason,
        Some(CloseReason::Timeout)
    );
    assert!(states["test"].paused_program.is_none());
}

#[tokio::test]
async fn d1_stop_and_offline_invalidate_a_pending_admission_snapshot() {
    for offline in [false, true] {
        let pipe = pipeline();
        let identity = pipe.open_participation("test", CreditGrant::Full);
        if offline {
            pipe.on_device_offline("test");
        } else {
            pipe.stop_device("test");
        }
        assert!(pipe.reserve_turn("test", identity).is_none());
    }
}

#[tokio::test]
async fn d1_resume_person_pause_after_timeout_still_works() {
    let pipe = pipeline();
    pipe.open_participation("test", CreditGrant::Full);
    pipe.states
        .lock()
        .unwrap()
        .get_mut("test")
        .unwrap()
        .paused_program = Some(bookmark(false));
    pipe.session_sweep_at(id::now_ms() + 60_000);
    assert!(
        pipe.resume_program("test"),
        "the surviving human bookmark remains usable"
    );
    tokio::task::yield_now().await;
}

#[tokio::test]
async fn d1_all_answer_terminal_paths_release_the_original_request() {
    for terminal in [
        "finished", "failed", "unknown", "stopped", "stall", "stale", "offline",
    ] {
        let pipe = pipeline();
        let identity = pipe.open_participation("test", CreditGrant::Full);
        pipe.states
            .lock()
            .unwrap()
            .get_mut("test")
            .unwrap()
            .participation
            .follow_window
            .enabled = false;
        let ticket = pipe.reserve_turn("test", identity).unwrap();
        {
            let mut states = pipe.states.lock().unwrap();
            let st = states.get_mut("test").unwrap();
            st.participation.answer_started(ticket.request, "play");
            let mut rec = answer(ticket.request);
            if terminal == "stale" {
                rec.stage = "stopping".into();
            }
            st.outputs.insert("play".into(), rec);
            st.finish_window_request(ticket.request, id::now_ms());
            assert!(st.participation.request_running());
        }
        match terminal {
            "finished" => {
                pipe.handle(DeviceEvent::PlayFinished {
                    device: "test".into(),
                    play_id: "play".into(),
                    played_ms: 100,
                    consumed_ms: None,
                    progress_basis: None,
                    ts: id::now_ms(),
                })
                .await
            }
            "failed" | "unknown" => {
                pipe.handle(DeviceEvent::PlayStage {
                    play_id: "play".into(),
                    stage: terminal.into(),
                    detail: None,
                })
                .await
            }
            "stopped" => {
                pipe.handle(DeviceEvent::StopStage {
                    play_id: "play".into(),
                    stage: "device_confirmed".into(),
                    outcome: crate::gateway::StopOutcome::Confirmed {
                        played_ms: 0,
                        consumed_ms: None,
                        progress_basis: None,
                    },
                    played_ms: Some(0),
                    consumed_ms: None,
                    progress_basis: None,
                })
                .await
            }
            "stall" => {
                pipe.play_stall_sweep_at(id::now_ms());
            }
            "stale" => {
                pipe.stale_output_sweep_at(id::now_ms());
            }
            "offline" => pipe.on_device_offline("test"),
            _ => unreachable!(),
        }
        let states = pipe.states.lock().unwrap();
        assert!(
            !states["test"].participation.request_running(),
            "{terminal}"
        );
        assert_eq!(
            states["test"].participation.close_reason,
            Some(CloseReason::SingleRequest),
            "{terminal}"
        );
    }
}

#[tokio::test]
async fn d1_a_answer_end_cannot_clear_b_in_the_event_handler() {
    let pipe = pipeline();
    let first = pipe.open_participation("test", CreditGrant::Full);
    let a = pipe.reserve_turn("test", first).unwrap();
    let current = pipe.states.lock().unwrap()["test"].participation.identity();
    let b = pipe.reserve_turn("test", current).unwrap();
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut("test").unwrap();
        st.participation.answer_started(a.request, "A");
        st.outputs.insert("A".into(), answer(a.request));
        st.finish_window_request(a.request, id::now_ms());
    }
    pipe.handle(DeviceEvent::PlayFinished {
        device: "test".into(),
        play_id: "A".into(),
        played_ms: 100,
        consumed_ms: None,
        progress_basis: None,
        ts: id::now_ms(),
    })
    .await;
    let states = pipe.states.lock().unwrap();
    assert!(states["test"].participation.contains(b.request));
    assert!(!states["test"].participation.contains(a.request));
}

#[tokio::test]
async fn d1_failed_no_audio_turn_finishes_its_reserved_request() {
    let pipe = pipeline();
    let identity = pipe.open_participation("test", CreditGrant::Full);
    pipe.states
        .lock()
        .unwrap()
        .get_mut("test")
        .unwrap()
        .participation
        .follow_window
        .enabled = false;
    let ticket = pipe.reserve_turn("test", identity).unwrap();
    pipe.clone()
        .run_device_turn_at("test".into(), "a question".into(), "test".into(), ticket)
        .await;
    let states = pipe.states.lock().unwrap();
    assert!(!states["test"].participation.request_running());
    assert_eq!(
        states["test"].participation.close_reason,
        Some(CloseReason::SingleRequest)
    );
}

#[tokio::test]
async fn d1_checked_exit_cannot_borrow_a_new_generation_or_request() {
    for new_generation in [false, true] {
        let pipe = pipeline();
        let identity = pipe.open_participation("test", CreditGrant::Full);
        let first = pipe.reserve_turn("test", identity).unwrap();
        let old_exit = pipe.states.lock().unwrap()["test"].participation.identity();
        if new_generation {
            pipe.open_participation("test", CreditGrant::Full);
        }
        let current = pipe.states.lock().unwrap()["test"].participation.identity();
        let second = pipe.reserve_turn("test", current).unwrap();
        let sessions = crate::session::SessionStore::new(pipe.store.clone());
        sessions.ensure("dev:test").unwrap();
        sessions.append("dev:test", "user", "B history").unwrap();
        pipe.states
            .lock()
            .unwrap()
            .get_mut("test")
            .unwrap()
            .paused_program = Some(bookmark(false));
        assert!(!pipe.retreat_session_at("test", "退下", "test", old_exit));
        {
            let states = pipe.states.lock().unwrap();
            assert!(states["test"].participation.contains(second.request));
            assert!(states["test"].paused_program.is_some());
            assert_eq!(states["test"].stop_epoch, first.stop_epoch);
        }
        assert_eq!(sessions.history("dev:test", 10).unwrap().len(), 1);
    }
}

#[tokio::test]
async fn d1_expired_admission_without_a_sweep_spends_no_credit() {
    let pipe = pipeline();
    pipe.open_participation("test", CreditGrant::Full);
    let expired = {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut("test").unwrap();
        st.participation
            .activate(
                st.participation.identity(),
                FollowWindowPolicy {
                    credits: Some(2),
                    ..Default::default()
                },
                CreditGrant::Full,
                id::now_ms() - 60_000,
            )
            .unwrap()
    };
    assert!(pipe
        .reserve_turn_with_credit("test", expired, true)
        .is_none());
    let states = pipe.states.lock().unwrap();
    assert_eq!(states["test"].participation.followup_credits, 2);
    assert!(!states["test"].participation.request_running());
}

#[tokio::test]
async fn d1_abandoned_partial_stream_settles_without_a_stop_receipt() {
    let pipe = pipeline();
    let identity = pipe.open_participation("test", CreditGrant::Full);
    pipe.states
        .lock()
        .unwrap()
        .get_mut("test")
        .unwrap()
        .participation
        .follow_window
        .enabled = false;
    let ticket = pipe.reserve_turn("test", identity).unwrap();
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut("test").unwrap();
        VoicePipeline::register_stream_output(
            st,
            "partial",
            &None,
            id::now_ms(),
            Some(ticket.request),
        );
        assert!(st.participation.request_running());
    }
    pipe.fail_voice_stream("partial");
    {
        let mut states = pipe.states.lock().unwrap();
        let st = states.get_mut("test").unwrap();
        assert_eq!(st.outputs["partial"].stage, "unknown");
        st.finish_window_request(ticket.request, id::now_ms());
        assert!(!st.participation.request_running());
        assert_eq!(
            st.participation.close_reason,
            Some(CloseReason::SingleRequest)
        );
    }
    pipe.fail_voice_stream("partial"); // duplicate terminal is inert
}
