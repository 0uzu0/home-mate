//! Batch 4 security hardening regression suite (D1-01, D1-02, D1-03):
//! - D1-01: `lan_gateway_exempt` removes sensitive read routes (`/api/v0/voice/events`,
//!   `/api/v0/tasks`, `/api/v0/memories`), blocks non-`dev:` admin sessions, and filters
//!   loopback-protected speakers from `/api/v0/voice/log`, `/api/v0/devices`, and
//!   `/api/v0/reminders`.
//! - D1-02: Gateway caps (`MAX_DEVICES_PER_HELLO = 8`, `MAX_CONCURRENT_GATEWAY_CONNS = 32`),
//!   offline non-reserved `DeviceSlot` pruning on disconnect while preserving loopback speakers.
//! - D1-03: Tool-layer `lan_exempt` blocks `MemoryTool`, `WorkingMemory{Set,Get,Delete}Tool`,
//!   `UserProfileProposeTool`, `OpenApiCallTool` (`Destructive` / Vault-credentialed `Query`),
//!   and credentialed `OpenSocket` (`Query` with Vault token).

use axum::extract::ConnectInfo;
use hm_core::api::{router, AppState};
use hm_core::config::{EffectLevel, OpenApiConfig, OpenApiOperation};
use hm_core::execution::Authority;
use hm_core::gateway::{CoreFrame, DeviceHub, GatewayFrame, HelloDevice, InputOrigin};
use hm_core::llm::fake::FakeLlm;
use hm_core::speech::{SpeechError, TtsAudio, TtsClient};
use hm_core::store::Store;
use hm_core::tool::mocks::MockInject;
use hm_core::tool::open_socket::{HttpMethod, OpenSocketContract};
use hm_core::tool::{SideEffect, ToolCtx, ToolStatus};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hm-batch4-{tag}-{}-{}",
            std::process::id(),
            hm_core::id::next("D")
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct InstantTts;
#[async_trait::async_trait]
impl TtsClient for InstantTts {
    async fn synthesize(&self, _text: &str) -> Result<TtsAudio, SpeechError> {
        let pcm = vec![0u8; 3200];
        let wav = hm_core::speech::pcm_to_wav(&pcm, 16_000, 1);
        Ok(TtsAudio {
            wav,
            sample_rate: 16_000,
            channels: 1,
            duration_ms: 100,
            provider: "fake-tts".into(),
            voice: "fake".into(),
        })
    }
    fn provider(&self) -> &str {
        "fake-tts"
    }
}

struct MockGw {
    tx: mpsc::UnboundedSender<GatewayFrame>,
    _srv: tokio::task::JoinHandle<()>,
    _r: tokio::task::JoinHandle<()>,
    _w: tokio::task::JoinHandle<()>,
}

impl MockGw {
    async fn connect(hub: Arc<DeviceHub>, peer: SocketAddr, hello: GatewayFrame) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let _ = hub.serve_connection_from(stream, Some(peer)).await;
            }
        });
        let stream = TcpStream::connect(addr).await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let (tx, mut tx_rx) = mpsc::unbounded_channel::<GatewayFrame>();
        let (rx_tx, mut rx) = mpsc::unbounded_channel::<CoreFrame>();
        let w = tokio::spawn(async move {
            while let Some(frame) = tx_rx.recv().await {
                let Ok(line) = serde_json::to_string(&frame) else {
                    continue;
                };
                if wr.write_all(line.as_bytes()).await.is_err()
                    || wr.write_all(b"\n").await.is_err()
                {
                    break;
                }
            }
        });
        let r = tokio::spawn(async move {
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(frame) = serde_json::from_str::<CoreFrame>(&line) {
                    let _ = rx_tx.send(frame);
                }
            }
        });
        tx.send(hello).unwrap();
        let ack = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("hello_ack timeout")
            .expect("connection closed before hello_ack");
        assert!(matches!(ack, CoreFrame::HelloAck { .. }), "got {ack:?}");
        Self {
            tx,
            _srv: srv,
            _r: r,
            _w: w,
        }
    }

    fn send(&self, frame: GatewayFrame) {
        self.tx.send(frame).unwrap();
    }

    fn disconnect(self) {
        drop(self.tx);
        self._w.abort();
        self._r.abort();
    }
}

