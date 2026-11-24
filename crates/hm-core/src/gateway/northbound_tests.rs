use super::*;
use crate::{
    api::{router, AppState},
    store::Store,
    tool::mocks::MockInject,
};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

const FIXTURE: &str = include_str!("../../../../docs/fixtures/northbound-status-v1.json");
fn frame() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}
struct Peer {
    writer: tokio::net::tcp::OwnedWriteHalf,
    _reader: tokio::net::tcp::OwnedReadHalf,
}
impl Peer {
    async fn send(&mut self, value: &Value) {
        self.writer
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
}
async fn connect(state: &Arc<AppState>, listener: Arc<TcpListener>) -> Peer {
    let address = listener.local_addr().unwrap();
    let hub = state.hub.clone();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _ = hub.serve_connection(socket).await;
    });
    let socket = TcpStream::connect(address).await.unwrap();
    let (reader, mut writer) = socket.into_split();
    writer.write_all(b"{\"type\":\"hello\",\"protocol\":0,\"min_protocol\":0,\"gateway\":\"fixture\",\"devices\":[{\"stable_id\":\"fixture-dot\",\"model\":\"fixture\",\"capabilities\":[\"audio_out\",\"stop\",\"play_stream\"],\"audio_profile\":\"fixture\"}]}\n").await.unwrap();
    let mut buffered = BufReader::new(reader);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), buffered.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let ack: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(ack["type"], "hello_ack");
    assert_eq!(ack["protocol"], 0);
    Peer {
        writer,
        _reader: buffered.into_inner(),
    }
}
async fn read(base: &str) -> Value {
    reqwest::get(format!("{base}/api/v0/diagnostics"))
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
}
async fn until(base: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let v = read(base).await;
            if predicate(&v) {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("diagnostic state must reach the real router")
}
fn report(v: &Value) -> &Value {
    &v["gateway"]["devices"][0]["northbound"]
}
async fn processed(state: &AppState, before: u64) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.hub.lock_state().frame_seq <= before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn northbound_actual_router_wire0_identity_freshness_absence_and_schema_truth() {
    let state = AppState::with_overrides(
        Arc::new(Store::open_in_memory().unwrap()),
        std::env::temp_dir().join("hm-nb-api"),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", api.local_addr().unwrap());
    let app = router(state.clone());
    let server = tokio::spawn(async move { axum::serve(api, app).await.unwrap() });
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let mut old = connect(&state, listener.clone()).await;
    assert_eq!(report(&read(&base).await)["state"], "absent");
    old.send(&frame()).await;
    let seen = until(&base, |v| report(v)["state"] == "available").await;
    assert_eq!(report(&seen)["sample"], frame()["northbound"]);
    assert_eq!(report(&seen)["stale"], false);
    assert!(report(&seen)["received_ms"].as_u64().is_some());
    let mut wrong = frame();
    wrong["descriptor"] = json!({"stable_id":"another-device","model":"fixture","capabilities":[],"audio_profile":"fixture"});
    wrong["northbound"]["critical_full"] = json!(999);
    let before = state.hub.lock_state().frame_seq;
    old.send(&wrong).await;
    processed(&state, before).await;
    assert_eq!(
        report(&read(&base).await)["sample"],
        frame()["northbound"],
        "mismatched descriptor cannot change any stored counter"
    );
    let mut unknown = frame();
    unknown["device"] = json!("unknown-device");
    let before = state.hub.lock_state().frame_seq;
    old.send(&unknown).await;
    processed(&state, before).await;
    assert_eq!(
        read(&base).await["gateway"]["devices"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    unknown["descriptor"] = json!({"stable_id":"unknown-device","model":"fixture","capabilities":[],"audio_profile":"fixture"});
    for _ in 0..2 {
        let before = state.hub.lock_state().frame_seq;
        old.send(&unknown).await;
        processed(&state, before).await;
        let snapshot = read(&base).await;
        let introduced = snapshot["gateway"]["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["stable_id"] == "unknown-device")
            .unwrap();
        assert_eq!(introduced["northbound"]["state"], "available");
        assert_eq!(introduced["northbound"]["sample"], frame()["northbound"]);
    }

    let mut fresh = connect(&state, listener).await;
    let stale = read(&base).await;
    assert_eq!(report(&stale)["stale"], true);
    assert_eq!(report(&stale)["sample"], frame()["northbound"]);
    // The superseded socket is actively closed. A callback already queued by
    // that reader must still be rejected by the production generation gate.
    let mut late = frame();
    late["northbound"]["critical_full"] = json!(999);
    state.hub.handle_frame(
        seen["gateway"]["generation"].as_u64().unwrap(),
        serde_json::from_value(late).unwrap(),
    );
    assert_eq!(
        report(&read(&base).await)["sample"],
        frame()["northbound"],
        "superseded callback cannot replace report"
    );
    fresh.send(&frame()).await;
    let live = until(&base, |v| report(v)["stale"] == false).await;
    assert_eq!(
        report(&live)["connection_generation"],
        live["gateway"]["generation"]
    );
    let mut unsupported = frame();
    unsupported["northbound"] = json!({"schema":2,"future_private_payload":"must-not-retain"});
    fresh.send(&unsupported).await;
    let unavailable = until(&base, |v| report(v)["state"] == "unsupported").await;
    assert!(report(&unavailable)["sample"].is_null());
    assert!(!unavailable.to_string().contains("must-not-retain"));
    for field in ["critical_high_water_bytes", "last_fault"] {
        let mut invalid = frame();
        invalid["northbound"].as_object_mut().unwrap().remove(field);
        let before = state.hub.lock_state().frame_seq;
        fresh.send(&invalid).await;
        processed(&state, before).await;
        until(&base, |v| report(v)["state"] == "invalid").await;
    }
    let mut absent = frame();
    absent.as_object_mut().unwrap().remove("northbound");
    fresh.send(&absent).await;
    let legacy = until(&base, |v| report(v)["state"] == "absent").await;
    assert!(legacy["gateway"]["connected"].as_bool().unwrap());
    fresh.send(&frame()).await;
    until(&base, |v| report(v)["state"] == "available").await;
    drop(fresh);
    let offline = until(&base, |v| v["gateway"]["connected"] == false).await;
    assert_eq!(report(&offline)["stale"], true);
    assert_eq!(report(&offline)["sample"], frame()["northbound"]);
    drop(old);
    server.abort();
}

#[test]
fn northbound_fixture_old_decoder_and_new_decoder_preserve_wire0_compatibility() {
    #[derive(serde::Deserialize)]
    struct OldStatus {
        device: String,
        online: bool,
        reason: String,
    }
    let old: OldStatus = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(old.device, "fixture-dot");
    assert!(old.online);
    assert_eq!(old.reason, "registered");
    let new: GatewayFrame = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(serde_json::to_value(new).unwrap(), frame());
    let mut legacy = frame();
    legacy.as_object_mut().unwrap().remove("northbound");
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<GatewayFrame>(legacy.clone()).unwrap())
            .unwrap(),
        legacy
    );
    for (key, value) in [
        ("audio_dropped", json!(-1)),
        ("writer_errors", json!("0")),
        ("boot_id", json!("")),
        ("extra", json!(1)),
    ] {
        let mut invalid = frame()["northbound"].clone();
        invalid[key] = value;
        assert_eq!(
            northbound::NorthboundRecord::snapshot(
                Some(&northbound::NorthboundRecord::new(Some(invalid), 1)),
                1,
                true
            )["state"],
            "invalid"
        );
    }
}

#[tokio::test]
async fn northbound_core_reader_eof_retires_actual_blocked_writer_task() {
    let state = AppState::with_overrides(
        Arc::new(Store::open_in_memory().unwrap()),
        std::env::temp_dir().join("hm-nb-writer"),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let mut peer = connect(&state, listener).await;
    // A nonreading real socket backs up the actual Core writer. The peer
    // half-closes its uplink but keeps its unread receive half alive.
    let ticket = state
        .hub
        .play_begin("fixture-dot", "media", 16000, None)
        .unwrap();
    let pcm = vec![0; 24576];
    for seq in 0..512 {
        state.hub.play_chunk(&ticket.play_id, seq, &pcm).unwrap();
    }
    peer.writer.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.hub.connected()
            || state.hub.snapshot()["transport_tasks"]["connection_tasks"] != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let observed = state.hub.snapshot();
    assert_eq!(observed["transport_tasks"]["connection_tasks"], 0);
    assert_eq!(observed["transport_tasks"]["reader_futures"], 0);
    assert_eq!(
        observed["transport_tasks"]["writer_tasks"], 0,
        "reader EOF cannot detach a blocked writer"
    );
    drop(peer);
}

#[tokio::test]
async fn northbound_supersession_retires_blocked_transport_without_clearing_fresh_waiters() {
    let state = AppState::with_overrides(
        Arc::new(Store::open_in_memory().unwrap()),
        std::env::temp_dir().join("hm-nb-supersede"),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let mut peer = connect(&state, listener.clone()).await;
    let mut retained_peers = Vec::new();
    for _ in 0..3 {
        let old_generation = state.hub.lock_state().generation;
        let old_play = state
            .hub
            .play_begin("fixture-dot", "media", 16000, None)
            .unwrap();
        let pcm = vec![0; 24576];
        for seq in 0..512 {
            state.hub.play_chunk(&old_play.play_id, seq, &pcm).unwrap();
        }
        let StopSubmission::Accepted {
            receiver: old_stop, ..
        } = state.hub.stop_and_wait(&old_play.play_id)
        else {
            panic!("live stop must be accepted")
        };
        // Keep BOTH old socket halves alive and unread: channel replacement
        // alone cannot interrupt the old writer blocked inside write_all.
        let fresh = connect(&state, listener.clone()).await;
        retained_peers.push(peer);
        peer = fresh;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = state.hub.snapshot();
                if snapshot["transport_tasks"]["connection_tasks"] == 1
                    && snapshot["transport_tasks"]["reader_futures"] == 1
                    && snapshot["transport_tasks"]["writer_tasks"] == 1
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("supersession retires actual old reader/writer, not only business state");
        assert!(
            matches!(old_stop.await, Ok(StopOutcome::Unknown { .. })),
            "old stop is unknown, never confirmed"
        );
        let fresh_play = state
            .hub
            .play_begin("fixture-dot", "media", 16000, None)
            .unwrap();
        let StopSubmission::Accepted {
            receiver: mut fresh_stop,
            ..
        } = state.hub.stop_and_wait(&fresh_play.play_id)
        else {
            panic!("live stop must be accepted")
        };
        let volume = state.hub.volume_set("fixture-dot", 0.2).unwrap();
        let (wifi_tx, mut wifi_rx) = oneshot::channel();
        state
            .hub
            .lock_state()
            .pending_wifi
            .insert("fixture-dot".into(), wifi_tx);
        // Deterministically replay the production retiring-generation cleanup
        // AFTER fresh registrations, proving the delayed-teardown ordering.
        state.hub.retire_connection(old_generation);
        assert!(matches!(
            fresh_stop.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            wifi_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(state.hub.lock_state().volume_pending.contains_key(&volume));
        assert!(!state.hub.lock_state().stop_sent.contains(&old_play.play_id));
        peer.send(
            &json!({"type":"stop_receipt","task_id":fresh_play.play_id,"stage":"device_confirmed","played_ms":0}),
        )
        .await;
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), fresh_stop)
                .await
                .unwrap()
                .unwrap(),
            StopOutcome::Confirmed { played_ms: 0, .. }
        ));
        assert_eq!(state.hub.snapshot()["pending_stop_sent"], 0);
    }
    drop(peer);
    drop(retained_peers);
}

#[tokio::test]
async fn typed_stop_actual_hub_send_failure_and_dropped_waiter_are_unknown() {
    let state = AppState::with_overrides(
        Arc::new(Store::open_in_memory().unwrap()),
        std::path::PathBuf::from("./data-test"),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let _peer = connect(&state, listener).await;
    let play = state
        .hub
        .play_begin("fixture-dot", "voice", 16000, None)
        .unwrap();
    let submission = state.hub.stop_and_wait(&play.play_id);
    match &submission {
        StopSubmission::Accepted {
            stop_id,
            play_id,
            connection_generation,
            ..
        } => {
            assert!(stop_id.starts_with("STOP"));
            assert_eq!(play_id, &play.play_id);
            assert_eq!(*connection_generation, state.hub.lock_state().generation);
        }
        _ => panic!("a live play stop must be accepted"),
    }
    // Fault injection after a real production connection/play/stop: lose the
    // sender before any hardware receipt. Completion must remain Unknown.
    state.hub.lock_state().stop_waiters.remove(&play.play_id);
    assert_eq!(
        submission.settle(Duration::from_secs(1), None).await,
        StopOutcome::unknown("receiver_dropped")
    );
    let play = state
        .hub
        .play_begin("fixture-dot", "voice", 16000, None)
        .unwrap();
    state.hub.detach_test_outbounds();
    let submission = state.hub.stop_and_wait(&play.play_id);
    assert!(matches!(&submission, StopSubmission::SendFailed { .. }));
    assert!(
        matches!(submission.settle(Duration::from_secs(1),None).await,StopOutcome::Unknown { reason } if reason.starts_with("send_failed:"))
    );
    assert_eq!(state.hub.snapshot()["pending_stop_waiters"], 0);
    assert!(matches!(
        state.hub.stop_and_wait("unknown-play"),
        StopSubmission::NoActivePlay
    ));
}
