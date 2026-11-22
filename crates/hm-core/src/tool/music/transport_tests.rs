//! Domain send-failure/no-live-play boundary complements real play/stop integration.
use super::*;
use std::sync::atomic::AtomicBool;

#[tokio::test]
async fn northbound_existing_play_send_failure_is_unknown_not_no_op() {
    for action in ["pause", "stop"] {
        let runtime = MusicRuntime::new(
            Arc::new(crate::store::Store::open_in_memory().unwrap()),
            crate::gateway::DeviceHub::new(),
        );
        let now = crate::id::now_ms();
        runtime.media.lock().unwrap().insert(
            "fixture".into(),
            MediaSession {
                tracks: vec![],
                index: 0,
                state: MediaState::Playing,
                position_ms: 0,
                play_id: Some("existing-live-play".into()),
                started_ms: now,
                base_ms: 0,
                updated_ms: now,
                repeat: RepeatMode::Off,
            },
        );
        let out = runtime
            .control_with_cancel(
                "fixture",
                action,
                Some("media"),
                Arc::new(AtomicBool::new(false)),
                None,
            )
            .await
            .unwrap();
        assert!(!out.confirmed);
        assert!(!out.no_op);
        assert_eq!(out.evidence, crate::tool::Evidence::Unknown);
    }
}

#[tokio::test]
async fn northbound_no_live_play_stop_is_local_no_op_without_device_evidence() {
    let runtime = MusicRuntime::new(
        Arc::new(crate::store::Store::open_in_memory().unwrap()),
        crate::gateway::DeviceHub::new(),
    );
    let now = crate::id::now_ms();
    runtime.media.lock().unwrap().insert(
        "fixture".into(),
        MediaSession {
            tracks: vec![],
            index: 0,
            state: MediaState::Paused,
            position_ms: 0,
            play_id: None,
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::Off,
        },
    );
    let out = runtime
        .control_with_cancel(
            "fixture",
            "stop",
            Some("media"),
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .await
        .unwrap();
    assert!(!out.confirmed);
    assert!(out.no_op);
    assert_eq!(out.evidence, crate::tool::Evidence::Observed);
}
