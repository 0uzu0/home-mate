//! D2 behavior checks use the production owner and ingress, without devices.
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
        Arc::new(|| Err("isolated: no agent".into())),
        Arc::new(|| Err("isolated: no TTS".into())),
        Arc::new(|| Err("isolated: no ASR".into())),
        std::env::temp_dir().join(id::next("hm-d2")),
    );
    with_follow_window(p)
}

/// The PlayFinished entrance for a voice play of one class, exactly as the
/// role logic calls it. Contract v2 #4 (2026-11-16) makes `kind` the gate.
fn end_play(st: &mut DeviceVoiceState, play_id: &str, kind: &str, at: u64) {
    let rec = OutputRecord {
        answer_text: String::new(),
        request: None,
        task_id: None,
        channel: "voice".into(),
        kind: kind.into(),
        content_ref: String::new(),
        stage: "device_confirmed".into(),
        started_ms: 0,
        stage_ms: 0,
        end_sent_ms: None,
        expected_ms: 0,
        synth_ms: 0,
    };
    st.note_voice_play_end(play_id, &rec, at);
}

#[test]
fn d2_only_answers_can_extend_the_tail() {
    let mut st = DeviceVoiceState::default();
    st.participation
        .activate(
            st.participation.identity(),
            FollowWindowPolicy::default(),
            CreditGrant::Full,
            1000,
        )
        .unwrap();
    // Contract v2 #4: an ack's end (kind "ack") is a fact, not a tail.
    end_play(&mut st, "P-ack", "ack", 49_000);
    assert!(
        !st.session_open(49_001),
        "ack/filler/probe must not give a new tail"
    );
    st.participation.close(
        st.participation.identity(),
        CloseReason::ExplicitExit,
        50_000,
    );
    end_play(&mut st, "P-ack2", "ack", 51_000);
    assert!(!st.session_open(51_001));
}

#[tokio::test]
async fn d2_protocol_utterance_does_not_activate_and_repeated_exit_is_idempotent() {
    let p = pipeline();
    for (seq, text) in [(1, "普通问题"), (2, "退下"), (3, "退下")] {
        p.handle(DeviceEvent::Utterance {
            device: "test".into(),
            ts: id::now_ms(),
            seq,
            source: "text".into(),
            text: text.into(),
            provenance: Default::default(),
        })
        .await;
        let states = p.states.lock().unwrap();
        let st = &states["test"];
        assert!(
            !st.session_open(id::now_ms()),
            "Utterance must never open its own window"
        );
        if seq >= 2 {
            assert_eq!(st.stop_epoch, 1, "repeat exit must not revoke again");
        }
    }
}

#[tokio::test]
async fn d2_exit_transcript_precedes_zero_credits_and_live_voice() {
    let p = pipeline();
    p.settings.lock().unwrap().retreat_intent = false;
    p.open_participation("test", CreditGrant::Full);
    p.states
        .lock()
        .unwrap()
        .get_mut("test")
        .unwrap()
        .participation
        .followup_credits = 0;
    p.handle_mic_transcript(MicTranscript {
        leased: false,
        device: "test".into(),
        result: Ok("停止播放".into()),
        duration_ms: 800,
        input_id: 1,
        capture_seq: 1,
        window_epoch: 0,
        control_only: false,
        wake_sliced: false,
        ticket: crate::mic::CaptureProvenance::legacy(0),
        enqueued_at: std::time::Instant::now(),
    })
    .await;
    let states = p.states.lock().unwrap();
    let st = &states["test"];
    assert_eq!(
        st.participation.close_reason,
        Some(CloseReason::ExplicitExit)
    );
    assert!(!st.session_open(id::now_ms()));
}

#[tokio::test]
async fn d2_self_heard_retreat_without_overlap_evidence_is_accepted() {
    let p = pipeline();
    p.settings.lock().unwrap().keyword_interrupt = false;
    p.open_participation("test", CreditGrant::Full);
    p.on_keyword(
        "test",
        "停止播放",
        id::now_ms(),
        Some(1234),
        true,
        Default::default(),
    );
    let states = p.states.lock().unwrap();
    assert_eq!(
        states["test"].participation.close_reason,
        Some(CloseReason::ExplicitExit)
    );
}

fn leased(lease: &str, input: &str) -> crate::gateway::InputProvenance {
    crate::gateway::InputProvenance {
        origin: crate::gateway::InputOrigin::Leased {
            lease: lease.into(),
            origin: Box::new(crate::gateway::InputOrigin::Text {
                input_id: input.into(),
            }),
            overlap: None,
        },
        transport: None,
    }
}

