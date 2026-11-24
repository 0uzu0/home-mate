//! End-to-end integration tests for #271:
//! - Concurrent multi-gateway connections (local loopback speaker + LAN phone)
//! - Per-device outbound frame routing (no cross-talk between sibling gateways)
//! - Per-device reconnect supersession (reconnecting B replaces only B; A stays online)
//! - Isolated disconnect cleanup (disconnecting B leaves A streaming/playing uninterrupted)
//! - Authenticated non-loopback LAN gateway handshake (`Hello.auth_token` -> `HelloReject(unauthorized)`)
//! - Loopback zero-config backward compatibility on a paired Core
//! - UDP 8933 LAN discovery responder (`home_mate_discover` -> `home_mate_beacon`, zero secrets)
//! - `/api/v0/status` `gateway.multi_gateway` & `gateway.voice_gateway_open`

use hm_core::api::AppState;
use hm_core::gateway::{CoreFrame, DeviceEvent, GatewayFrame, HelloDevice, StopSubmission};
use hm_core::tool::mocks::MockInject;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

struct MockGatewayClient {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl MockGatewayClient {
    async fn connect_as(
        hub: Arc<hm_core::gateway::DeviceHub>,
        simulated_peer: SocketAddr,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = hub
                .serve_connection_from(stream, Some(simulated_peer))
                .await;
        });
        let stream = TcpStream::connect(addr).await.unwrap();
        let (rd, wr) = stream.into_split();
        (
            Self {
                reader: BufReader::new(rd),
                writer: wr,
            },
            srv,
        )
    }

    async fn send(&mut self, frame: &GatewayFrame) {
        let line = serde_json::to_string(frame).unwrap();
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn recv_frame(&mut self) -> Option<CoreFrame> {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(2), self.reader.read_line(&mut line))
            .await
            .expect("timed out waiting for CoreFrame")
            .unwrap();
        if n == 0 {
            return None;
        }
        Some(serde_json::from_str(line.trim()).unwrap())
    }

    async fn expect_no_frame_for_device(&mut self, forbidden_device: &str, wait: Duration) {
        let deadline = tokio::time::Instant::now() + wait;
        while tokio::time::Instant::now() < deadline {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            let mut line = String::new();
            match tokio::time::timeout(rem, self.reader.read_line(&mut line)).await {
                Err(_) => return,
                Ok(Ok(0)) => return,
                Ok(Ok(_)) => {
                    let frame: CoreFrame = serde_json::from_str(line.trim()).unwrap();
                    let raw = serde_json::to_string(&frame).unwrap();
                    assert!(
                        !raw.contains(forbidden_device),
                        "sibling gateway leaked frame for '{forbidden_device}': {frame:?}"
                    );
                }
                Ok(Err(e)) => panic!("read error: {e}"),
            }
        }
    }

    async fn recv_matching(
        &mut self,
        forbidden_device: Option<&str>,
        mut pred: impl FnMut(&CoreFrame) -> bool,
    ) -> CoreFrame {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            let mut line = String::new();
            let n = tokio::time::timeout(rem, self.reader.read_line(&mut line))
                .await
                .expect("timed out waiting for matching CoreFrame")
                .unwrap();
            assert!(n > 0, "gateway TCP connection closed unexpectedly");
            let frame: CoreFrame = serde_json::from_str(line.trim()).unwrap();
            if let Some(forbid) = forbidden_device {
                let raw = serde_json::to_string(&frame).unwrap();
                assert!(
                    !raw.contains(forbid),
                    "cross-gateway leak for '{forbid}': {frame:?}"
                );
            }
            if pred(&frame) {
                return frame;
            }
        }
        panic!("timed out waiting for matching CoreFrame");
    }
}

