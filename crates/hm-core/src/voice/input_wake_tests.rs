//! Real onset/KWS transitions, plus rejection at each capture authority fence.
use super::tests::pipeline;
use super::*;
use crate::gateway::{InputOrigin, InputProvenance, TransportIdentity};

fn source(p: &VoicePipeline, ticketed: bool) -> InputProvenance {
    InputProvenance {
        origin: if ticketed {
            InputOrigin::Leased {
                lease: p.capture_lease("test"),
                origin: Box::new(InputOrigin::Audio {
                    capture_epoch: "capture-one".into(),
                    ticket: "segment-one".into(),
                    input_id: "utterance-one".into(),
                    start_sample: 0,
                    end_sample: 1280,
                }),
                overlap: None,
            }
        } else {
            InputOrigin::Legacy
        },
        transport: Some(TransportIdentity {
            generation: 1,
            inbound_seq: 1,
        }),
    }
}

fn feed(p: &VoicePipeline, source: &InputProvenance) {
    p.feed_chunk(
        "test".into(),
        vec![12000; 1280],
        16000,
        1,
        id::now_ms(),
        source.clone(),
    );
}

fn prepared(ticketed: bool, blocks: usize) -> (Arc<VoicePipeline>, InputProvenance) {
    let p = pipeline();
    p.settings.lock().unwrap().proactive_asr_wake = false;
    p.settings.lock().unwrap().wake_ack.enabled = false;
    p.states
        .lock()
        .unwrap()
        .entry("test".into())
        .or_default()
        .input_connection_generation = Some(1);
    let source = source(&p, ticketed);
    for _ in 0..blocks {
        feed(&p, &source);
    }
    (p, source)
}

#[tokio::test]
async fn keyword_transfers_exact_pending_or_confirmed_physical_input_without_extending_it() {
    for ticketed in [false, true] {
        for blocks in [1, 3] {
            let (p, source) = prepared(ticketed, blocks);
            let original = {
                let states = p.states.lock().unwrap();
                let input = states["test"].input.as_ref().unwrap();
                assert_eq!(input.confirmed, blocks == 3);
                assert_eq!(input.eligibility, "pending");
                (
                    input.id,
                    input.start,
                    input.start_ms,
                    input.capture_deadline,
                    input.origin.clone(),
                )
            };
            p.on_keyword(
                "test",
                "你好高达",
                id::now_ms(),
                Some(80),
                false,
                source.clone(),
            );
            {
                let states = p.states.lock().unwrap();
                let st = &states["test"];
                let input = st
                    .input
                    .as_ref()
                    .expect("same physical input must survive accepted KWS");
                assert_eq!(
                    (
                        input.id,
                        input.start,
                        input.start_ms,
                        input.capture_deadline,
                        input.origin.clone()
                    ),
                    original
                );
                assert_eq!(input.confirmed, blocks == 3, "KWS never confirms VAD");
                assert_eq!(input.eligibility, "session");
                assert!(input.valid(st));
                assert!(input.context.is_some());
                assert!(!input.recognizing);
                assert_eq!(st.mic_counters.asr_inflight, 0, "KWS must not dispatch ASR");
            }
            for _ in blocks..3 {
                feed(&p, &source);
            }
            let states = p.states.lock().unwrap();
            let st = &states["test"];
            let input = st.input.as_ref().unwrap();
            assert_eq!(input.id, original.0);
            assert!(input.confirmed && input.valid(st));
        }
    }
}