#[tokio::test]
async fn d2_old_events_and_deferred_ack_are_inert_in_every_order_after_reactivation() {
    // Every step asserts the same state: none can enable a later stale step.
    // Rotations and reversals exercise both early/late terminal interleavings.
    for reverse in [false, true] {
        for offset in 0..7 {
            let p = pipeline();
            let old = leased(&p.capture_lease("test"), "captured-before-close");
            let identity = p.open_participation("test", CreditGrant::Full);
            let permit = p.permit_for_identity("test", identity).unwrap();
            assert!(p.retreat_session_at("test", "退下", "test", identity));
            p.open_participation("test", CreditGrant::Full);
            let before = p.states.lock().unwrap()["test"].participation.identity();
            for step in 0..7 {
                let index = (if reverse { 6 - step } else { step } + offset) % 7;
                match index {
                    0 => {
                        p.handle_mic_transcript(MicTranscript {
                            leased: false,
                            device: "test".into(),
                            result: Ok("你好高达，旧问题".into()),
                            duration_ms: 800,
                            input_id: 1,
                            capture_seq: 1,
                            window_epoch: 0,
                            ticket: crate::mic::CaptureProvenance::chunk(old.clone(), 1280),
                            control_only: false,
                            wake_sliced: false,
                            enqueued_at: std::time::Instant::now(),
                        })
                        .await
                    }
                    1 => p.on_keyword(
                        "test",
                        "你好高达",
                        id::now_ms(),
                        Some(999999),
                        false,
                        old.clone(),
                    ),
                    2 => {
                        p.handle(DeviceEvent::Wake {
                            device: "test".into(),
                            ts: id::now_ms(),
                            seq: 99,
                            provenance: old.clone(),
                        })
                        .await
                    }
                    3 => {
                        p.handle(DeviceEvent::Utterance {
                            device: "test".into(),
                            ts: id::now_ms(),
                            seq: 100,
                            source: "text".into(),
                            text: "退下".into(),
                            provenance: old.clone(),
                        })
                        .await
                    }
                    4 => {
                        p.handle(DeviceEvent::PlayFinished {
                            device: "test".into(),
                            play_id: "old-play".into(),
                            played_ms: 49000,
                            consumed_ms: None,
                            progress_basis: None,
                            ts: id::now_ms(),
                        })
                        .await
                    }
                    5 => assert!(p
                        .play_local_ack_at("test", "旧应答", "old_ack", permit)
                        .is_none()),
                    _ => p.on_stopped_locally("test", "old-play", "late", 100, 0, 0, id::now_ms()),
                }
                let states = p.states.lock().unwrap();
                let st = &states["test"];
                assert_eq!(
                    st.participation.identity(),
                    before,
                    "stale event {index} mutated the new owner"
                );
                assert!(st.outputs.is_empty());
                assert!(st.current_task.is_none());
            }
            assert_eq!(
                p.recent(100)
                    .iter()
                    .filter(|e| e["kind"] == "stale_session_event_rejected")
                    .count(),
                7
            );
        }
    }
}

#[test]
fn d2_first_plus_two_and_each_answer_gets_a_complete_tail_without_refilling() {
    let mut p = participation::Participation::default();
    p.activate(
        p.identity(),
        FollowWindowPolicy {
            credits: Some(2),
            ms: 20_000,
            ..Default::default()
        },
        CreditGrant::Full,
        1000,
    )
    .unwrap();
    for (i, (start, end)) in [(1000, 50000), (69000, 80000), (99000, 110000)]
        .into_iter()
        .enumerate()
    {
        assert!(p.session_open(start, false));
        let request = p.begin_request(p.identity(), start, true).unwrap();
        assert_eq!(
            p.followup_credits,
            2 - i as u8,
            "only followups spend credit"
        );
        p.answer_started(request, "answer");
        assert!(p.finish_request(request, start));
        assert!(
            p.session_open(end - 1, false),
            "a long answer cannot time out mid-stream"
        );
        assert!(p.answer_ended(request, "answer", end));
        assert!(p.session_open(end + 19999, false));
        assert!(!p.session_open(end + 20000, false));
        assert_eq!(p.followup_credits, 2 - i as u8);
    }
    assert!(
        p.begin_request(p.identity(), 110001, true).is_none(),
        "fourth ordinary request must be denied"
    );
}

#[tokio::test]
async fn d2_trusted_overlap_rejects_only_a_word_in_the_corresponding_live_answer() {
    use crate::gateway::{InputOrigin, InputProvenance, PlaybackOverlap};
    let p = pipeline();
    let identity = p.open_participation("test", CreditGrant::Full);
    let ticket = p.reserve_turn("test", identity).unwrap().request;
    let mut rec = OutputRecord {
        answer_text: "你可以说停止播放，或者暂停。".into(),
        request: Some(ticket),
        task_id: None,
        channel: "voice".into(),
        kind: "answer".into(),
        content_ref: "你可以说停止播放，或者暂停。".into(),
        stage: "submitted".into(),
        started_ms: 0,
        stage_ms: 0,
        end_sent_ms: None,
        expected_ms: 0,
        synth_ms: 0,
    };
    p.states
        .lock()
        .unwrap()
        .get_mut("test")
        .unwrap()
        .outputs
        .insert("live-answer".into(), rec.clone());
    let origin = InputOrigin::Leased {
        lease: p.capture_lease("test"),
        origin: Box::new(InputOrigin::Audio {
            capture_epoch: "mic-epoch".into(),
            ticket: "capture-ticket".into(),
            input_id: "spoken".into(),
            start_sample: 100,
            end_sample: 200,
        }),
        overlap: Some(PlaybackOverlap {
            play_id: "live-answer".into(),
            capture_epoch: "mic-epoch".into(),
            ticket: "capture-ticket".into(),
            start_sample: 150,
            end_sample: 300,
        }),
    };
    p.on_keyword(
        "test",
        "停止播放",
        id::now_ms(),
        Some(999999),
        true,
        InputProvenance {
            origin: origin.clone(),
            transport: None,
        },
    );
    assert_eq!(
        p.states.lock().unwrap()["test"].participation.close_reason,
        None
    );
    assert!(p
        .recent(100)
        .iter()
        .any(|e| e["reason"] == "self_heard_retreat_overlap"));
    rec.answer_text = "这是另一个回答。".into();
    p.states
        .lock()
        .unwrap()
        .get_mut("test")
        .unwrap()
        .outputs
        .insert("live-answer".into(), rec);
    p.on_keyword(
        "test",
        "停止播放",
        id::now_ms(),
        Some(999999),
        true,
        InputProvenance {
            origin,
            transport: None,
        },
    );
    assert_eq!(
        p.states.lock().unwrap()["test"].participation.close_reason,
        Some(CloseReason::ExplicitExit)
    );
}

#[test]
fn d2_exit_matching_rejects_negation_and_quoted_commands() {
    for text in [
        "不要停止播放",
        "别退下",
        "暂停是什么意思",
        "说停止播放",
        "叫退下的歌",
    ] {
        assert!(!retreat_match(text, &default_retreat_words()), "{text}");
    }
    assert!(retreat_match("不用了，谢谢", &default_retreat_words()));
}