fn hello_frame(device_id: &str, caps: &[&str]) -> GatewayFrame {
    GatewayFrame::Hello {
        protocol: 1,
        supported_versions: vec![0, 1],
        min_protocol: 0,
        gateway: format!("mock-gw-{device_id}"),
        gateway_version: "0.31.0".into(),
        devices: vec![HelloDevice {
            stable_id: device_id.into(),
            model: "AEOBC".into(),
            firmware: Some("hm-firmware 0.31.0 abc123 (emos)".into()),
            capabilities: caps.iter().map(|s| (*s).to_string()).collect(),
            audio_profile: "default".into(),
            wifi_ssid: None,
        }],
        aec_tail_ms: Some(160),
        profile_version: None,
        auth_token: None,
    }
}

fn build_state(dir: &TempDir) -> Arc<AppState> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let state = AppState::with_overrides(
        store,
        dir.0.clone(),
        MockInject::default(),
        Some(Arc::new(FakeLlm::new(vec![]))),
        Some(Arc::new(InstantTts)),
        None,
        None,
    );
    state.voice.set_memory_extraction(false);
    state.voice.start();
    state
}

async fn spawn_http_with_peer(
    state: Arc<AppState>,
    peer: SocketAddr,
) -> (String, tokio::task::JoinHandle<()>) {
    let app = router(state).layer(axum::middleware::from_fn(
        move |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
            req.extensions_mut().insert(ConnectInfo(peer));
            next.run(req).await
        },
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn batch4_lan_exempt_hardening_and_gateway_hello_caps() {
    let dir = TempDir::new("security");
    let state = build_state(&dir);
    state.auth.set_lan_gateway_exempt(true).unwrap();
    let token = state.auth.provision("admin-lan").unwrap();

    // Connect a loopback speaker so it becomes loopback-protected.
    let loopback_gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50901".parse().unwrap(),
        hello_frame("dot2-local", &["audio_in", "audio_out"]),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(state.hub.device_is_loopback_protected("dot2-local"));

    // Seed an admin Web Console session message (non-dev:).
    let admin_sid = state.sessions.ensure("admin-console-1").unwrap();
    state
        .sessions
        .append(&admin_sid, "user", "private admin question")
        .unwrap();

    // Connect a LAN gateway too, and emit real Keyword events on both to test voice_log filtering.
    let lan_gw = MockGw::connect(
        state.hub.clone(),
        "192.168.10.88:50902".parse().unwrap(),
        hello_frame("android-lan-2", &["audio_in", "audio_out"]),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!state.hub.device_is_loopback_protected("android-lan-2"));

    let now = hm_core::id::now_ms();
    loopback_gw.send(GatewayFrame::Keyword {
        origin: InputOrigin::Legacy,
        device: "dot2-local".into(),
        ts: now,
        seq: 1,
        keyword: "dot2 secret wake".into(),
        fed_ms: Some(400),
        self_heard: false,
        rms_dbfs: Some(-20.0),
        lag_ms: Some(0),
        bundle_id: None,
        kws_generation: None,
        kws_tier: Some("real".into()),
        wake_dur_ms: Some(400),
        max_gap_ms: Some(80),
        l2_passed: Some(true),
    });
    lan_gw.send(GatewayFrame::Keyword {
        origin: InputOrigin::Legacy,
        device: "android-lan-2".into(),
        ts: now + 10,
        seq: 1,
        keyword: "lan wake ok".into(),
        fed_ms: Some(400),
        self_heard: false,
        rms_dbfs: Some(-20.0),
        lag_ms: Some(0),
        bundle_id: None,
        kws_generation: None,
        kws_tier: Some("real".into()),
        wake_dur_ms: Some(400),
        max_gap_ms: Some(80),
        l2_passed: Some(true),
    });
    tokio::time::sleep(Duration::from_millis(80)).await;

    let lan_peer: SocketAddr = "192.168.10.88:55111".parse().unwrap();
    let (base, _srv) = spawn_http_with_peer(state.clone(), lan_peer).await;
    let client = reqwest::Client::new();

    // D1-01: Sensitive read routes require Bearer auth even under lan_gateway_exempt.
    for path in ["/api/v0/voice/events", "/api/v0/tasks", "/api/v0/memories"] {
        let status = client
            .get(format!("{base}{path}"))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(
            status, 401,
            "{path} must require Bearer auth under lan_exempt"
        );
        let auth_status = client
            .get(format!("{base}{path}"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(
            auth_status, 200,
            "{path} must succeed with admin Bearer token"
        );
    }

    // D1-01: Admin Web Console session (non-dev:) is blocked under lan_exempt.
    let msg_status = client
        .get(format!("{base}/api/v0/session/{admin_sid}/messages"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(
        msg_status, 401,
        "admin Web Console session must be rejected under lan_exempt"
    );

    // D1-01: /api/v0/voice/log hides loopback-protected speaker rows under lan_exempt.
    let vlog: Value = client
        .get(format!("{base}/api/v0/voice/log"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let vlog_text = vlog.to_string();
    assert!(
        !vlog_text.contains("dot2 secret wake"),
        "loopback voice log leaked under lan_exempt: {vlog_text}"
    );
    assert!(
        vlog_text.contains("lan wake ok"),
        "non-loopback LAN voice log should remain visible: {vlog_text}"
    );

    // D1-01: /api/v0/devices hides loopback-protected speaker rows from voice_log under lan_exempt.
    let devs: Value = client
        .get(format!("{base}/api/v0/devices"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let devs_vlog = devs["voice_log"].to_string();
    assert!(
        !devs_vlog.contains("dot2 secret wake"),
        "/api/v0/devices voice_log leaked loopback entry under lan_exempt: {devs_vlog}"
    );
    assert!(
        devs_vlog.contains("lan wake ok"),
        "/api/v0/devices voice_log should keep LAN entry: {devs_vlog}"
    );

    // D1-01: /api/v0/reminders filters loopback-protected reminders under lan_exempt,
    // while authenticated admin Bearer token sees both.
    for (dev, note) in [
        ("dot2-local", "dot2 private reminder"),
        ("android-lan-2", "lan shared reminder"),
    ] {
        let r = client
            .post(format!("{base}/api/v0/reminders"))
            .bearer_auth(&token)
            .json(&json!({ "note": note, "device": dev, "delay_minutes": 30 }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "seeding reminder for {dev} must succeed");
    }
    let rem_lan: Value = client
        .get(format!("{base}/api/v0/reminders"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rem_lan_text = rem_lan.to_string();
    assert!(
        !rem_lan_text.contains("dot2 private reminder"),
        "loopback reminder leaked under lan_exempt: {rem_lan_text}"
    );
    assert!(
        rem_lan_text.contains("lan shared reminder"),
        "LAN reminder should remain visible under lan_exempt: {rem_lan_text}"
    );
    let rem_admin: Value = client
        .get(format!("{base}/api/v0/reminders"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rem_admin_text = rem_admin.to_string();
    assert!(
        rem_admin_text.contains("dot2 private reminder")
            && rem_admin_text.contains("lan shared reminder"),
        "authenticated admin must see both reminders: {rem_admin_text}"
    );

    // D1-02: Oversized Hello (> 8 devices) is rejected with `too_many_devices`.
    let gw_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = gw_listener.local_addr().unwrap();
    let hub_clone = state.hub.clone();
    let srv_handle = tokio::spawn(async move {
        let (stream, _) = gw_listener.accept().await.unwrap();
        hub_clone
            .serve_connection_from(stream, Some("192.168.10.99:55222".parse().unwrap()))
            .await
    });
    let client_stream = TcpStream::connect(gw_addr).await.unwrap();
    let (rd, mut wr) = client_stream.into_split();
    let many_devices: Vec<Value> = (0..12)
        .map(|i| {
            json!({
                "stable_id": format!("spam-dev-{i}"),
                "model": "fake",
                "capabilities": ["audio_in"],
                "audio_profile": "fake",
            })
        })
        .collect();
    let hello_line = json!({
        "type": "hello",
        "protocol": 2,
        "min_protocol": 0,
        "supported_versions": [0, 2],
        "gateway": "spam-gw",
        "gateway_version": "0.0.1",
        "devices": many_devices,
    })
    .to_string();
    wr.write_all(format!("{hello_line}\n").as_bytes())
        .await
        .unwrap();
    let mut rd_lines = BufReader::new(rd).lines();
    let reply = rd_lines.next_line().await.unwrap().unwrap();
    assert!(
        reply.contains("too_many_devices"),
        "expected too_many_devices HelloReject, got: {reply}"
    );
    assert!(srv_handle.await.unwrap().is_err());

    // D1-03: Configure OpenAPI Destructive + Vault-credentialed Query ops and credentialed OpenSocket.
    state
        .config
        .save_openapi(
            OpenApiConfig {
                enabled: true,
                base_url: "http://127.0.0.1:9".into(),
                spec_url: "http://127.0.0.1:9/openapi.json".into(),
                operations: vec![
                    OpenApiOperation {
                        id: "nuke_everything".into(),
                        method: "delete".into(),
                        path: "/everything".into(),
                        summary: "destructive op".into(),
                        params: vec![],
                        effect_level: EffectLevel::Destructive,
                        verification: None,
                    },
                    OpenApiOperation {
                        id: "read_private_vault".into(),
                        method: "get".into(),
                        path: "/private".into(),
                        summary: "vault query op".into(),
                        params: vec![],
                        effect_level: EffectLevel::Query,
                        verification: None,
                    },
                ],
                ..Default::default()
            },
            Some("secret-vault-token"),
            "test",
        )
        .unwrap();
    state
        .open_sockets
        .register(
            &state.registry,
            OpenSocketContract {
                name: "custom.private_query".into(),
                service: "open_socket".into(),
                description: "Vault-authenticated query socket".into(),
                parameters: json!({ "type": "object", "properties": {} }),
                effect: SideEffect::Query,
                endpoint_url: "http://127.0.0.1:9/private".into(),
                method: HttpMethod::GET,
                enabled: true,
                timeout_ms: 5_000,
                auth_header: "Authorization".into(),
                default_subject: "私有查询".into(),
                default_action: "query".into(),
            },
            Some("Bearer socket-secret"),
            "test",
        )
        .unwrap();

    // D1-03: lan_exempt blocks Memory / WorkingMemory / UserProfile /
    // OpenAPI Destructive / Vault-credentialed Query / credentialed OpenSocket
    // (even when the authority carries a management principal).
    let lan_auth = Authority::anonymous(state.store.clone(), Some("lan-requester".into()))
        .unwrap()
        .with_lan_exempt(true);
    let lan_ctx =
        ToolCtx::new("t-lan", "c-lan", Arc::new(AtomicBool::new(false))).with_authority(lan_auth);
    for (tool, args) in [
        ("memory.remember", json!({"text": "secret", "kind": "fact"})),
        ("workmem.set", json!({"key_or_topic": "k", "content": "v"})),
        ("workmem.get", json!({})),
        ("workmem.delete", json!({"key_or_topic": "k"})),
        (
            "user_profile.propose",
            json!({"key": "k", "content": "v", "rationale": "r"}),
        ),
        (
            "openapi.call",
            json!({"operation": "nuke_everything", "arguments": {}}),
        ),
        (
            "openapi.call",
            json!({"operation": "read_private_vault", "arguments": {}}),
        ),
        ("custom.private_query", json!({})),
    ] {
        let res = state.registry.call(tool, args, &lan_ctx).await;
        assert_eq!(
            res.status,
            ToolStatus::Failed,
            "{tool} must fail under lan_exempt"
        );
        assert_eq!(
            res.error.as_ref().map(|e| e.code.as_str()),
            Some("PAIRING_REQUIRED"),
            "{tool} wrong error: {:?}",
            res.error
        );
    }

    // D1-02: Disconnect prunes offline non-reserved LAN DeviceSlot while keeping
    // loopback-reserved speaker slot as `online: false`.
    lan_gw.disconnect();
    loopback_gw.disconnect();
    tokio::time::sleep(Duration::from_millis(80)).await;
    let snap = state.hub.snapshot();
    let devs_arr = snap["devices"].as_array().expect("snapshot devices array");
    assert!(
        !devs_arr.iter().any(|d| d["stable_id"] == "android-lan-2"),
        "offline non-reserved LAN device must be pruned on disconnect: {snap}"
    );
    let dot2_entry = devs_arr
        .iter()
        .find(|d| d["stable_id"] == "dot2-local")
        .expect("loopback-reserved speaker must remain in snapshot after disconnect");
    assert_eq!(dot2_entry["online"], json!(false));

    // D1-02: DeviceHub::listen hard-caps concurrent gateway sockets at 32.
    let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listen_addr = probe.local_addr().unwrap();
    drop(probe);
    let cap_hub = state.hub.clone();
    let listen_task = tokio::spawn(async move {
        let _ = cap_hub.listen(listen_addr).await;
    });
    tokio::time::sleep(Duration::from_millis(40)).await;
    let mut held_sockets = Vec::with_capacity(32);
    for _ in 0..32 {
        held_sockets.push(TcpStream::connect(listen_addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
    let mut extra = TcpStream::connect(listen_addr).await.unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_millis(500), extra.read(&mut buf))
        .await
        .expect("33rd gateway connection must be dropped promptly")
        .unwrap();
    assert_eq!(n, 0, "33rd gateway connection must observe immediate EOF");
    drop(held_sockets);
    listen_task.abort();
}
