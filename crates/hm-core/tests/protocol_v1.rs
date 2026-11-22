//! Protocol v1 mechanism (#8, gateway #11/#13, 2026-11-16): handshake
//! interval negotiation + reject frames with reasons, the remote mute latch
//! (one frame, correlated receipt, honest unknown), and link-error facts.
//! The fake gateway here is deliberately raw (hand-written JSON lines) so
//! the tests also prove an OLD-style hello — no new fields at all — still
//! negotiates, which is exactly the mixed-window case release A must
//! survive.

use hm_core::api::AppState;
use hm_core::gateway::{CoreFrame, GatewayFrame};
use hm_core::tool::mocks::MockInject;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};

/// Boot the core with a gateway listener on an ephemeral port. Returns the
/// state and the port; the TEST plays the gateway over raw TCP.
async fn spawn_hub() -> (Arc<AppState>, u16) {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], port).into());
    tokio::time::sleep(Duration::from_millis(50)).await;
    (state, port)
}

/// Management API on an ephemeral port (only the mute tests need it).
async fn spawn_api(state: &Arc<AppState>) -> String {
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

struct RawGw {
    write: Mutex<Option<tokio::net::tcp::OwnedWriteHalf>>,
    rx: Mutex<mpsc::Receiver<CoreFrame>>,
}

impl RawGw {
    async fn send_line(&self, line: &str) {
        let mut guard = self.write.lock().await;
        let w = guard.as_mut().expect("connection closed");
        w.write_all(line.as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
    }

    async fn send(&self, f: &GatewayFrame) {
        let line = serde_json::to_string(f).unwrap();
        self.send_line(&line).await;
    }

    async fn recv(&self) -> CoreFrame {
        tokio::time::timeout(Duration::from_secs(5), self.rx.lock().await.recv())
            .await
            .expect("timeout waiting for a core frame")
            .expect("connection died")
    }

    async fn until(&self, pred: impl Fn(&CoreFrame) -> bool) -> CoreFrame {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let f = tokio::time::timeout_at(deadline, self.rx.lock().await.recv())
                .await
                .expect("timeout waiting for the expected frame")
                .expect("connection died");
            if pred(&f) {
                return f;
            }
        }
    }

    /// Drop the socket from the gateway side (the real disconnect path).
    async fn close(&self) {
        self.write.lock().await.take();
    }
}

/// Connect and send the hello as a RAW line (the caller controls every
/// field, including "an old gateway that has never heard of min_protocol").
/// Reads frames until the connection ends; the HelloAck/HelloReject verdict
/// is left to the test via `recv`.
async fn connect_raw(port: u16, hello_line: &str) -> Arc<RawGw> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (rd, wr) = stream.into_split();
    let (tx, rx) = mpsc::channel(64);
    let gw = Arc::new(RawGw {
        write: Mutex::new(Some(wr)),
        rx: Mutex::new(rx),
    });
    gw.send_line(hello_line).await;
    tokio::spawn(async move {
        let mut lines = BufReader::new(rd).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let Ok(f) = serde_json::from_str::<CoreFrame>(&l) else {
                continue;
            };
            if tx.send(f).await.is_err() {
                break;
            }
        }
    });
    gw
}

fn hello_json(protocol: u32, min_protocol: u32, caps: &[&str], gateway_version: &str) -> String {
    let caps: Vec<Value> = caps.iter().map(|c| json!(c)).collect();
    json!({
        "type": "hello",
        "protocol": protocol,
        "min_protocol": min_protocol,
        "gateway": "fake-gw test",
        "gateway_version": gateway_version,
        "devices": [{
            "stable_id": "dot2-d",
            "model": "AEOBC",
            "capabilities": caps,
            "audio_profile": "fireos_builtin_speaker",
        }],
    })
    .to_string()
}