#[tokio::test]
async fn d2_timeout_and_single_request_close_revoke_capture_without_erasing_human_bookmarks() {
    for single in [false, true] {
        let p = pipeline();
        let old = leased(&p.capture_lease("test"), "old");
        let identity = p.open_participation("test", CreditGrant::Full);
        let old_generation = p.states.lock().unwrap()["test"].participation.generation();
        {
            let mut states = p.states.lock().unwrap();
            let st = states.get_mut("test").unwrap();
            st.paused_program = Some(Program {
                text: "人的书签".into(),
                created_ms: id::now_ms(),
                resumable: true,
                ..Default::default()
            });
            if single {
                st.participation.follow_window.enabled = false;
                let request = st
                    .participation
                    .begin_request(identity, id::now_ms(), false)
                    .unwrap();
                st.participation.finish_request(request, id::now_ms());
            }
        }
        if !single {
            p.session_sweep_at(id::now_ms() + 60000);
        }
        p.apply_capture_revocations();
        let states = p.states.lock().unwrap();
        let st = &states["test"];
        assert_eq!(st.participation.generation(), old_generation + 1);
        assert_eq!(st.participation.followup_credits, 0);
        assert!(st.paused_program.is_some());
        drop(states);
        p.handle(DeviceEvent::Wake {
            device: "test".into(),
            seq: 2,
            ts: id::now_ms(),
            provenance: old,
        })
        .await;
        assert!(!p.states.lock().unwrap()["test"].session_open(id::now_ms()));
        assert!(p
            .recent(100)
            .iter()
            .any(|e| e["kind"] == "stale_session_event_rejected"));
    }
}

#[tokio::test]
async fn d2_pending_close_retries_without_a_new_edge() {
    let p = pipeline();
    p.settings.lock().unwrap().follow_window_tone = true;
    p.open_participation("test", CreditGrant::Full);
    {
        let mut states = p.states.lock().unwrap();
        let st = states.get_mut("test").unwrap();
        st.participation
            .activate(
                st.participation.identity(),
                FollowWindowPolicy::default(),
                CreditGrant::Full,
                id::now_ms() - 60000,
            )
            .unwrap();
        st.follow_open_last_tick = true;
        st.current_task = Some("busy-for-boundary-only".into());
    }
    p.follow_window_tone_sweep();
    {
        let mut states = p.states.lock().unwrap();
        let st = states.get_mut("test").unwrap();
        assert!(st.follow_close_pending_at.is_some());
        assert!(!st.follow_open_last_tick);
        st.current_task = None;
    }
    p.follow_window_tone_sweep();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(p.states.lock().unwrap()["test"]
        .follow_close_pending_at
        .is_none());
    assert!(
        p.recent(100)
            .iter()
            .any(|e| e["kind"] == "follow_window_close"),
        "the pending attempt must actually run"
    );
}

#[tokio::test]
async fn d2_close_cancels_all_requests_including_a_task_created_after_revoke() {
    let p = pipeline();
    let identity = p.open_participation("test", CreditGrant::Full);
    let a = p.reserve_turn("test", identity).unwrap();
    let b = {
        let identity = p.states.lock().unwrap()["test"].participation.identity();
        p.reserve_turn("test", identity).unwrap()
    };
    let (a_scope, b_scope, identity) = {
        let states = p.states.lock().unwrap();
        let st = &states["test"];
        (
            st.participation.request_cancel_scope(a.request).unwrap(),
            st.participation.request_cancel_scope(b.request).unwrap(),
            st.participation.identity(),
        )
    };
    assert!(p.retreat_session_at("test", "退下", "test", identity));
    for scope in [a_scope, b_scope] {
        assert!(scope.flag().load(std::sync::atomic::Ordering::SeqCst));
        let late = p.tasks.create_with_scope("dev:test", scope).unwrap();
        assert!(
            late.is_cancelled(),
            "a task not announced before close inherits the same revoked scope"
        );
        assert!(late.commit(|| panic!("old session mutation ran")).is_none());
    }
}

#[tokio::test]
async fn d2_revoked_tool_controls_cannot_operate_new_program_state() {
    let p = pipeline();
    let first = p.open_participation("test", CreditGrant::Full);
    let request = p.reserve_turn("test", first).unwrap().request;
    let scope = p.states.lock().unwrap()["test"]
        .participation
        .request_cancel_scope(request)
        .unwrap();
    let expected = p.states.lock().unwrap()["test"].participation.identity();
    p.retreat_session_at("test", "退下", "test", expected);
    p.open_participation("test", CreditGrant::Full);
    let identity = {
        let mut states = p.states.lock().unwrap();
        let st = states.get_mut("test").unwrap();
        st.live_program = Some(Program {
            text: "新的内容".into(),
            created_ms: id::now_ms(),
            resumable: true,
            ..Default::default()
        });
        st.paused_program = Some(Program {
            text: "新的书签".into(),
            created_ms: id::now_ms(),
            resumable: true,
            ..Default::default()
        });
        st.participation.identity()
    };
    let cancel = scope.flag();
    assert!(!p.pause_program_with_cancel("test", "old tool", Some(&cancel)));
    assert!(!p.resume_program_with_cancel("test", &cancel));
    assert!(!p.stop_program_with_cancel("test", &cancel));
    let states = p.states.lock().unwrap();
    let st = &states["test"];
    assert_eq!(st.participation.identity(), identity);
    assert_eq!(st.live_program.as_ref().unwrap().text, "新的内容");
    assert_eq!(st.paused_program.as_ref().unwrap().text, "新的书签");
}

#[test]
fn d2_failed_or_audio_less_requests_never_renew_a_tail() {
    for with_audio in [false, true] {
        let mut p = participation::Participation::default();
        p.activate(
            p.identity(),
            FollowWindowPolicy {
                ms: 20_000,
                ..Default::default()
            },
            CreditGrant::Full,
            1000,
        )
        .unwrap();
        let request = p.begin_request(p.identity(), 1000, true).unwrap();
        if with_audio {
            p.answer_started(request, "failed-answer");
        }
        p.finish_request(request, 50000);
        if with_audio {
            assert!(p.answer_aborted(request, "failed-answer", 50000));
        }
        assert!(
            !p.session_open(50001, false),
            "failure/no audio grants no new tail"
        );
    }
}