fn mk_hello(gateway: &str, device_id: &str, auth_token: Option<String>) -> GatewayFrame {
    GatewayFrame::Hello {
        protocol: 2,
        min_protocol: 0,
        supported_versions: vec![0, 2],
        gateway: gateway.into(),
        gateway_version: "0.2.0".into(),
        devices: vec![HelloDevice {
            stable_id: device_id.into(),
            model: "test-model".into(),
            firmware: Some("0.2.0".into()),
            capabilities: vec![
                "audio_in".into(),
                "audio_out".into(),
                "play_stream".into(),
                "volume".into(),
                "remote_mute".into(),
            ],
            audio_profile: "default".into(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
        auth_token,
    }
}

fn boot_state() -> Arc<AppState> {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    let tmp_dir = std::env::temp_dir().join(format!(
        "hm-core-test-multi-gw-{}-{}",
        std::process::id(),
        hm_core::id::now_ms()
    ));
    AppState::with_overrides(
        store,
        tmp_dir,
        MockInject::default(),
        None,
        None,
        None,
        None,
    )
}

#[tokio::test]
async fn multi_gateway_coexistence_routing_reconnect_and_isolated_disconnect() {
    let state = boot_state();
    let token = state.auth.provision("household-owner").unwrap();
    state.hub.set_voice_gateway_open(true);
    let mut ctrl_rx = state.hub.subscribe_control();

    // 1. Connect Gateway A (local loopback speaker, zero auth_token needed).
    let (mut gw_a, _srv_a) =
        MockGatewayClient::connect_as(state.hub.clone(), "127.0.0.1:41001".parse().unwrap()).await;
    gw_a.send(&mk_hello("dot2-local-gw", "dot2-livingroom", None))
        .await;
    let ack_a = gw_a.recv_frame().await.expect("HelloAck A");
    let gen_a = match ack_a {
        CoreFrame::HelloAck { generation, .. } => generation,
        other => panic!("expected HelloAck A, got {other:?}"),
    };
    assert!(matches!(
        gw_a.recv_frame().await,
        Some(CoreFrame::TimeSet { .. })
    ));

    // 2. Connect Gateway B (remote LAN Android phone with valid Bearer token).
    let (mut gw_b1, srv_b1) =
        MockGatewayClient::connect_as(state.hub.clone(), "192.168.50.88:52001".parse().unwrap())
            .await;
    gw_b1
        .send(&mk_hello(
            "android-gateway",
            "android-fold",
            Some(token.clone()),
        ))
        .await;
    let ack_b1 = gw_b1.recv_frame().await.expect("HelloAck B1");
    let gen_b1 = match ack_b1 {
        CoreFrame::HelloAck { generation, .. } => generation,
        other => panic!("expected HelloAck B1, got {other:?}"),
    };
    assert!(gen_b1 > gen_a);
    assert!(matches!(
        gw_b1.recv_frame().await,
        Some(CoreFrame::TimeSet { .. })
    ));

    // Both gateways coexist simultaneously without superseding each other!
    let snap = state.hub.snapshot();
    assert_eq!(snap["connected"], true);
    assert_eq!(snap["multi_gateway"], true);
    assert_eq!(snap["voice_gateway_open"], true);
    assert_eq!(snap["connection_count"], 2);
    let devs = snap["devices"].as_array().unwrap();
    assert_eq!(devs.len(), 2);
    assert!(devs.iter().all(|d| d["online"].as_bool() == Some(true)));
    assert!(
        !snap.to_string().contains(&token),
        "hub snapshot must never echo auth_token"
    );

    // 3. Per-device outbound routing: frames for A go ONLY to A; frames for B go ONLY to B.
    let play_a = state
        .hub
        .play_begin("dot2-livingroom", "voice", 16_000, Some("你好客厅".into()))
        .expect("play_begin on A");
    state
        .hub
        .play_chunk(&play_a.play_id, 0, &[0u8; 320])
        .expect("play_chunk on A");
    state.hub.play_end(&play_a.play_id).expect("play_end on A");
    state
        .hub
        .volume_set("dot2-livingroom", 0.55)
        .expect("volume_set on A");

    let mut saw_begin = false;
    let mut saw_chunk = false;
    let mut saw_end = false;
    let mut saw_vol = false;
    while !(saw_begin && saw_chunk && saw_end && saw_vol) {
        let f = gw_a.recv_matching(Some("android-fold"), |_| true).await;
        match f {
            CoreFrame::PlayBegin { ref device, .. } if device == "dot2-livingroom" => {
                saw_begin = true
            }
            CoreFrame::PlayChunk {
                ref task_id,
                seq: 0,
                ..
            } if task_id == &play_a.play_id => saw_chunk = true,
            CoreFrame::PlayEnd { ref task_id } if task_id == &play_a.play_id => saw_end = true,
            CoreFrame::VolumeSet { ref device, .. } if device == "dot2-livingroom" => {
                saw_vol = true
            }
            _ => {}
        }
    }
    // Gateway B received ZERO of A's frames!
    gw_b1
        .expect_no_frame_for_device("dot2-livingroom", Duration::from_millis(80))
        .await;

    // Stop on A works even though B connected with a higher generation!
    let stop_sub = state.hub.stop_and_wait(&play_a.play_id);
    assert!(
        matches!(stop_sub, StopSubmission::Accepted { connection_generation, .. } if connection_generation == gen_a),
        "stop_and_wait must target A's generation ({gen_a}) even when newer sibling B ({gen_b1}) exists"
    );
    gw_a.recv_matching(
        Some("android-fold"),
        |f| matches!(f, CoreFrame::Stop { task_id, flush: true } if task_id == &play_a.play_id),
    )
    .await;

    // Now route frames to B only.
    state
        .hub
        .mic_session("android-fold", "start")
        .expect("mic_session on B");
    state
        .hub
        .assistant_state("android-fold", "listening")
        .expect("assistant_state on B");
    let mut saw_mic = false;
    let mut saw_state = false;
    while !(saw_mic && saw_state) {
        let f = gw_b1.recv_matching(Some("dot2-livingroom"), |_| true).await;
        match f {
            CoreFrame::MicSession {
                ref device,
                ref action,
                ..
            } if device == "android-fold" && action == "start" => saw_mic = true,
            CoreFrame::AssistantState {
                ref device,
                ref state,
            } if device == "android-fold" && state == "listening" => saw_state = true,
            _ => {}
        }
    }
    gw_a.expect_no_frame_for_device("android-fold", Duration::from_millis(80))
        .await;

    // 4. Reconnect device B (android-fold): supersedes ONLY B1; A stays online & uninterrupted!
    let (mut gw_b2, _srv_b2) =
        MockGatewayClient::connect_as(state.hub.clone(), "192.168.50.88:52002".parse().unwrap())
            .await;
    gw_b2
        .send(&mk_hello(
            "android-gateway",
            "android-fold",
            Some(token.clone()),
        ))
        .await;
    assert!(matches!(
        gw_b2.recv_frame().await,
        Some(CoreFrame::HelloAck { .. })
    ));
    assert!(matches!(
        gw_b2.recv_frame().await,
        Some(CoreFrame::TimeSet { .. })
    ));
    // Old B1 task must terminate promptly via retirement!
    tokio::time::timeout(Duration::from_secs(2), srv_b1)
        .await
        .expect("old B1 task retired")
        .unwrap();

    // Gateway A still works uninterrupted and B2 now receives android-fold frames.
    state
        .hub
        .assistant_state("dot2-livingroom", "thinking")
        .unwrap();
    state
        .hub
        .assistant_state("android-fold", "thinking")
        .unwrap();
    gw_a.recv_matching(Some("android-fold"), |f| {
        matches!(f, CoreFrame::AssistantState { device, state } if device == "dot2-livingroom" && state == "thinking")
    })
    .await;
    gw_b2
        .recv_matching(Some("dot2-livingroom"), |f| {
            matches!(f, CoreFrame::AssistantState { device, state } if device == "android-fold" && state == "thinking")
        })
        .await;

    // 5. Disconnect B2: ONLY android-fold goes offline; dot2-livingroom stays online and working!
    drop(gw_b2);
    let mut saw_b_left = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(50), ctrl_rx.recv()).await {
            if let DeviceEvent::DeviceLeft { device } = ev {
                assert_ne!(
                    device, "dot2-livingroom",
                    "disconnecting B must never emit DeviceLeft for A"
                );
                if device == "android-fold" {
                    saw_b_left = true;
                    break;
                }
            }
        }
    }
    assert!(saw_b_left, "expected DeviceLeft for android-fold");

    let snap_after = state.hub.snapshot();
    assert_eq!(snap_after["connected"], true);
    assert_eq!(snap_after["connection_count"], 1);
    let dev_a = snap_after["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["stable_id"] == "dot2-livingroom")
        .unwrap();
    let dev_b = snap_after["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["stable_id"] == "android-fold")
        .unwrap();
    assert_eq!(dev_a["online"], true);
    assert_eq!(dev_b["online"], false);

    // Offline B must NOT misroute to A!
    assert!(state
        .hub
        .assistant_state("android-fold", "speaking")
        .is_err());
    gw_a.expect_no_frame_for_device("android-fold", Duration::from_millis(60))
        .await;

    // A still plays & receives audio uninterrupted!
    state
        .hub
        .assistant_state("dot2-livingroom", "speaking")
        .unwrap();
    gw_a.recv_matching(Some("android-fold"), |f| {
        matches!(f, CoreFrame::AssistantState { device, state } if device == "dot2-livingroom" && state == "speaking")
    })
    .await;
}

#[tokio::test]
async fn lan_gateway_requires_valid_auth_token_on_paired_core_while_loopback_stays_open() {
    let state = boot_state();
    let token = state.auth.provision("owner").unwrap();
    state.hub.set_voice_gateway_open(true);

    // 1. Non-loopback peer WITHOUT auth_token -> HelloReject(reason="unauthorized")
    let (mut unauth, srv1) =
        MockGatewayClient::connect_as(state.hub.clone(), "192.168.50.99:53001".parse().unwrap())
            .await;
    unauth
        .send(&mk_hello("rogue-phone", "android-rogue", None))
        .await;
    match unauth.recv_frame().await {
        Some(CoreFrame::HelloReject { reason, .. }) => assert_eq!(reason, "unauthorized"),
        other => panic!("expected HelloReject(unauthorized), got {other:?}"),
    }
    tokio::time::timeout(Duration::from_secs(2), srv1)
        .await
        .unwrap()
        .unwrap();
    assert!(!state.hub.device_exists("android-rogue"));

    // 2. Non-loopback peer WITH invalid auth_token -> HelloReject(reason="unauthorized")
    let (mut bad_tok, srv2) =
        MockGatewayClient::connect_as(state.hub.clone(), "192.168.50.99:53002".parse().unwrap())
            .await;
    bad_tok
        .send(&mk_hello(
            "rogue-phone",
            "android-rogue-2",
            Some("hm1_wrong_token".into()),
        ))
        .await;
    match bad_tok.recv_frame().await {
        Some(CoreFrame::HelloReject { reason, .. }) => assert_eq!(reason, "unauthorized"),
        other => panic!("expected HelloReject(unauthorized), got {other:?}"),
    }
    tokio::time::timeout(Duration::from_secs(2), srv2)
        .await
        .unwrap()
        .unwrap();
    assert!(!state.hub.device_exists("android-rogue-2"));

    // 3. Non-loopback peer WITH valid auth_token -> HelloAck
    let (mut good_lan, _srv3) =
        MockGatewayClient::connect_as(state.hub.clone(), "192.168.50.99:53003".parse().unwrap())
            .await;
    good_lan
        .send(&mk_hello(
            "trusted-phone",
            "android-valid",
            Some(token.clone()),
        ))
        .await;
    assert!(matches!(
        good_lan.recv_frame().await,
        Some(CoreFrame::HelloAck { .. })
    ));
    assert!(state.hub.device_exists("android-valid"));

    // 4. Loopback peer WITHOUT auth_token on the SAME paired Core -> still accepted!
    let (mut local_gw, _srv4) =
        MockGatewayClient::connect_as(state.hub.clone(), "127.0.0.1:41002".parse().unwrap()).await;
    local_gw
        .send(&mk_hello("local-dot2", "dot2-local", None))
        .await;
    assert!(matches!(
        local_gw.recv_frame().await,
        Some(CoreFrame::HelloAck { .. })
    ));
    assert!(state.hub.device_exists("dot2-local"));
    assert_eq!(state.hub.snapshot()["connection_count"], 2);

    // Secrets never leak into hub ring events or snapshot.
    let recent_str = serde_json::to_string(&state.hub.recent(50)).unwrap();
    assert!(!recent_str.contains(&token));
    assert!(!recent_str.contains("hm1_wrong_token"));
}

#[tokio::test]
async fn status_and_udp_discovery_beacon_advertise_multi_gateway_without_leaking_secrets() {
    let state = boot_state();
    let token = state.auth.provision("owner").unwrap();
    state
        .config
        .save_device_meta("dot2-living", "客厅音箱", "客厅", "test")
        .unwrap();
    state.hub.set_voice_gateway_open(true);

    // Boot HTTP API router and check /api/v0/status exposes multi_gateway + voice_gateway_open.
    let api = hm_core::api::router(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            api.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let status: Value = reqwest::Client::new()
        .get(format!("http://{http_addr}/api/v0/status"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["gateway"]["multi_gateway"], json!(true));
    assert_eq!(status["gateway"]["voice_gateway_open"], json!(true));

    // Boot UDP discovery responder and send both junk + real home_mate_discover probe.
    let (sock, disc_addr) =
        hm_core::discovery::bind_discovery_socket("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
    let disc_task = tokio::spawn(hm_core::discovery::run_discovery_loop(
        sock,
        state.clone(),
        http_addr.port(),
        8932,
        None,
    ));

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // Garbage packet must be ignored without crashing responder.
    client.send_to(b"not-json-at-all", disc_addr).await.unwrap();
    client
        .send_to(br#"{"type":"unrelated"}"#, disc_addr)
        .await
        .unwrap();
    // Real discover probe receives unicast home_mate_beacon immediately.
    client
        .send_to(br#"{"type":"home_mate_discover"}"#, disc_addr)
        .await
        .unwrap();

    let mut buf = [0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .expect("beacon response arrived")
        .unwrap();
    let raw = std::str::from_utf8(&buf[..n]).unwrap();
    assert!(!raw.contains(&token), "beacon must never contain token");
    let beacon: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(beacon["type"], "home_mate_beacon");
    assert_eq!(beacon["version"], hm_core::CORE_VERSION);
    assert_eq!(beacon["assistant_name"], state.effective_assistant_name());
    assert_eq!(beacon["room_name"], "客厅");
    assert_eq!(beacon["web_port"], http_addr.port());
    assert_eq!(beacon["gateway_port"], 8932);
    assert_eq!(beacon["voice_gateway_open"], true);
    assert_eq!(beacon["auth_required"], true);
    assert_eq!(beacon["multi_gateway"], true);

    disc_task.abort();
}

#[tokio::test]
async fn sibling_isolation_blocks_mid_play_leak_unknown_device_leak_and_spoofed_frames() {
    let state = boot_state();
    let token = state.auth.provision("owner").unwrap();
    state.hub.set_voice_gateway_open(true);
    let mut ctrl_rx = state.hub.subscribe_control();

    // Connect Gateway A (dot2-livingroom, with wifi capability) + Gateway B (android-fold).
    let (mut gw_a, _srv_a) =
        MockGatewayClient::connect_as(state.hub.clone(), "127.0.0.1:41010".parse().unwrap()).await;
    let mut hello_a = mk_hello("dot2-local-gw", "dot2-livingroom", None);
    if let GatewayFrame::Hello {
        ref mut devices, ..
    } = hello_a
    {
        devices[0].capabilities.push("wifi".into());
    }
    gw_a.send(&hello_a).await;
    assert!(matches!(
        gw_a.recv_frame().await,
        Some(CoreFrame::HelloAck { .. })
    ));
    assert!(matches!(
        gw_a.recv_frame().await,
        Some(CoreFrame::TimeSet { .. })
    ));

    let (mut gw_b, _srv_b) =
        MockGatewayClient::connect_as(state.hub.clone(), "192.168.50.88:52010".parse().unwrap())
            .await;
    gw_b.send(&mk_hello(
        "android-gateway",
        "android-fold",
        Some(token.clone()),
    ))
    .await;
    assert!(matches!(
        gw_b.recv_frame().await,
        Some(CoreFrame::HelloAck { .. })
    ));
    assert!(matches!(
        gw_b.recv_frame().await,
        Some(CoreFrame::TimeSet { .. })
    ));

    // Set up a pending volume task and pending wifi waiter owned by Gateway A.
    let vol_task_a = state.hub.volume_set("dot2-livingroom", 0.4).unwrap();
    let mut wifi_rx_a = state
        .hub
        .wifi_config("dot2-livingroom", "HomeWiFi".into(), "secret".into())
        .unwrap();
    // Drain A's legitimate VolumeSet & WifiConfig frames.
    gw_a.recv_matching(Some("android-fold"), |f| {
        matches!(f, CoreFrame::WifiConfig { .. })
    })
    .await;

    // 1. Sibling B tries to spoof VolumeReceipt, ModeReport, WifiState, KwsDeployReceipt,
    //    and DeviceStatus(online=false, descriptor=Some) targeting Gateway A!
    gw_b.send(&GatewayFrame::VolumeReceipt {
        task_id: vol_task_a.clone(),
        effective: 0.99,
        unit: "linear".into(),
        stage: "device_confirmed".into(),
        device: Some("dot2-livingroom".into()),
    })
    .await;
    gw_b.send(&GatewayFrame::VolumeReceipt {
        task_id: vol_task_a.clone(),
        effective: 0.98,
        unit: "linear".into(),
        stage: "device_confirmed".into(),
        device: None,
    })
    .await;
    gw_b.send(&GatewayFrame::ModeReport {
        device: "dot2-livingroom".into(),
        duplex: hm_core::gateway::DuplexState {
            capability: "full".into(),
            requested: "full".into(),
            negotiated: "full".into(),
            effective: "full".into(),
        },
        aec: hm_core::gateway::AecState::default(),
    })
    .await;
    gw_b.send(&GatewayFrame::WifiState {
        device: "dot2-livingroom".into(),
        ts: 1,
        ssid: "EvilNet".into(),
        state: "applied".into(),
        detail: None,
    })
    .await;
    gw_b.send(&GatewayFrame::KwsDeployReceipt {
        device: "dot2-livingroom".into(),
        accepted: true,
        hash: "spoof".into(),
        keywords: 1,
        detail: "spoof".into(),
        local_exit: 0,
        intents_hash: String::new(),
        threshold: 0.0,
        score: 0.0,
    })
    .await;
    gw_b.send(&GatewayFrame::DeviceStatus {
        device: "dot2-livingroom".into(),
        online: false,
        reason: "spoofed_drop".into(),
        descriptor: Some(HelloDevice {
            stable_id: "dot2-livingroom".into(),
            model: "spoof".into(),
            firmware: None,
            capabilities: vec![],
            audio_profile: "spoof".into(),
            wifi_ssid: None,
        }),
        northbound: None,
    })
    .await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify NONE of the spoofed frames affected Gateway A!
    let snap = state.hub.snapshot();
    assert_eq!(snap["connection_count"], 2);
    let dev_a = snap["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["stable_id"] == "dot2-livingroom")
        .unwrap();
    assert_eq!(
        dev_a["online"], true,
        "spoofed DeviceStatus(offline) must not knock A offline"
    );
    assert!(
        dev_a["volume"].is_null(),
        "spoofed VolumeReceipt from B must not overwrite A's volume"
    );
    assert!(
        wifi_rx_a.try_recv().is_err(),
        "spoofed WifiState from B must not resolve A's pending wifi waiter"
    );
    while let Ok(ev) = ctrl_rx.try_recv() {
        match ev {
            DeviceEvent::ModeReport { ref device, .. }
            | DeviceEvent::WifiState { ref device, .. }
            | DeviceEvent::KwsDeployReceipt { ref device, .. }
                if device == "dot2-livingroom" =>
            {
                panic!("spoofed sibling event for dot2-livingroom leaked into control bus: {ev:?}");
            }
            _ => {}
        }
    }

    // 2. Start a play on B, then disconnect B mid-play while A stays as the SOLE live gateway.
    //    Subsequent play_chunk / play_end / stop for play_b MUST error and NEVER leak onto A!
    let play_b = state
        .hub
        .play_begin("android-fold", "voice", 16_000, Some("B播报".into()))
        .expect("play_begin on B");
    drop(gw_b);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if state.hub.snapshot()["connection_count"] == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state.hub.snapshot()["connection_count"], 1);

    assert!(
        state
            .hub
            .play_chunk(&play_b.play_id, 0, &[1u8; 320])
            .is_err(),
        "play_chunk for disconnected B must fail, never route to surviving A"
    );
    assert!(
        state.hub.play_end(&play_b.play_id).is_err(),
        "play_end for disconnected B must fail, never route to surviving A"
    );
    assert!(
        state.hub.stop(&play_b.play_id).is_err(),
        "stop for disconnected B must fail, never route to surviving A"
    );

    // 3. Targeting an unknown/never-connected device while A is the sole connected gateway
    //    MUST fail honestly and NEVER leak onto A!
    assert!(
        state
            .hub
            .assistant_state("ghost-speaker", "listening")
            .is_err(),
        "unknown device must fail instead of misrouting to sole live gateway A"
    );
    assert!(
        state.hub.volume_set("ghost-speaker", 0.5).is_err(),
        "volume_set for unknown device must fail instead of misrouting to A"
    );
    gw_a.expect_no_frame_for_device("ghost-speaker", Duration::from_millis(60))
        .await;
    gw_a.expect_no_frame_for_device(&play_b.play_id, Duration::from_millis(60))
        .await;
}

#[tokio::test]
async fn option_1a_lan_gateway_exempt_tcp_8932_allows_private_lan_and_blocks_public_and_bad_tokens()
{
    let state = boot_state();
    let _token = state.auth.provision("owner").unwrap();
    state.hub.set_voice_gateway_open(true);

    // 1. Fail-closed by default: RFC1918 peer without token is rejected.
    let (mut lan_before, _s0) =
        MockGatewayClient::connect_as(state.hub.clone(), "192.168.1.55:44001".parse().unwrap())
            .await;
    lan_before
        .send(&mk_hello("android-panel", "panel-1", None))
        .await;
    assert!(matches!(
        lan_before.recv_frame().await,
        Some(CoreFrame::HelloReject { ref reason, .. }) if reason == "unauthorized"
    ));

    // 2. Enable Option 1A lan_gateway_exempt=true.
    state.auth.set_lan_gateway_exempt(true).unwrap();

    // Private RFC1918 peer without token -> accepted!
    let (mut lan_ok, _s1) =
        MockGatewayClient::connect_as(state.hub.clone(), "192.168.1.55:44002".parse().unwrap())
            .await;
    lan_ok
        .send(&mk_hello("android-panel", "panel-1", None))
        .await;
    assert!(matches!(
        lan_ok.recv_frame().await,
        Some(CoreFrame::HelloAck { .. })
    ));

    // Tailscale CGNAT peer without token -> accepted!
    let (mut ts_ok, _s2) =
        MockGatewayClient::connect_as(state.hub.clone(), "100.64.10.20:44003".parse().unwrap())
            .await;
    ts_ok
        .send(&mk_hello("android-phone", "phone-1", None))
        .await;
    assert!(matches!(
        ts_ok.recv_frame().await,
        Some(CoreFrame::HelloAck { .. })
    ));
    assert_eq!(state.hub.snapshot()["connection_count"], 2);

    // Private RFC1918 peer presenting a WRONG token -> STILL rejected!
    let (mut lan_bad_tok, _s3) =
        MockGatewayClient::connect_as(state.hub.clone(), "192.168.1.56:44004".parse().unwrap())
            .await;
    lan_bad_tok
        .send(&mk_hello(
            "android-panel",
            "panel-bad",
            Some("hm1_wrong_token".into()),
        ))
        .await;
    assert!(matches!(
        lan_bad_tok.recv_frame().await,
        Some(CoreFrame::HelloReject { ref reason, .. }) if reason == "unauthorized"
    ));

    // Public IP peer without token -> STILL rejected even with lan_gateway_exempt=true!
    let (mut pub_no_tok, _s4) =
        MockGatewayClient::connect_as(state.hub.clone(), "203.0.113.9:44005".parse().unwrap())
            .await;
    pub_no_tok
        .send(&mk_hello("rogue-client", "rogue-1", None))
        .await;
    assert!(matches!(
        pub_no_tok.recv_frame().await,
        Some(CoreFrame::HelloReject { ref reason, .. }) if reason == "unauthorized"
    ));

    // Regression: an unauthenticated LAN-exempt :8932 Hello must NEVER wipe the shared
    // per-IP auth-failure counter used to throttle brute-force attempts against admin routes.
    let brute_ip: std::net::IpAddr = "192.168.1.77".parse().unwrap();
    for _ in 0..19 {
        state.auth.record_auth_failure(brute_ip);
    }
    assert!(state.auth.is_rate_limited(brute_ip).is_none());
    let (mut exempt_probe, _s5) = MockGatewayClient::connect_as(
        state.hub.clone(),
        std::net::SocketAddr::new(brute_ip, 44006),
    )
    .await;
    exempt_probe
        .send(&mk_hello("android-probe", "probe-1", None))
        .await;
    assert!(matches!(
        exempt_probe.recv_frame().await,
        Some(CoreFrame::HelloAck { .. })
    ));
    // 20th failure from the same IP must STILL trigger rate limiting!
    state.auth.record_auth_failure(brute_ip);
    assert!(
        state.auth.is_rate_limited(brute_ip).is_some(),
        "unauthenticated LAN-exempt hello must not reset per-IP auth failure counter"
    );
}

#[tokio::test]
async fn phase3_loopback_hijack_blocked_and_primary_device_priority_preserved() {
    let state = boot_state();
    let _token = state.auth.provision("owner").unwrap();
    state.hub.set_voice_gateway_open(true);
    state.auth.set_lan_gateway_exempt(true).unwrap();

    // 1. Local loopback speaker gateway registers dot2-d (with config_apply_v2).
    let (mut local_gw, _s1) =
        MockGatewayClient::connect_as(state.hub.clone(), "127.0.0.1:45001".parse().unwrap()).await;
    let dot2_hello = GatewayFrame::Hello {
        protocol: 2,
        min_protocol: 0,
        supported_versions: vec![0, 2],
        gateway: "hm-firmware-dot2".into(),
        gateway_version: "0.0.53".into(),
        aec_tail_ms: None,
        profile_version: None,
        auth_token: None,
        devices: vec![HelloDevice {
            stable_id: "dot2-d".into(),
            model: "test-model".into(),
            firmware: Some("hm-firmware 0.0.53".into()),
            capabilities: vec![
                "audio_out".into(),
                "audio_in".into(),
                "config_apply_v2".into(),
            ],
            audio_profile: "test".into(),
            wifi_ssid: None,
        }],
    };
    local_gw.send(&dot2_hello).await;
    assert!(matches!(
        local_gw.recv_frame().await,
        Some(CoreFrame::HelloAck { .. })
    ));

    // 2. P0-1: Unauthenticated LAN peer attempting to hijack dot2-d via Hello is rejected!
    let (mut rogue_hello, _s2) =
        MockGatewayClient::connect_as(state.hub.clone(), "192.168.1.90:45002".parse().unwrap())
            .await;
    rogue_hello
        .send(&mk_hello("rogue-android", "dot2-d", None))
        .await;
    assert!(matches!(
        rogue_hello.recv_frame().await,
        Some(CoreFrame::HelloReject { ref reason, .. }) if reason == "device_id_reserved"
    ));

    // 3. P1-1: Legitimate LAN Android companion ("android-a" < "dot2-d" alphabetically) connects.
    let (mut android_gw, _s3) =
        MockGatewayClient::connect_as(state.hub.clone(), "192.168.1.91:45003".parse().unwrap())
            .await;
    android_gw
        .send(&mk_hello("home-mate-gateway-android", "android-a", None))
        .await;
    assert!(matches!(
        android_gw.recv_frame().await,
        Some(CoreFrame::HelloAck { .. })
    ));

    // Loopback speaker dot2-d MUST remain primary_audio_out, devices[0], and summary gateway!
    let primary = state.hub.primary_audio_out().expect("primary audio out");
    assert_eq!(primary.stable_id, "dot2-d");
    let snap = state.hub.snapshot();
    assert_eq!(snap["gateway"], "hm-firmware-dot2");
    assert_eq!(snap["devices"][0]["stable_id"], "dot2-d");
    assert_eq!(snap["devices"][0]["loopback"], true);
    assert_eq!(snap["devices"][1]["stable_id"], "android-a");
    assert_eq!(snap["devices"][1]["loopback"], false);

    // 4. P0-1 (DeviceStatus path): LAN Android cannot steal dot2-d via hot-join DeviceStatus.
    android_gw
        .send(&GatewayFrame::DeviceStatus {
            device: "dot2-d".into(),
            online: true,
            reason: String::new(),
            descriptor: Some(HelloDevice {
                stable_id: "dot2-d".into(),
                model: "spoof".into(),
                firmware: Some("spoof".into()),
                capabilities: vec!["audio_out".into()],
                audio_profile: "spoof".into(),
                wifi_ssid: None,
            }),
            northbound: None,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    let snap2 = state.hub.snapshot();
    assert_eq!(snap2["devices"][0]["stable_id"], "dot2-d");
    assert_eq!(snap2["devices"][0]["model"], "test-model");
    assert_eq!(snap2["devices"][0]["loopback"], true);

    // 5. P0-2: Cross-connection config_state_waiter spoofing by android-a must NOT resolve dot2-d's waiter!
    let hub_clone = state.hub.clone();
    let cfg_fut = tokio::spawn(async move {
        hub_clone
            .config_state_get("dot2-d", Duration::from_secs(2))
            .await
    });
    // Wait until dot2-d receives ConfigStateGet and capture its real request_id
    let req_id = loop {
        match local_gw.recv_frame().await {
            Some(CoreFrame::ConfigStateGet { request_id, .. }) => break request_id,
            Some(_) => continue,
            None => panic!("local_gw closed before ConfigStateGet"),
        }
    };
    // android-a tries to spoof ConfigState with dot2-d's request_id using its own device id!
    android_gw
        .send(&GatewayFrame::ConfigState {
            request_id: Some(req_id.clone()),
            device: "android-a".into(),
            gateway_boot_id: Some("spoof-boot".into()),
            state_revision: Some(999),
            blocks: vec![],
            capability_fingerprint: None,
            ts: 1,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !cfg_fut.is_finished(),
        "sibling gateway must NEVER resolve another connection's config_state_waiter"
    );
    // Real owner dot2-d replies and resolves cleanly!
    local_gw
        .send(&GatewayFrame::ConfigState {
            request_id: Some(req_id),
            device: "dot2-d".into(),
            gateway_boot_id: Some("dot2-real-boot".into()),
            state_revision: Some(7),
            blocks: vec![],
            capability_fingerprint: None,
            ts: 2,
        })
        .await;
    let resolved = cfg_fut
        .await
        .unwrap()
        .expect("dot2-d config_state resolved");
    match resolved {
        GatewayFrame::ConfigState {
            gateway_boot_id,
            state_revision,
            ..
        } => {
            assert_eq!(gateway_boot_id.as_deref(), Some("dot2-real-boot"));
            assert_eq!(state_revision, Some(7));
        }
        other => panic!("unexpected resolved frame: {other:?}"),
    }
}