async fn wait_for_device(state: &Arc<AppState>) {
    for _ in 0..100 {
        if state.hub.device_exists("dot2-d") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("device never joined");
}

#[tokio::test]
async fn a_versioned_hello_negotiates_and_reports_the_link() {
    let (state, port) = spawn_hub().await;
    let gw = connect_raw(
        port,
        &hello_json(0, 0, &["audio_out", "remote_mute"], "0.0.99"),
    )
    .await;
    // The ack names both sides' versions and the accepted floor.
    let ack = gw.recv().await;
    let CoreFrame::HelloAck {
        protocol,
        min_protocol,
        core_version,
        ..
    } = ack
    else {
        panic!("expected hello_ack, got {ack:?}");
    };
    assert_eq!(protocol, 0);
    assert_eq!(min_protocol, hm_core::DEVICE_PROTOCOL_MIN_VERSION);
    assert!(
        !core_version.is_empty(),
        "the ack carries a machine-readable core version"
    );
    wait_for_device(&state).await;
    // /status (snapshot) exposes the negotiated link facts.
    let snap = state.hub.snapshot();
    assert_eq!(snap["protocol_operating"], json!(0), "{snap}");
    assert_eq!(snap["gateway_version"], json!("0.0.99"), "{snap}");
    assert_eq!(
        snap["last_link_error"],
        json!(null),
        "a good handshake clears the previous error: {snap}"
    );
}

#[tokio::test]
async fn a_disjoint_interval_gets_a_reject_frame_with_the_reason() {
    let (state, port) = spawn_hub().await;
    // A gateway from the far future: no overlap with [0,0].
    let gw = connect_raw(port, &hello_json(7, 7, &["audio_out"], "9.9.9")).await;
    let f = gw.recv().await;
    let CoreFrame::HelloReject {
        reason,
        protocol,
        min_protocol,
        core_version,
    } = f
    else {
        panic!("expected hello_reject, got {f:?}");
    };
    assert!(reason.contains("protocol_no_intersection"), "{reason}");
    assert_eq!(protocol, hm_core::DEVICE_PROTOCOL_VERSION);
    assert_eq!(min_protocol, hm_core::DEVICE_PROTOCOL_MIN_VERSION);
    assert!(!core_version.is_empty());
    // The core recorded WHY (ring + status), then dropped the link.
    for _ in 0..50 {
        if state.hub.snapshot()["last_link_error"].is_null() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        } else {
            break;
        }
    }
    let snap = state.hub.snapshot();
    assert_eq!(
        snap["last_link_error"]["kind"],
        json!("protocol_error"),
        "{snap}"
    );
    assert!(
        snap["last_link_error"]["reason"]
            .as_str()
            .unwrap()
            .contains("protocol_no_intersection"),
        "{snap}"
    );
    assert!(
        !snap["connected"].as_bool().unwrap_or(true),
        "a rejected hello must not connect"
    );
    // A disconnected RawGw: sending after close fails or the socket dies —
    // the point is the core refused; nothing further to drive.
    let _ = gw;
}

#[tokio::test]
async fn an_old_style_hello_without_the_new_fields_is_accepted() {
    let (state, port) = spawn_hub().await;
    // Byte-for-byte what a release-67-era gateway sends: no min_protocol,
    // no gateway_version. serde defaults must land it at operating 0.
    let old_hello = json!({
        "type": "hello",
        "protocol": 0,
        "gateway": "home-mate-device-gateway 0.0.53 (dot2-direct)",
        "devices": [{
            "stable_id": "dot2-d",
            "model": "AEOBC",
            "capabilities": ["audio_in", "audio_out", "stop", "volume", "wifi", "play_stream"],
            "audio_profile": "fireos_builtin_speaker",
        }],
    })
    .to_string();
    let gw = connect_raw(port, &old_hello).await;
    let ack = gw.recv().await;
    assert!(
        matches!(ack, CoreFrame::HelloAck { .. }),
        "an old-style hello must still negotiate: {ack:?}"
    );
    wait_for_device(&state).await;
    let snap = state.hub.snapshot();
    assert_eq!(snap["protocol_operating"], json!(0), "{snap}");
    assert_eq!(
        snap["gateway_version"],
        json!(null),
        "an old gateway declares no version: {snap}"
    );
}