#[test]
fn d2_partially_finished_but_failed_answer_has_no_tail_authority() {
    let mut p = participation::Participation::default();
    p.activate(
        p.identity(),
        FollowWindowPolicy {
            credits: Some(2),
            ms: 20_000,
            ..Default::default()
        },
        CreditGrant::Full,
        1000,
    )
    .unwrap();
    let request = p.begin_request(p.identity(), 1000, true).unwrap();
    p.answer_started(request, "first-piece");
    p.answer_started(request, "last-piece");
    assert!(p.answer_ended(request, "first-piece", 50000));
    assert!(p.answer_aborted(request, "last-piece", 51000));
    assert!(p.finish_request(request, 52000));
    assert!(
        !p.session_open(52001, false),
        "a partially failed answer never completed and cannot renew the window"
    );
}

#[test]
fn d2_admission_near_deadline_cannot_extend_a_failed_or_silent_turn() {
    for failed_audio in [false, true] {
        let mut p = participation::Participation::default();
        p.activate(
            p.identity(),
            FollowWindowPolicy {
                ms: 20_000,
                ..Default::default()
            },
            CreditGrant::Full,
            1000,
        )
        .unwrap();
        assert!(p.session_open(19000, false));
        let request = p.begin_request(p.identity(), 19000, true).unwrap();
        if failed_audio {
            p.answer_started(request, "failed");
        }
        p.finish_request(request, 19001);
        if failed_audio {
            p.answer_aborted(request, "failed", 19001);
        }
        assert!(p.session_open(20999, false));
        assert!(
            !p.session_open(21001, false),
            "admission itself must not move the activation/answer deadline"
        );
    }
}

#[test]
fn legacy_input_survives_a_close() {
    // Field regression 2026-11-15 (dot2-d): after ONE explicit exit, the real
    // gateway's Legacy PCM and Keyword facts were rejected — ~600–750
    // stale_session_event_rejected/min for 3 minutes (1800+) and the device
    // could never wake again. Under the compatibility contract a close
    // revokes the OLD generation's LEASES; unticketed Legacy input is the
    // real gateway's only source and must stay accepted (strict-D2 risk of
    // late legacy re-opening is documented in the codex plan, §Legacy 放行评估).
    let mut p = super::Participation::default();
    let id0 = p.identity();
    p.activate(
        id0,
        super::FollowWindowPolicy::default(),
        super::CreditGrant::Full,
        1_000,
    );
    let id1 = p.identity();
    p.close(id1, super::CloseReason::ExplicitExit, 2_500);
    assert!(
        p.capture_revoked(),
        "close must still revoke leased tickets"
    );
    let legacy = crate::gateway::InputOrigin::Legacy;
    assert!(
        p.accepts_origin(&legacy),
        "Legacy input must survive a close — the real gateway has no tickets yet"
    );
}

/// Wake arbitration reaches the SECOND wake path too (2026-11-15).
///
/// The spotter is not the only way the same phrase opens a session: with
/// `asr_wake_fallback` on, the transcript is matched again on every speaker
/// that heard it. Without this gate a node that stayed silent on the spotter
/// path would wake up from the transcript a second later — a duplicate
/// answer, just slower. Off/observe/solo must all keep the old behaviour.
#[test]
fn the_asr_path_respects_a_lost_arbitration_only_while_enforcing() {
    let p = pipeline();
    // Nobody arbitrated anything yet: the transcript wakes as always.
    assert!(!p.asr_wake_arbitrated_out("dot2-d"), "no verdict = no veto");

    p.set_wake_arbitration("dot2-d", WakeArbitration::Enforce, "test")
        .unwrap();
    // This node lost the same utterance to dot2-e 200 ms ago.
    p.last_arbitration.lock().unwrap().insert(
        "dot2-d".into(),
        (
            false,
            id::now_ms().saturating_sub(200),
            "dot2-e".into(),
            false,
        ),
    );
    assert!(
        p.asr_wake_arbitrated_out("dot2-d"),
        "a loser must not wake from the transcript"
    );
    assert!(
        p.recent(50)
            .iter()
            .any(|e| e["kind"] == "arbitration_lost" && e["via"] == "asr_wake"),
        "the veto is recorded, never silent"
    );

    // The veto is ONE-SHOT: the same verdict cannot silence a second
    // sentence. A time window was the first shape and it deadened the only
    // remaining wake path for seconds after a single loss (2026-11-15).
    assert!(
        !p.asr_wake_arbitrated_out("dot2-d"),
        "only the transcript of the lost utterance is refused"
    );

    // The winner's own transcript is not vetoed.
    p.last_arbitration.lock().unwrap().insert(
        "dot2-d".into(),
        (true, id::now_ms(), "dot2-d".into(), false),
    );
    assert!(!p.asr_wake_arbitrated_out("dot2-d"));

    // A verdict older than the grace window belongs to an earlier utterance.
    p.last_arbitration.lock().unwrap().insert(
        "dot2-d".into(),
        (
            false,
            id::now_ms().saturating_sub(30_000),
            "dot2-e".into(),
            false,
        ),
    );
    assert!(!p.asr_wake_arbitrated_out("dot2-d"));

    // Per-device isolation: dot2-d losing arbitration must NOT veto android-2.
    p.record_last_arbitration("dot2-d", false, id::now_ms(), "dot2-e".into(), false);
    assert!(
        !p.asr_wake_arbitrated_out("android-2"),
        "a loss on dot2-d must not veto android-2"
    );
    assert!(
        p.asr_wake_arbitrated_out("dot2-d"),
        "dot2-d itself is still vetoed once"
    );

    // And with arbitration off the memory is inert.
    p.set_wake_arbitration("dot2-d", WakeArbitration::Off, "test")
        .unwrap();
    p.last_arbitration.lock().unwrap().insert(
        "dot2-d".into(),
        (false, id::now_ms(), "dot2-e".into(), false),
    );
    assert!(!p.asr_wake_arbitrated_out("dot2-d"));
}