#[tokio::test]
async fn keyword_does_not_rebase_stale_or_dispatched_capture() {
    for cause in [
        "recognizing",
        "consumed",
        "expired_capture",
        "expired_lifetime",
        "connection",
        "source_connection",
        "lease",
        "epoch",
        "ticket",
        "context",
        "reset",
        "onset",
    ] {
        let (p, mut keyword) = prepared(true, 1);
        match cause {
            "reset" => p.mic.lock().unwrap().get_mut("test").unwrap().reset(),
            "onset" => {
                let mut mic = p.mic.lock().unwrap();
                let processor = mic.get_mut("test").unwrap();
                processor.reset();
                processor.feed_with_provenance(
                    vec![12000; 1280],
                    16000,
                    crate::mic::CaptureProvenance::chunk(keyword.clone(), 1280),
                );
            }
            "context" => {
                let store = crate::session::SessionStore::new(p.store.clone());
                let old = store
                    .admit_device_context("test", id::now_ms(), 30_000, true)
                    .unwrap()
                    .ticket();
                p.states
                    .lock()
                    .unwrap()
                    .get_mut("test")
                    .unwrap()
                    .input
                    .as_mut()
                    .unwrap()
                    .context = Some(old);
                store
                    .admit_device_context("test", id::now_ms() + 30_000, 30_000, true)
                    .unwrap();
            }
            _ => {}
        }
        let mut states = p.states.lock().unwrap();
        let st = states.get_mut("test").unwrap();
        match cause {
            "recognizing" => st.input.as_mut().unwrap().recognizing = true,
            "consumed" => st.input.as_mut().unwrap().consumed = true,
            "expired_capture" => {
                st.input.as_mut().unwrap().capture_deadline =
                    Instant::now() - Duration::from_millis(1)
            }
            "expired_lifetime" => {
                st.input.as_mut().unwrap().start = Instant::now() - INPUT_LIFETIME
            }
            "connection" => st.input_connection_generation = Some(2),
            "source_connection" => {
                st.input.as_mut().unwrap().origin.spans[0]
                    .provenance
                    .transport
                    .as_mut()
                    .unwrap()
                    .generation = 2
            }
            "lease" => {
                st.participation.revoke_capture();
            }
            "epoch" => st.current_capture_epoch = Some("other-epoch".into()),
            "ticket" => {
                if let InputOrigin::Leased { origin, .. } = &mut keyword.origin {
                    if let InputOrigin::Audio { ticket, .. } = &mut **origin {
                        *ticket = "other-ticket".into();
                    }
                }
            }
            _ => {}
        }
        let original = st.input.as_ref().unwrap().id;
        assert!(
            p.take_wake_capture_locked("test", st, &keyword).is_none(),
            "{cause}"
        );
        assert_eq!(
            st.input.as_ref().unwrap().id,
            original,
            "rejected candidate follows ordinary cancellation: {cause}"
        );
        p.stop_device_locked("test", st, false);
        assert!(
            st.input.is_none(),
            "ordinary stop must revoke rejected slot: {cause}"
        );
        assert_eq!(st.mic_counters.asr_inflight, 0);
    }
}

#[tokio::test]
async fn restoration_rechecks_live_onset_and_connection_after_context_admission() {
    for cause in ["reset", "connection", "deadline"] {
        let (p, keyword) = prepared(true, 1);
        let mut states = p.states.lock().unwrap();
        let st = states.get_mut("test").unwrap();
        let mut capture = p.take_wake_capture_locked("test", st, &keyword).unwrap();
        p.stop_device_locked("test", st, false);
        let (policy, window) = p.context_policy();
        st.participation
            .activate(
                st.participation.identity(),
                policy,
                CreditGrant::Full,
                id::now_ms(),
            )
            .unwrap();
        let context = p
            .admit_context_locked("test", st, id::now_ms(), window, true)
            .unwrap();
        match cause {
            "reset" => p.mic.lock().unwrap().get_mut("test").unwrap().reset(),
            "connection" => st.input_connection_generation = Some(2),
            "deadline" => capture.capture_deadline = Instant::now() - Duration::from_millis(1),
            _ => unreachable!(),
        }
        p.restore_wake_capture_locked("test", st, Some(capture), &keyword, context);
        assert!(
            st.input.is_none(),
            "candidate cannot be revived after {cause}"
        );
    }
}