#[tokio::test]
async fn duplicate_device_ids_are_refused_with_a_reason_frame() {
    let (_state, port) = spawn_hub().await;
    let caps: Vec<Value> = ["audio_out"].iter().map(|c| json!(c)).collect();
    let dev = json!({
        "stable_id": "dot2-d",
        "model": "AEOBC",
        "capabilities": caps,
        "audio_profile": "p",
    });
    let hello = json!({
        "type": "hello",
        "protocol": 0,
        "min_protocol": 0,
        "gateway": "fake-gw test",
        "devices": [dev, dev],
    })
    .to_string();
    let gw = connect_raw(port, &hello).await;
    let f = gw.recv().await;
    let CoreFrame::HelloReject { reason, .. } = f else {
        panic!("expected hello_reject, got {f:?}");
    };
    assert!(reason.contains("duplicate device id"), "{reason}");
}

#[tokio::test]
async fn remote_mute_resolves_only_on_the_correlated_device_fact() {
    let (state, port) = spawn_hub().await;
    let base = spawn_api(&state).await;
    let gw = connect_raw(
        port,
        &hello_json(0, 0, &["audio_out", "remote_mute"], "0.1.0"),
    )
    .await;
    gw.recv().await; // hello_ack
    wait_for_device(&state).await;

    // The command leaves; the fake device answers with the echoed task id.
    let cmd = tokio::task::spawn({
        let base = base.clone();
        async move {
            reqwest::Client::new()
                .put(format!("{base}/api/v0/devices/dot2-d/mute"))
                .json(&json!({ "muted": true }))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    });
    let f = gw
        .until(|f| matches!(f, CoreFrame::MuteSet { muted: true, .. }))
        .await;
    let CoreFrame::MuteSet {
        task_id,
        device,
        muted,
    } = f
    else {
        unreachable!()
    };
    assert_eq!(device, "dot2-d");
    assert!(muted);
    // A fact WITHOUT the task id (a key press) must not settle the command…
    gw.send(&GatewayFrame::MuteState {
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 1,
        muted: true,
        source: "button".into(),
        task_id: None,
    })
    .await;
    // …so the API is still waiting; now send the REAL correlated receipt.
    gw.send(&GatewayFrame::MuteState {
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 2,
        muted: true,
        source: "core_set".into(),
        task_id: Some(task_id.clone()),
    })
    .await;
    let reply = cmd.await.unwrap();
    assert_eq!(reply["applied"], json!(true), "{reply}");
    assert_eq!(reply["stage"], json!("device_confirmed"), "{reply}");
    assert_eq!(reply["muted"], json!(true), "{reply}");
    assert_eq!(reply["source"], json!("core_set"), "{reply}");
    assert_eq!(reply["task_id"], json!(task_id), "{reply}");
    // The device is the fact source: the hub ring recorded BOTH facts.
    // (The core-side muted snapshot lives in the voice pipeline /mode JSON;
    // here the hub contract is what this suite owns.)
}

#[tokio::test]
async fn remote_mute_is_refused_without_the_capability_or_when_offline() {
    let (state, port) = spawn_hub().await;
    let base = spawn_api(&state).await;
    // NO remote_mute in the descriptor.
    let gw = connect_raw(port, &hello_json(0, 0, &["audio_out"], "0.1.0")).await;
    gw.recv().await;
    wait_for_device(&state).await;
    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{base}/api/v0/devices/dot2-d/mute"))
        .json(&json!({ "muted": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        409,
        "capability gap is an operation refusal, not a disconnect"
    );
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("remote_mute"),
        "{body}"
    );

    // Device gone: the descriptor may be cached but the speaker is not there.
    gw.close().await;
    for _ in 0..50 {
        let offline = state.hub.snapshot()["devices"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["online"] == json!(false));
        if offline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let resp = client
        .put(format!("{base}/api/v0/devices/dot2-d/mute"))
        .json(&json!({ "muted": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409, "{:?}", resp.status());
}

#[tokio::test]
async fn an_unanswered_mute_is_honest_unknown() {
    let (state, port) = spawn_hub().await;
    let base = spawn_api(&state).await;
    let gw = connect_raw(
        port,
        &hello_json(0, 0, &["audio_out", "remote_mute"], "0.1.0"),
    )
    .await;
    gw.recv().await;
    wait_for_device(&state).await;
    // The device swallows the command (no receipt): the API must NOT claim
    // success — this is the audit's "发出 ≠ 生效" rule, now enforced.
    tokio::spawn({
        let gw = gw.clone();
        async move {
            // Consume (and ignore) the MuteSet so the channel does not fill.
            loop {
                match tokio::time::timeout(Duration::from_secs(1), gw.recv()).await {
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        }
    });
    let body: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/devices/dot2-d/mute"))
        .json(&json!({ "muted": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["applied"], json!(false), "{body}");
    assert_eq!(body["stage"], json!("unknown"), "{body}");
    assert!(
        body["task_id"].as_str().is_some(),
        "the task id is reported so the log can be searched: {body}"
    );
}

#[tokio::test]
async fn a_protocol_error_frame_is_recorded_and_cleared_by_the_next_handshake() {
    let (state, port) = spawn_hub().await;
    let gw = connect_raw(port, &hello_json(0, 0, &["audio_out"], "0.2.0")).await;
    gw.recv().await;
    wait_for_device(&state).await;
    assert!(state.hub.snapshot()["last_link_error"].is_null());
    // The gateway reports a rejection it made against a DIFFERENT core (or a
    // privileged-frame violation): recorded as a link fact, never fatal here.
    gw.send(&GatewayFrame::ProtocolError {
        protocol: 3,
        min_protocol: 3,
        gateway_version: "0.2.0".into(),
        reason: "protocol_no_intersection: peer accepts [3,3], this side accepts [0,0]".into(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let snap = state.hub.snapshot();
    assert_eq!(
        snap["last_link_error"]["kind"],
        json!("protocol_error"),
        "{snap}"
    );
    assert_eq!(snap["last_link_error"]["peer_protocol"], json!(3), "{snap}");
    // The link is still UP — the error describes the previous attempt.
    assert_eq!(snap["connected"], json!(true), "{snap}");
    // A fresh good handshake clears it.
    gw.close().await;
    let gw2 = connect_raw(port, &hello_json(0, 0, &["audio_out"], "0.2.1")).await;
    gw2.recv().await;
    wait_for_device(&state).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        state.hub.snapshot()["last_link_error"].is_null(),
        "a successful handshake clears the previous rejection: {}",
        state.hub.snapshot()
    );
}

#[tokio::test]
async fn disconnect_clears_gateway_version_and_protocol_operating() {
    let (state, port) = spawn_hub().await;
    let gw = connect_raw(port, &hello_json(0, 0, &["audio_out"], "0.0.99")).await;
    gw.recv().await;
    wait_for_device(&state).await;
    let snap = state.hub.snapshot();
    assert_eq!(snap["protocol_operating"], json!(0), "{snap}");
    assert_eq!(snap["gateway_version"], json!("0.0.99"), "{snap}");

    gw.close().await;
    for _ in 0..50 {
        if state.hub.snapshot()["connected"] == json!(false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let snap = state.hub.snapshot();
    assert_eq!(snap["connected"], json!(false), "{snap}");
    assert_eq!(
        snap["gateway_version"],
        json!(null),
        "disconnect must clear gateway_version: {snap}"
    );
    assert_eq!(
        snap["protocol_operating"],
        json!(null),
        "disconnect must clear protocol_operating: {snap}"
    );
}

#[tokio::test]
async fn wifi_config_capability_gate_returns_409_when_missing_wifi() {
    let (state, port) = spawn_hub().await;
    let base = spawn_api(&state).await;
    let gw = connect_raw(port, &hello_json(0, 0, &["audio_out"], "0.1.0")).await;
    gw.recv().await;
    wait_for_device(&state).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v0/devices/dot2-d/wifi"))
        .json(&json!({ "ssid": "TestNet", "psk": "password123" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        409,
        "missing wifi capability must return 409"
    );
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap_or("").contains("wifi"),
        "{body}"
    );
}

#[tokio::test]
async fn voice_mode_exposes_device_mute_source_end_to_end() {
    let (state, port) = spawn_hub().await;
    let base = spawn_api(&state).await;
    let gw = connect_raw(
        port,
        &hello_json(0, 0, &["audio_out", "remote_mute"], "0.1.0"),
    )
    .await;
    gw.recv().await;
    wait_for_device(&state).await;

    gw.send(&GatewayFrame::MuteState {
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 1,
        muted: true,
        source: "button".into(),
        task_id: None,
    })
    .await;

    let client = reqwest::Client::new();
    let mut saw_button = false;
    for _ in 0..50 {
        let mode: Value = client
            .get(format!("{base}/api/v0/mode"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(dev) = mode["devices"]
            .as_array()
            .and_then(|a| a.iter().find(|d| d["device"] == json!("dot2-d")))
        {
            if dev["muted"] == json!(true) && dev["mute_source"] == json!("button") {
                saw_button = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw_button,
        "expected /api/v0/mode to expose muted=true and mute_source='button'"
    );

    let cmd = tokio::task::spawn({
        let base = base.clone();
        async move {
            reqwest::Client::new()
                .put(format!("{base}/api/v0/devices/dot2-d/mute"))
                .json(&json!({ "muted": false }))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    });
    let f = gw
        .until(|f| matches!(f, CoreFrame::MuteSet { muted: false, .. }))
        .await;
    let CoreFrame::MuteSet { task_id, .. } = f else {
        unreachable!()
    };
    gw.send(&GatewayFrame::MuteState {
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 2,
        muted: false,
        source: "core_set".into(),
        task_id: Some(task_id),
    })
    .await;
    let reply = cmd.await.unwrap();
    assert_eq!(reply["applied"], json!(true), "{reply}");

    let mut saw_core_set = false;
    for _ in 0..50 {
        let mode: Value = client
            .get(format!("{base}/api/v0/mode"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(dev) = mode["devices"]
            .as_array()
            .and_then(|a| a.iter().find(|d| d["device"] == json!("dot2-d")))
        {
            if dev["muted"] == json!(false) && dev["mute_source"] == json!("core_set") {
                saw_core_set = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw_core_set,
        "expected /api/v0/mode to expose muted=false and mute_source='core_set'"
    );
}

#[tokio::test]
async fn cross_generation_mute_state_does_not_settle_pending_mute_set() {
    let (state, port) = spawn_hub().await;
    let base = spawn_api(&state).await;
    let gw1 = connect_raw(
        port,
        &hello_json(0, 0, &["audio_out", "remote_mute"], "0.1.0"),
    )
    .await;
    gw1.recv().await;
    wait_for_device(&state).await;

    let cmd = tokio::task::spawn({
        let base = base.clone();
        async move {
            reqwest::Client::new()
                .put(format!("{base}/api/v0/devices/dot2-d/mute"))
                .json(&json!({ "muted": true }))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    });
    let f = gw1
        .until(|f| matches!(f, CoreFrame::MuteSet { muted: true, .. }))
        .await;
    let CoreFrame::MuteSet { task_id, .. } = f else {
        unreachable!()
    };

    gw1.close().await;
    let gw2 = connect_raw(
        port,
        &hello_json(0, 0, &["audio_out", "remote_mute"], "0.1.0"),
    )
    .await;
    gw2.recv().await;
    for _ in 0..50 {
        let online = state.hub.snapshot()["devices"]
            .as_array()
            .and_then(|a| a.iter().find(|d| d["stable_id"] == json!("dot2-d")))
            .and_then(|d| d["online"].as_bool())
            .unwrap_or(false);
        if online {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    gw2.send(&GatewayFrame::MuteState {
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 1,
        muted: true,
        source: "core_set".into(),
        task_id: Some(task_id),
    })
    .await;

    let reply = cmd.await.unwrap();
    assert_eq!(
        reply["applied"],
        json!(false),
        "cross-generation MuteState must not settle gen-1 command: {reply}"
    );
    assert_eq!(reply["stage"], json!("unknown"), "{reply}");
}

#[tokio::test]
async fn hello_reject_closes_the_tcp_socket() {
    let (_state, port) = spawn_hub().await;
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let hello = hello_json(7, 7, &["audio_out"], "9.9.9");
    wr.write_all(hello.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();

    let mut lines = BufReader::new(rd).lines();
    let first = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("timeout waiting for hello_reject")
        .unwrap()
        .expect("expected hello_reject line");
    let frame: CoreFrame = serde_json::from_str(&first).unwrap();
    assert!(
        matches!(frame, CoreFrame::HelloReject { .. }),
        "expected HelloReject, got {frame:?}"
    );

    let eof = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("timeout waiting for socket EOF after HelloReject")
        .unwrap();
    assert!(
        eof.is_none(),
        "expected TCP socket to close (EOF) after HelloReject, got {eof:?}"
    );
}

#[tokio::test]
async fn device_mute_mismatch_does_not_return_applied_true() {
    let (state, port) = spawn_hub().await;
    let base = spawn_api(&state).await;
    let gw = connect_raw(
        port,
        &hello_json(0, 0, &["audio_out", "remote_mute"], "0.1.0"),
    )
    .await;
    gw.recv().await;
    wait_for_device(&state).await;

    // Case 1: muted value mismatch (requested true, device reports false)
    let cmd = tokio::task::spawn({
        let base = base.clone();
        async move {
            reqwest::Client::new()
                .put(format!("{base}/api/v0/devices/dot2-d/mute"))
                .json(&json!({ "muted": true }))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    });
    let f = gw
        .until(|f| matches!(f, CoreFrame::MuteSet { muted: true, .. }))
        .await;
    let CoreFrame::MuteSet { task_id, .. } = f else {
        unreachable!()
    };
    gw.send(&GatewayFrame::MuteState {
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 1,
        muted: false,
        source: "core_set".into(),
        task_id: Some(task_id),
    })
    .await;
    let reply = cmd.await.unwrap();
    assert_eq!(reply["applied"], json!(false), "{reply}");
    assert_eq!(reply["stage"], json!("mismatch"), "{reply}");

    // Case 2: device id mismatch (requested dot2-d, frame names other-dev)
    let cmd2 = tokio::task::spawn({
        let base = base.clone();
        async move {
            reqwest::Client::new()
                .put(format!("{base}/api/v0/devices/dot2-d/mute"))
                .json(&json!({ "muted": true }))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    });
    let f2 = gw
        .until(|f| matches!(f, CoreFrame::MuteSet { muted: true, .. }))
        .await;
    let CoreFrame::MuteSet {
        task_id: task_id2, ..
    } = f2
    else {
        unreachable!()
    };
    gw.send(&GatewayFrame::MuteState {
        device: "other-dev".into(),
        ts: hm_core::id::now_ms(),
        seq: 1,
        muted: true,
        source: "core_set".into(),
        task_id: Some(task_id2),
    })
    .await;
    let reply2 = cmd2.await.unwrap();
    assert_eq!(reply2["applied"], json!(false), "{reply2}");
    assert_eq!(reply2["stage"], json!("mismatch"), "{reply2}");
}