#[tokio::test]
async fn rc2_rc3_rc4_serial94_field_audit_regressions() {
    use crate::gateway::InputProvenance;
    let p = pipeline();
    let dev = "dot2-d";
    let now = id::now_ms();

    // RC-4 (#235): when our own live voice output literally contains "退下吧",
    // even if the gateway reports `self_heard = false` at a weak echo level
    // (-38.58 dBFS, as measured in dot2-d seq=462), it must be suppressed as
    // `self_heard_retreat_overlap`.
    let ident = p.open_participation(dev, CreditGrant::Full);
    {
        let mut states = p.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        let req = st.participation.begin_request(ident, now, false).unwrap();
        st.outputs.insert(
            "self-tts-retreat".into(),
            OutputRecord {
                answer_text: "请重复一遍：停止播放、停止播放。".into(),
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
    p.on_keyword_at(
        dev,
        "停止播放",
        now,
        Some(1000),
        false,
        InputProvenance::default(),
        Some(-38.58),
        Some(90),
        WakeAcoustics::default(),
        false,
    );
    assert_eq!(
        p.states.lock().unwrap()[dev].participation.close_reason,
        None,
        "weak (-38.58 dBFS) self-echo of our own spoken retreat text must not close session even with self_heard=false"
    );
    assert!(
        p.recent(20)
            .iter()
            .any(|e| e["kind"] == "keyword_ignored" && e["reason"] == "self_heard_retreat_overlap"),
        "expected self_heard_retreat_overlap ignore event"
    );

    // Conversely, a strong near-end human retreat (-17.63 dBFS, dot2-d seq=204)
    // during active voice playback must succeed and close the session.
    p.on_keyword_at(
        dev,
        "停止播放",
        now + 100,
        Some(1100),
        false,
        InputProvenance::default(),
        Some(-17.63),
        Some(90),
        WakeAcoustics::default(),
        false,
    );
    assert_eq!(
        p.states.lock().unwrap()[dev].participation.close_reason,
        Some(participation::CloseReason::ExplicitExit),
        "near-end human retreat (-17.63 dBFS) must close session"
    );

    // RC-2: waking while media is playing must pause the media session before
    // `stop_device_locked` so the MediaSession is preserved in
    // `MediaState::Paused` instead of destroyed.
    let store = Arc::new(Store::open_in_memory().unwrap());
    let hub = DeviceHub::new();
    let music = crate::tool::music::MusicRuntime::new(store.clone(), hub.clone());
    let p_media = with_follow_window(VoicePipeline::new(
        hub,
        store.clone(),
        Arc::new(TaskRegistry::new(store)),
        Arc::new(|| Err("isolated: no agent".into())),
        Arc::new(|| Err("isolated: no TTS".into())),
        Arc::new(|| Err("isolated: no ASR".into())),
        std::env::temp_dir().join(id::next("hm-d2-media")),
    ));
    p_media.attach_music(&music);
    music.seed_media_session_for_test(
        dev,
        crate::tool::music::MediaSession {
            tracks: vec![crate::tool::music::Track {
                source: crate::tool::music::Source::Library,
                id: "t1".into(),
                title: "晴天".into(),
                artist: "周杰伦".into(),
                binding: "lib".into(),
            }],
            index: 0,
            state: crate::tool::music::MediaState::Playing,
            position_ms: 12_000,
            play_id: None,
            started_ms: now,
            base_ms: 12_000,
            updated_ms: now,
            repeat: crate::tool::music::RepeatMode::Off,
        },
    );
    p_media.on_keyword_at(
        dev,
        "你好高达",
        now + 200,
        Some(1200),
        true,
        InputProvenance::default(),
        Some(-15.9),
        Some(90),
        WakeAcoustics::default(),
        true,
    );
    assert_eq!(
        p_media.media_state(dev),
        Some(crate::tool::music::MediaState::Paused),
        "media session must be cleanly paused on wake"
    );

    // RC-3b: `MicProcessor::promote_wake_capture` must keep `wake_ack_overlap = true`
    // on trailing blocks fed into the open wake segment after promotion.
    let mut mic = crate::mic::MicProcessor::new(crate::mic::VadConfig {
        start_db: -40.0,
        end_db: -45.0,
        start_blocks: 2,
        end_blocks: 4,
        min_blocks: 2,
        max_blocks: 50,
        ..Default::default()
    });
    let mut ctl_prov = crate::mic::CaptureProvenance::legacy(1280);
    ctl_prov.spans[0].control_only = true;
    ctl_prov.spans[0].wake_ack_overlap = false;
    let loud = vec![6000i16; 1280];
    let _ = mic.feed_with_provenance_and_vad(loud.clone(), 16_000, ctl_prov.clone(), None);
    let _ = mic.feed_with_provenance_and_vad(loud.clone(), 16_000, ctl_prov.clone(), None);
    assert!(mic.in_speech());
    mic.promote_wake_capture();
    let _ = mic.feed_with_provenance_and_vad(loud.clone(), 16_000, ctl_prov, None);
    assert!(
        mic.open_provenance()
            .spans
            .iter()
            .filter(|s| s.control_only)
            .all(|s| s.wake_ack_overlap),
        "trailing control_only blocks of a wake-promoted segment must inherit wake_ack_overlap"
    );
    let _ = mic.feed_with_provenance_and_vad(
        loud,
        16_000,
        crate::mic::CaptureProvenance::legacy(1280),
        None,
    );
    assert!(
        mic.open_provenance().allows_wake_ack_followup(),
        "wake-promoted segment followed by post-stop speech must pass allows_wake_ack_followup"
    );
}

#[tokio::test]
async fn d2_rc8_in_flight_background_or_prior_asr_does_not_starve_new_wake_or_speech() {
    let p = pipeline();
    let dev = "dot2-d";
    {
        let mut s = p.settings.lock().unwrap();
        s.proactive_asr_wake = true;
        s.wake_ack.enabled = false;
    }
    p.states
        .lock()
        .unwrap()
        .entry(dev.into())
        .or_default()
        .input_connection_generation = Some(1);

    let prov = crate::gateway::InputProvenance {
        origin: crate::gateway::InputOrigin::Legacy,
        transport: Some(crate::gateway::TransportIdentity {
            generation: 1,
            inbound_seq: 1,
        }),
    };
    let loud = vec![12000i16; 1280];
    let quiet = vec![0i16; 1280];

    // 1) Idle background speech opens and closes -> enters recognizing=true with eligibility="asr_fallback".
    for _ in 0..3 {
        p.feed_chunk(
            dev.into(),
            loud.clone(),
            16_000,
            1,
            id::now_ms(),
            prov.clone(),
        );
    }
    let bg_input_id = {
        let mut states = p.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        let input = st
            .input
            .as_mut()
            .expect("background segment must allocate InputLease");
        assert_eq!(input.eligibility, "asr_fallback");
        // Simulate hung HTTP ASR in flight before the user speaks "你好小智".
        input.recognizing = true;
        st.mic_counters.asr_inflight = 1;
        input.id
    };
    p.mic_arc(dev).unwrap().lock().unwrap().reset();

    // 2) User speaks 3 confirmed blocks of "你好小智" while bg_input_id is still recognizing=true.
    for _ in 0..3 {
        p.feed_chunk(
            dev.into(),
            loud.clone(),
            16_000,
            1,
            id::now_ms(),
            prov.clone(),
        );
    }
    let wake_input_id = {
        let states = p.states.lock().unwrap();
        let st = states.get(dev).unwrap();
        let input = st
            .input
            .as_ref()
            .expect("new confirmed speech must preempt hung asr_fallback lease");
        assert_ne!(
            input.id, bg_input_id,
            "must allocate a fresh InputLease for the new speech"
        );
        assert!(!input.recognizing);
        assert!(input.confirmed);
        input.id
    };

    // 3) KWS "你好小智" fires while the wake segment is still open -> transfers wake_input_id to "session".
    p.on_keyword(dev, "你好高达", id::now_ms(), Some(80), false, prov.clone());
    {
        let states = p.states.lock().unwrap();
        let st = states.get(dev).unwrap();
        let input = st
            .input
            .as_ref()
            .expect("wake capture must remain bound after KWS wake");
        assert_eq!(input.id, wake_input_id);
        assert_eq!(input.eligibility, "session");
    }

    // 4) Even if an in-flight "session" recognition was still occupying st.input when a new
    //    utterance only had 1 pending onset block at KWS wake time, KWS adopts the live onset.
    p.mic_arc(dev).unwrap().lock().unwrap().reset();
    {
        let mut states = p.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        let input = st.input.as_mut().unwrap();
        input.recognizing = true;
        st.mic_counters.asr_inflight = 1;
    }
    // Feed 1 loud block (pending onset, not yet 3-block confirmed) with a newer Instant.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    p.feed_chunk(
        dev.into(),
        loud.clone(),
        16_000,
        1,
        id::now_ms(),
        prov.clone(),
    );
    p.on_keyword(dev, "你好高达", id::now_ms(), Some(90), false, prov.clone());
    {
        let states = p.states.lock().unwrap();
        let st = states.get(dev).unwrap();
        let input = st
            .input
            .as_ref()
            .expect("KWS wake must adopt live pending onset when blocked by older recognition");
        assert!(input.id > wake_input_id);
        assert_eq!(input.eligibility, "session");
        assert!(!input.recognizing);
    }

    // 5) When a prior "session" recognition is in flight while a new in-session speech
    //    segment starts, and the prior recognition finishes mid-speech (`revoke_input("consumed")`),
    //    the next audio block of the ongoing speech segment automatically acquires a session lease.
    p.mic_arc(dev).unwrap().lock().unwrap().reset();
    let prior_session_input_id = {
        let mut states = p.states.lock().unwrap();
        let st = states.get_mut(dev).unwrap();
        let input = st.input.as_mut().unwrap();
        input.recognizing = true;
        st.mic_counters.asr_inflight = 1;
        input.id
    };
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    for _ in 0..3 {
        p.feed_chunk(
            dev.into(),
            loud.clone(),
            16_000,
            1,
            id::now_ms(),
            prov.clone(),
        );
    }
    assert!(p.mic_arc(dev).unwrap().lock().unwrap().in_speech());
    assert_eq!(
        p.states
            .lock()
            .unwrap()
            .get(dev)
            .unwrap()
            .input
            .as_ref()
            .unwrap()
            .id,
        prior_session_input_id,
        "in-flight session recognition must not be aborted by a subsequent onset"
    );
    {
        let mut states = p.states.lock().unwrap();
        states.get_mut(dev).unwrap().revoke_input("consumed");
        assert!(states.get(dev).unwrap().input.is_none());
    }
    p.feed_chunk(dev.into(), loud, 16_000, 1, id::now_ms(), prov.clone());
    {
        let states = p.states.lock().unwrap();
        let st = states.get(dev).unwrap();
        let input = st
            .input
            .as_ref()
            .expect("ongoing in_speech segment must acquire lease on next block after prior slot is consumed");
        assert!(input.id > prior_session_input_id);
        assert!(input.confirmed);
        assert_eq!(input.eligibility, "session");
    }
    let _ = quiet;
}

/// #114 (A3, A5, A9):
/// - A3: +6.0 dB `mic_gain_offset_db` is compensated once across KWS (`wake_claim_for`),
///   passive (`NearEndPresence` -> `check_fleet_bleed`), and ASR (`build_claim(..., None, ...)`).
/// - A5: `Hello` structured AEC fields distinguish `enabled` (with custom tail/samples),
///   `disabled` (`aec_enabled: false, aec_tail_ms: null`), and `unknown` across reconnects.
/// - A9: low-voice near-end utterance right at startup opens a segment without the
///   floor estimator learning or swallowing the first utterance.
#[tokio::test]
async fn d2_audio_truth_single_offset_compensation_hello_aec_states_and_startup_low_voice() {
    let p = pipeline();
    let dev = "dot2-d";
    p.states.lock().unwrap().entry(dev.into()).or_default();

    // Register a peer so `wake_claim_for` exercises multi-node claim construction.
    crate::fleet::FleetService::new(p.store.clone())
        .upsert("dot2-e", "http://127.0.0.1:9", Some("tok"), false)
        .unwrap();
    let cfg = crate::config::ConfigService::new(p.store.clone());
    cfg.save_device_meta(dev, "儿童房", "儿童房", "test")
        .unwrap();
    cfg.save_unit_acoustic_profile(
        dev,
        crate::config::UnitAcousticProfile {
            mic_gain_offset_db: 6.0,
            mic_calibrated: true,
            ..Default::default()
        },
        "test",
    )
    .unwrap();

    let now = id::now_ms();
    // 1. Passive NearEndPresence carries raw post-AEC -35.0 dBFS (noise floor -55.0, SNR 20.0).
    p.handle(crate::gateway::DeviceEvent::NearEndPresence {
        device: dev.into(),
        ts_ms: now,
        duration_ms: 240,
        rms_dbfs: Some(-35.0),
        noise_floor_dbfs: Some(-55.0),
        snr_db: Some(20.0),
        clipped: false,
        echo_health: Some("healthy".into()),
        profile_version: Some("v2".into()),
    })
    .await;

    // `fresh_mic_level` keeps raw -35.0 dBFS so downstream consumers never double-add +6 dB.
    let raw_snap = p.fresh_mic_level(dev).expect("fresh mic level");
    assert!((raw_snap.rms_dbfs.unwrap() - (-35.0)).abs() < 1e-4);

    // KWS claim (`wake_claim_for`) with raw -35.0 dBFS applies +6.0 dB exactly once -> -29.0 dBFS.
    let kws_claim = p
        .wake_claim_for(dev, "你好小智", now, Some(40), Some(-35.0))
        .expect("wake_claim_for");
    assert!((kws_claim.rms_dbfs.unwrap() - (-29.0)).abs() < 1e-4);
    assert!((kws_claim.snr_db.unwrap() - 20.0).abs() < 1e-4);
    assert!(kws_claim.calibrated);

    // ASR claim (`build_claim` with `rms_dbfs = None`) reads raw `fresh_mic_level` (-35.0)
    // and applies +6.0 dB exactly once -> -29.0 dBFS (never -23.0 dBFS).
    let asr_claim = p.build_claim(dev, now as i64, None, "你好小智");
    assert!((asr_claim.rms_dbfs.unwrap() - (-29.0)).abs() < 1e-4);
    assert!((asr_claim.snr_db.unwrap() - 20.0).abs() < 1e-4);

    // Passive bleed survival (`check_fleet_bleed`): raw -35.0 dBFS + 6.0 dB = -29.0 dBFS >= -32.0 dBFS,
    // so near-end speech survives a peer's active FleetPlaybackNotice!
    p.sovereignty
        .record_playback_notice(crate::sovereignty::FleetPlaybackNotice {
            device: "dot2-e".into(),
            room: "客厅".into(),
            channel: "voice".into(),
            active: true,
            text_preview: "你好小智".into(),
            at_ms: now,
            ttl_ms: 15_000,
        });
    assert!(
        !p.check_fleet_bleed(dev, "你好小智", true, Some(240)),
        "+6 dB single compensation (-35 + 6 = -29 >= -32 dBFS) must preserve near-end wake against peer bleed"
    );

    // 2. A5: Hello structured AEC states (`enabled` with non-default 640ms/10240 samples,
    //    `disabled` with `aec_enabled: false`, and `unknown` legacy None) across reconnects.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let hub = p.hub.clone();
    tokio::spawn(async move {
        let _ = hub.listen(addr).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    for (aec_json, expected_state, expected_enabled, expected_tail, expected_samples) in [
        (
            serde_json::json!({"aec_enabled": true, "aec_tail_ms": 640, "effective_tail_samples": 10240, "aec_sample_rate": 16000, "aec_provenance": "speex_mdf_v2"}),
            "enabled",
            serde_json::json!(true),
            serde_json::json!(640),
            serde_json::json!(10240),
        ),
        (
            serde_json::json!({"aec_enabled": false, "aec_tail_ms": null}),
            "disabled",
            serde_json::json!(false),
            serde_json::Value::Null,
            serde_json::Value::Null,
        ),
        (
            serde_json::json!({}),
            "unknown",
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        ),
    ] {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let mut lines = tokio::io::BufReader::new(rd).lines();
        let mut hello = serde_json::json!({
            "type": "hello",
            "protocol": 2,
            "min_protocol": 0,
            "supported_versions": [0, 2],
            "gateway": "gw-a5",
            "gateway_version": "0.0.53",
            "devices": [{
                "stable_id": dev,
                "model": "AEOBC",
                "capabilities": ["audio_out", "audio_in"],
                "audio_profile": "dot2"
            }]
        });
        for (k, v) in aec_json.as_object().unwrap() {
            hello[k] = v.clone();
        }
        let mut line = serde_json::to_string(&hello).unwrap();
        line.push('\n');
        wr.write_all(line.as_bytes()).await.unwrap();
        let _ack = lines.next_line().await.unwrap().unwrap();
        let snap = p.hub.snapshot();
        let d = snap["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["stable_id"] == dev)
            .unwrap();
        assert_eq!(d["aec_state"], expected_state, "{d}");
        assert_eq!(d["aec_enabled"], expected_enabled, "{d}");
        assert_eq!(d["aec_tail_ms"], expected_tail, "{d}");
        assert_eq!(d["effective_tail_samples"], expected_samples, "{d}");
    }

    // 3. A9: low-voice near-end utterance right at startup (-35 dBFS over -62 dBFS quiet blocks)
    //    opens a segment cleanly without the noise floor learning the user's voice.
    let mut mic = crate::mic::MicProcessor::new(crate::mic::VadConfig::default());
    let quiet_room = vec![25i16; 1280]; // ~ -62 dBFS
    let soft_voice = vec![600i16; 1280]; // ~ -34.7 dBFS
    for _ in 0..2 {
        assert!(matches!(
            mic.feed_with_provenance(
                quiet_room.clone(),
                16_000,
                crate::mic::CaptureProvenance::legacy(1280)
            ),
            crate::mic::FeedOutcome::None
        ));
    }
    let mut started = false;
    for _ in 0..3 {
        if matches!(
            mic.feed_with_provenance(
                soft_voice.clone(),
                16_000,
                crate::mic::CaptureProvenance::legacy(1280)
            ),
            crate::mic::FeedOutcome::SpeechStarted
        ) {
            started = true;
        }
    }
    assert!(
        started && mic.in_speech(),
        "A9: startup low-voice near-end (-34.7 dBFS) must open a speech segment without being swallowed by noise floor"
    );
    assert!(
        mic.noise_floor_db().unwrap_or(-60.0) < -50.0,
        "A9: noise floor must not learn the user's startup voice: {:?}",
        mic.noise_floor_db()
    );
}

/// #117 (I-1, I-3, transport-bound InputOrigin) & #216 (160ms pre-onset lead-in for short questions):
/// - Verifies `InputOrigin::Legacy` binds to hub-stamped `TransportIdentity { generation, inbound_seq }`
///   in `InputLease::matches_capture` and surfaces `transport_generation` + `inbound_seq_first..last`
///   in `input_snapshot()`.
/// - Verifies `MicProcessor` retains 2 clean pre-onset blocks (`160 ms` = `2560` samples) in
///   `MicSegment.lead_pcm` without altering `seg.provenance.samples() == seg.pcm.len()`, and
///   `handle_mic_outcome` prepends that `160 ms` lead-in to the WAV sent to `AsrClient::transcribe`.
#[tokio::test]
async fn d2_input_origin_transport_binding_and_short_query_160ms_pre_onset_lead_in() {
    struct CapturingAsr {
        captured_wav: Arc<std::sync::Mutex<Vec<u8>>>,
    }
    #[async_trait::async_trait]
    impl crate::speech::AsrClient for CapturingAsr {
        async fn transcribe(&self, wav: &[u8]) -> Result<String, crate::speech::SpeechError> {
            *self.captured_wav.lock().unwrap() = wav.to_vec();
            Ok("几点了？".into())
        }
        fn provider(&self) -> &str {
            "capturing"
        }
    }

    let captured_wav = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_ref = captured_wav.clone();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let p = VoicePipeline::new(
        DeviceHub::new(),
        store.clone(),
        Arc::new(TaskRegistry::new(store)),
        Arc::new(|| Err("isolated: no agent".into())),
        Arc::new(|| Err("isolated: no TTS".into())),
        Arc::new(move || {
            Ok(Arc::new(CapturingAsr {
                captured_wav: captured_ref.clone(),
            }))
        }),
        std::env::temp_dir().join(id::next("hm-216")),
    );
    let dev = "dot2-d";
    p.states.lock().unwrap().entry(dev.into()).or_default();
    let now = id::now_ms();
    let _ident = p.open_participation(dev, CreditGrant::Full);

    let mk_prov = |gen: u64, seq: u64| crate::gateway::InputProvenance {
        origin: crate::gateway::InputOrigin::Legacy,
        transport: Some(crate::gateway::TransportIdentity {
            generation: gen,
            inbound_seq: seq,
        }),
    };

    // Feed 2 quiet pre-onset blocks (with distinctive sample marker 111) + 3 loud onset blocks (marker 9000).
    let quiet_lead = vec![111i16; 1280];
    let loud_body = vec![9_000i16; 1280];
    let quiet_tail = vec![0i16; 1280];

    for seq in 1..=2 {
        p.feed_chunk(
            dev.into(),
            quiet_lead.clone(),
            16_000,
            1,
            now + seq * 80,
            mk_prov(7, seq),
        );
    }
    for seq in 3..=5 {
        p.feed_chunk(
            dev.into(),
            loud_body.clone(),
            16_000,
            1,
            now + seq * 80,
            mk_prov(7, seq),
        );
    }

    // Verify #117: InputLease matches transport generation 7, rejects stale transport generation 6,
    // and exposes `transport_generation: 7` and `inbound_seq_first: 3, inbound_seq_last: 5` in snapshot.
    {
        let states = p.states.lock().unwrap();
        let st = states.get(dev).unwrap();
        let lease = st.input.as_ref().expect("active input lease");
        assert!(lease.matches_capture(&mk_prov(7, 6)));
        assert!(
            !lease.matches_capture(&mk_prov(6, 6)),
            "Legacy InputOrigin from an older transport generation must not match current InputLease"
        );
        let snap = st.input_snapshot();
        assert_eq!(snap["transport_generation"], 7);
        assert_eq!(snap["inbound_seq_first"], 3);
        assert_eq!(snap["inbound_seq_last"], 3);
    }

    // Close the segment with 8 quiet hangover blocks (4 trimmed, 4 kept -> 3 + 4 = 7 blocks in seg.pcm).
    for seq in 6..=13 {
        p.feed_chunk(
            dev.into(),
            quiet_tail.clone(),
            16_000,
            1,
            now + seq * 80,
            mk_prov(7, seq),
        );
    }

    for _ in 0..40 {
        if !captured_wav.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let wav = captured_wav.lock().unwrap().clone();
    let (sr, ch, pcm_bytes) = crate::speech::wav_pcm(&wav).expect("valid WAV sent to ASR");
    assert_eq!(sr, 16_000);
    assert_eq!(ch, 1);
    // 2 pre-onset lead blocks (2560 samples) + 7 segment blocks (8960 samples) = 11520 samples (23040 bytes).
    assert_eq!(
        pcm_bytes.len(),
        (2 + 7) * 1280 * 2,
        "#216: WAV sent to ASR must include 2 blocks (160ms) of pre-onset lead-in plus 7 segment blocks"
    );
    let first_sample = i16::from_le_bytes([pcm_bytes[0], pcm_bytes[1]]);
    let body_sample = i16::from_le_bytes([pcm_bytes[2560 * 2], pcm_bytes[2560 * 2 + 1]]);
    assert_eq!(
        first_sample, 111,
        "#216: pre-onset lead-in must preserve actual pre-trigger samples"
    );
    assert_eq!(body_sample, 9_000);
}
