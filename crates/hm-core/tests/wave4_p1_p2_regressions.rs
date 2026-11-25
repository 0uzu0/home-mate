//! Wave 4 Core P1/P2 Isolation & Regression Suite (`#282` / `MG-P2-05`):
//! 1. `MG-P1-01`: Per-device `VoiceSettings` overrides (`device_overrides`) with real
//!    runtime behavioral isolation across sibling devices + SQLite reload persistence.
//! 2. `MG-P1-02`: `config_delivery` v2 isolation (no sibling rollback, per-device
//!    `duplex_requested` diff, no offline `node_id` fallback, always check per-device
//!    capabilities even for a single online Android, real `core_node_id`, and
//!    `DeviceJoined` KWS reconciliation).
//! 3. `MG-P1-03`: Per-device bounded FIFO audio queues & independent VAD workers
//!    (burst + slow VAD on Device B drops 0 frames on Device A and preserves strict FIFO).
//! 4. `MG-P1-05`: `tool/learning.rs` passes `device_id` on all outbound requests and
//!    blocks sibling devices from hijacking or stopping an active learning session.
//! 5. `MG-P1-06`: Deterministic oldest-entry FIFO eviction on music & personal-media
//!    candidate caches preserves sibling in-flight candidates.
//! 6. `MG-P2-01`: Concurrent per-device diagnostic `mic_capture` runs independently
//!    without overwriting or blocking sibling captures.
//! 7. `MG-P2-02`: `HubState.volume_pending` and `volume_pending_gen` are unconditionally
//!    cleaned on `VolumeReceipt { device: Some(...) }` (zero memory leak).
//! 8. `MG-P2-03`: Idle-gated offline-KWS gateways (e.g. `android-v2`) report healthy
//!    `"ready"` (`idle_gated: true`) while stalled continuous gateways and active-session
//!    stalled idle-gated gateways still report `"stale"`.
//! 9. `MG-P2-04` (API side): Multi-device fail-closed `400 DEVICE_REQUIRED` on
//!    `/api/v0/reminders` and audible diagnostics, per-device `wifi_provisioning` on
//!    `/api/v0/status`, and deterministic sorted ordering in `ManagementRuntime::resolve_device`.

use axum::extract::State as AxumState;
use axum::http::Uri;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use hm_core::api::{router, AppState};
use hm_core::config::{ConfigService, PersonalEndpointConfig, LEARNING_KEY, LEARNING_TOKEN_SECRET};
use hm_core::config_delivery::{
    Block, Commit, Compatibility, ConfigApplyCoordinator, ConfigDelivery, Operation, Package,
    Purpose, Source as CfgSource, Target,
};
use hm_core::gateway::{
    AecState, ConfigBlock, ConfigReceiptStatus, CoreFrame, DeviceHub, DuplexState, GatewayFrame,
    HelloDevice, InputOrigin,
};
use hm_core::llm::fake::FakeLlm;
use hm_core::speech::{AsrClient, SpeechError, TtsAudio, TtsClient};
use hm_core::store::Store;
use hm_core::tool::mocks::MockInject;
use hm_core::tool::music::{MusicRuntime, Source as MusicSource, Track};
use hm_core::tool::personal_media::PersonalMediaRuntime;
use hm_core::tool::{ToolCtx, ToolStatus};
use hm_core::voice::VoicePipeline;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const DOT2_ID: &str = "dot2-d";
const ANDROID_ID: &str = "android-sm-f9760-0a8a";

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hm-wave4-p1p2-{tag}-{}-{}",
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

struct NullAsr;
#[async_trait::async_trait]
impl AsrClient for NullAsr {
    async fn transcribe(&self, _wav: &[u8]) -> Result<String, SpeechError> {
        Ok(String::new())
    }
    fn provider(&self) -> &str {
        "null-asr"
    }
}

struct MockGw {
    tx: mpsc::UnboundedSender<GatewayFrame>,
    _rx: mpsc::UnboundedReceiver<CoreFrame>,
    _srv: tokio::task::JoinHandle<()>,
    _r: tokio::task::JoinHandle<()>,
    _w: tokio::task::JoinHandle<()>,
}

impl MockGw {
    async fn connect(
        hub: Arc<DeviceHub>,
        peer: SocketAddr,
        device_id: &str,
        profile_version: Option<&str>,
        caps: Vec<&str>,
        token: Option<String>,
        auto_confirm_play: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hub_c = hub.clone();
        let srv = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let _ = hub_c.serve_connection_from(s, Some(peer)).await;
        });
        let stream = TcpStream::connect(addr).await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let is_lo = peer.ip().is_loopback();
        let hello = GatewayFrame::Hello {
            protocol: 2,
            min_protocol: 0,
            supported_versions: vec![0, 2],
            gateway: if is_lo { "dot2-gw" } else { "android-gw" }.into(),
            gateway_version: "2.4.0".into(),
            devices: vec![HelloDevice {
                stable_id: device_id.into(),
                model: if is_lo { "dot2" } else { "android" }.into(),
                firmware: Some("2.4.0".into()),
                capabilities: caps.into_iter().map(String::from).collect(),
                audio_profile: "default".into(),
                wifi_ssid: None,
            }],
            aec_tail_ms: Some(200),
            profile_version: profile_version.map(str::to_string),
            auth_token: token,
        };
        wr.write_all(format!("{}\n", serde_json::to_string(&hello).unwrap()).as_bytes())
            .await
            .unwrap();
        wr.flush().await.unwrap();

        let (tx, mut out_rx) = mpsc::unbounded_channel::<GatewayFrame>();
        let reply_tx = tx.clone();
        let dev = device_id.to_string();
        let w = tokio::spawn(async move {
            while let Some(f) = out_rx.recv().await {
                let line = format!("{}\n", serde_json::to_string(&f).unwrap());
                if wr.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                let _ = wr.flush().await;
            }
        });
        let (in_tx, mut rx) = mpsc::unbounded_channel::<CoreFrame>();
        let r = tokio::spawn(async move {
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(cf) = serde_json::from_str::<CoreFrame>(&line) {
                    if auto_confirm_play {
                        let now = hm_core::id::now_ms();
                        match &cf {
                            CoreFrame::Play { task_id, .. } => {
                                let _ = reply_tx.send(GatewayFrame::PlayReceipt {
                                    task_id: task_id.clone(),
                                    stage: "accepted".into(),
                                    detail: None,
                                    ts: now,
                                });
                                let _ = reply_tx.send(GatewayFrame::PlayReceipt {
                                    task_id: task_id.clone(),
                                    stage: "device_confirmed".into(),
                                    detail: None,
                                    ts: now,
                                });
                                let _ = reply_tx.send(GatewayFrame::PlayFinished {
                                    task_id: task_id.clone(),
                                    device: dev.clone(),
                                    played_ms: 100,
                                    consumed_ms: Some(100),
                                    progress_basis: Some("mock".into()),
                                    ts: now,
                                });
                            }
                            CoreFrame::ModeSet {
                                device,
                                duplex_requested,
                            } => {
                                let _ = reply_tx.send(GatewayFrame::ModeReport {
                                    device: device.clone(),
                                    duplex: DuplexState {
                                        capability: "full".into(),
                                        requested: duplex_requested.clone(),
                                        negotiated: duplex_requested.clone(),
                                        effective: duplex_requested.clone(),
                                    },
                                    aec: AecState {
                                        owner: "device".into(),
                                        reference: "dac".into(),
                                    },
                                });
                            }
                            CoreFrame::VolumeSet {
                                task_id,
                                device,
                                value,
                                ..
                            } => {
                                let _ = reply_tx.send(GatewayFrame::VolumeReceipt {
                                    task_id: task_id.clone(),
                                    effective: *value,
                                    unit: "linear".into(),
                                    stage: "device_confirmed".into(),
                                    device: Some(device.clone()),
                                });
                            }
                            _ => {}
                        }
                    }
                    let _ = in_tx.send(cf);
                }
            }
        });

        for _ in 0..50 {
            if hub.device_exists(device_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(hub.device_exists(device_id), "{device_id} must register");
        tokio::time::sleep(Duration::from_millis(40)).await;
        while rx.try_recv().is_ok() {}
        Self {
            tx,
            _rx: rx,
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

fn default_caps() -> Vec<&'static str> {
    vec![
        "audio_in",
        "audio_out",
        "play_stream",
        "volume",
        "remote_mute",
        "kws",
        "duplex_full",
    ]
}

async fn wait_until<F: FnMut() -> bool>(timeout_ms: u64, mut pred: F) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if pred() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn make_app(dir: &TempDir) -> (Arc<AppState>, String) {
    let store = Arc::new(Store::open(&dir.0.join("hm.db")).unwrap());
    store.allow_loopback_public_for_test();
    let state = AppState::with_overrides(
        store,
        dir.0.clone(),
        MockInject::default(),
        Some(Arc::new(FakeLlm::new(vec![]))),
        Some(Arc::new(InstantTts)),
        Some(Arc::new(NullAsr)),
        None,
    );
    let token = state.auth.provision("wave4-p1p2-admin").unwrap();
    state.hub.set_voice_gateway_open(true);
    state.voice.set_memory_extraction(false);
    state.voice.start();
    (state, token)
}

async fn spawn_http(state: Arc<AppState>) -> String {
    let peer = SocketAddr::from(([127, 0, 0, 1], 55555));
    let api = router(state).layer(axum::middleware::from_fn(
        move |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(peer));
            next.run(req).await
        },
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, api).await;
    });
    format!("http://{addr}")
}

fn auth_http(token: &str) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

fn ctx_for(task_id: &str, device: Option<&str>) -> ToolCtx {
    let mut ctx = ToolCtx::new(task_id, "call-1", Arc::new(AtomicBool::new(false)));
    ctx.device_id = device.map(str::to_string);
    ctx
}

fn keyword_frame(device: &str, keyword: &str, ts: u64, seq: u64) -> GatewayFrame {
    GatewayFrame::Keyword {
        device: device.into(),
        origin: InputOrigin::default(),
        ts,
        seq,
        keyword: keyword.into(),
        fed_ms: Some(400),
        self_heard: false,
        rms_dbfs: Some(-20.0),
        lag_ms: Some(40),
        bundle_id: None,
        kws_generation: None,
        kws_tier: None,
        wake_dur_ms: None,
        max_gap_ms: None,
        l2_passed: None,
    }
}

fn audio_chunk_frame(device: &str, pcm_b64: String, ts: u64, seq: u64) -> GatewayFrame {
    GatewayFrame::AudioChunk {
        device: device.into(),
        origin: InputOrigin::default(),
        ts,
        seq,
        pcm_b64,
        format: "s16le".into(),
        sample_rate: 16_000,
        channels: 1,
        layout: "mono".into(),
        vad_speech: None,
        vad_prob: None,
    }
}

fn make_v2_pkg(
    household_id: &str,
    purpose: Purpose,
    source_node: &str,
    target_node: Option<&str>,
    blocks: Vec<Block>,
) -> Package {
    Package {
        format: "home-mate-config".into(),
        version: 2,
        package_id: "00000000-0000-4000-8000-000000000099".into(),
        package_revision: 1,
        created_ms: 1,
        purpose,
        source: CfgSource {
            node_id: source_node.into(),
            config_revision: 0,
            core_version: "0.1.0".into(),
            schema_version: 2,
        },
        target: Target {
            household_id: household_id.into(),
            node_id: target_node.map(str::to_string),
        },
        compatibility: Compatibility {
            core_api_schema: 2,
            gateway_protocol_min: 0,
            gateway_protocol_max: 2,
            required_capabilities: vec!["config_apply_v2".into()],
        },
        change_reason: "wave4 test".into(),
        blocks,
        bindings: vec![],
        content_hash: None,
    }
}

async fn connect_raw_v2(
    hub: &Arc<DeviceHub>,
    peer: SocketAddr,
    device_id: &str,
    caps: Vec<&str>,
    token: Option<String>,
) -> (
    tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    tokio::net::tcp::OwnedWriteHalf,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hub_c = hub.clone();
    tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let _ = hub_c.serve_connection_from(s, Some(peer)).await;
    });
    let stream = TcpStream::connect(addr).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let hello = GatewayFrame::Hello {
        protocol: 2,
        min_protocol: 0,
        supported_versions: vec![0, 2],
        gateway: format!("gw-{device_id}"),
        gateway_version: "2.4.0".into(),
        devices: vec![HelloDevice {
            stable_id: device_id.into(),
            model: "test".into(),
            firmware: Some("2.4.0".into()),
            capabilities: caps.into_iter().map(String::from).collect(),
            audio_profile: "default".into(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
        auth_token: token,
    };
    wr.write_all(format!("{}\n", serde_json::to_string(&hello).unwrap()).as_bytes())
        .await
        .unwrap();
    wr.flush().await.unwrap();
    let _ack = lines.next_line().await.unwrap().unwrap();
    let _time = lines.next_line().await.unwrap().unwrap();
    for _ in 0..50 {
        if hub.device_exists(device_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(hub.device_exists(device_id));
    (lines, wr)
}

async fn send_raw_gw(wr: &mut tokio::net::tcp::OwnedWriteHalf, frame: &GatewayFrame) {
    wr.write_all(format!("{}\n", serde_json::to_string(frame).unwrap()).as_bytes())
        .await
        .unwrap();
    wr.flush().await.unwrap();
}

/// 1. `MG-P1-01`: Per-device overrides in `VoiceSettings` isolate runtime behavior
/// between `dot2-d` and `android-2` with zero cross-device bleed and survive SQLite reload.
#[tokio::test]
async fn mg_p1_01_per_device_voice_settings_isolation() {
    let dir = TempDir::new("mg_p1_01");
    let (state, token) = make_app(&dir);
    let base = spawn_http(state.clone()).await;
    let http = auth_http(&token);

    let gw_dot2 = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([127, 0, 0, 1], 41001)),
        DOT2_ID,
        None,
        default_caps(),
        None,
        true,
    )
    .await;
    let gw_android = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([192, 168, 5, 42], 41002)),
        ANDROID_ID,
        Some("android-v2"),
        default_caps(),
        Some(token.clone()),
        true,
    )
    .await;

    // Seed global defaults via PUT /api/v0/mode so both devices start from known defaults.
    let global_put = http
        .put(format!("{base}/api/v0/mode"))
        .json(&json!({
            "tail_blind_ms": 300,
            "kws_threshold": 0.25,
            "kws_score": 3.0,
            "barge_in": true,
            "barge_protect_ms": 400,
            "keyword_interrupt": true,
            "proactive_speech": true,
            "follow_window_enabled": true,
            "follow_window_ms": 9000,
            "context_window_ms": 180000,
            "followup_credits": 2,
            "follow_window_tone": true,
            "wake_words": ["你好高达"],
            "wake_ack": { "enabled": true, "phrases": ["在呢"] },
            "retreat_intent": true,
            "retreat_words": ["退下"],
            "proactive_asr_wake": false,
            "wake_arbitration": "off"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(global_put.status(), 200);

    // Override ONLY android-2 via PUT /api/v0/mode/{dev}.
    let dev_put = http
        .put(format!("{base}/api/v0/mode/{ANDROID_ID}"))
        .json(&json!({
            "tail_blind_ms": 950,
            "kws_threshold": 0.42,
            "kws_score": 4.2,
            "barge_in": false,
            "barge_protect_ms": 1500,
            "keyword_interrupt": false,
            "proactive_speech": false,
            "follow_window_enabled": true,
            "follow_window_ms": 15000,
            "context_window_ms": 240000,
            "followup_credits": 5,
            "follow_window_tone": false,
            "wake_words": ["你好小艺"],
            "wake_ack": { "enabled": false, "phrases": ["我在"] },
            "retreat_intent": false,
            "retreat_words": ["先退吧"],
            "proactive_asr_wake": true,
            "wake_arbitration": "observe"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(dev_put.status(), 200);

    // GET /api/v0/mode?device=... returns each device's independent effective settings.
    let android_mode: Value = http
        .get(format!("{base}/api/v0/mode?device={ANDROID_ID}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(android_mode["scope"], "device");
    assert_eq!(android_mode["has_device_override"], true);
    assert_eq!(android_mode["tail_blind_ms"], 950);
    assert_eq!(android_mode["kws_threshold"], 0.42);
    assert_eq!(android_mode["kws_score"], 4.2);
    assert_eq!(android_mode["barge_in"], false);
    assert_eq!(android_mode["barge_protect_ms"], 1500);
    assert_eq!(android_mode["keyword_interrupt"], false);
    assert_eq!(android_mode["proactive_speech"], false);
    assert_eq!(android_mode["follow_window_ms"], 15000);
    assert_eq!(android_mode["context_window_ms"], 240000);
    assert_eq!(android_mode["followup_credits"], 5);
    assert_eq!(android_mode["follow_window_tone"], false);
    assert_eq!(android_mode["wake_words"], json!(["你好小艺"]));
    assert_eq!(android_mode["wake_ack"]["enabled"], false);
    assert_eq!(android_mode["retreat_intent"], false);
    assert_eq!(android_mode["retreat_words"], json!(["先退吧"]));
    assert_eq!(android_mode["proactive_asr_wake"], true);
    assert_eq!(android_mode["wake_arbitration"]["mode"], "observe");

    let dot2_mode: Value = http
        .get(format!("{base}/api/v0/mode?device={DOT2_ID}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dot2_mode["has_device_override"], false);
    assert_eq!(dot2_mode["tail_blind_ms"], 300);
    assert_eq!(dot2_mode["kws_threshold"], 0.25);
    assert_eq!(dot2_mode["barge_in"], true);
    assert_eq!(dot2_mode["keyword_interrupt"], true);
    assert_eq!(dot2_mode["proactive_speech"], true);
    assert_eq!(dot2_mode["follow_window_ms"], 9000);
    assert_eq!(dot2_mode["followup_credits"], 2);
    assert_eq!(dot2_mode["wake_words"], json!(["你好高达"]));
    assert_eq!(dot2_mode["wake_ack"]["enabled"], true);
    assert_eq!(dot2_mode["retreat_intent"], true);
    assert_eq!(dot2_mode["proactive_asr_wake"], false);

    // Behavioral check 1: `proactive_speech` blocks android-2 announce (409) but admits dot2-d (202).
    let ann_android = http
        .post(format!("{base}/api/v0/voice/announce"))
        .json(&json!({ "device": ANDROID_ID, "text": "洗衣机好了", "source": "test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ann_android.status(), 409);
    let ann_dot2 = http
        .post(format!("{base}/api/v0/voice/announce"))
        .json(&json!({ "device": DOT2_ID, "text": "洗衣机好了", "source": "test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ann_dot2.status(), 202);

    // Behavioral check 2: `keyword_interrupt=false` ignores keyword on android-2,
    // while dot2-d (`keyword_interrupt=true`) wakes on `"你好高达"`.
    let now = hm_core::id::now_ms();
    gw_android.send(keyword_frame(ANDROID_ID, "你好小艺", now, 1));
    gw_dot2.send(keyword_frame(DOT2_ID, "你好高达", now, 1));
    assert!(
        wait_until(2_000, || {
            let ignored = state.voice.recent(50).iter().any(|e| {
                e["kind"] == "keyword_ignored"
                    && e["device"] == ANDROID_ID
                    && e["reason"] == "disabled"
            });
            let dot2_open = state.voice.mode_snapshot()["devices"]
                .as_array()
                .is_some_and(|ds| {
                    ds.iter()
                        .any(|d| d["device"] == DOT2_ID && d["participation"] == "conversation")
                });
            ignored && dot2_open
        })
        .await,
        "android-2 must ignore keyword when its per-device keyword_interrupt=false and dot2-d must wake: {:?}",
        state.voice.recent(50)
    );
    let snap_after_wake = state.voice.mode_snapshot();
    let dot2_entry = snap_after_wake["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == DOT2_ID)
        .unwrap();
    assert_eq!(dot2_entry["participation"], "conversation");
    assert_eq!(dot2_entry["follow_window_ms"], 9000);
    assert_eq!(dot2_entry["session"]["credits"], 2);

    // Behavioral check 3: enable `keyword_interrupt=true` ONLY on android-2, then verify
    // android-2 rejects global `"你好高达"` (`not_in_reflex_table`) and wakes on `"你好小艺"`
    // with its own `follow_window_ms = 15000` & `credits = 5`.
    assert_eq!(
        http.put(format!("{base}/api/v0/mode/{ANDROID_ID}"))
            .json(&json!({ "keyword_interrupt": true }))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    gw_android.send(keyword_frame(ANDROID_ID, "你好高达", now + 500, 2));
    assert!(
        wait_until(2_000, || {
            state.voice.recent(50).iter().any(|e| {
                e["kind"] == "keyword_ignored"
                    && e["device"] == ANDROID_ID
                    && e["keyword"] == "你好高达"
                    && e["reason"] == "not_in_reflex_table"
            })
        })
        .await,
        "android-2 with custom wake_words must not wake on global wake word"
    );
    gw_android.send(keyword_frame(ANDROID_ID, "你好小艺", now + 800, 3));
    assert!(
        wait_until(2_000, || {
            state.voice.mode_snapshot()["devices"]
                .as_array()
                .is_some_and(|ds| {
                    ds.iter()
                        .any(|d| d["device"] == ANDROID_ID && d["participation"] == "conversation")
                })
        })
        .await,
        "android-2 must wake on its per-device wake word"
    );
    let snap2 = state.voice.mode_snapshot();
    let android_entry = snap2["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == ANDROID_ID)
        .unwrap();
    assert_eq!(android_entry["participation"], "conversation");
    assert_eq!(android_entry["follow_window_ms"], 15000);
    assert_eq!(android_entry["session"]["credits"], 5);

    // Behavioral check 3b: legacy `deploy_reflex` (non-v2 gateways) also honors
    // per-device `device_overrides[device].wake_words` without bleeding across units!
    let legacy_and = state.voice.deploy_reflex(ANDROID_ID).unwrap();
    let legacy_dot = state.voice.deploy_reflex(DOT2_ID).unwrap();
    let and_kw_file = legacy_and["keywords_file"].as_str().unwrap();
    let dot_kw_file = legacy_dot["keywords_file"].as_str().unwrap();
    assert!(
        and_kw_file.contains("你好小艺") && !and_kw_file.contains("你好高达"),
        "legacy deploy_reflex for android-2 must deploy per-device wake word: {and_kw_file}"
    );
    assert!(
        dot_kw_file.contains("你好高达") && !dot_kw_file.contains("你好小艺"),
        "legacy deploy_reflex for dot2-d must keep node default wake word: {dot_kw_file}"
    );

    // Behavioral check 4: `barge_in=false` on android-2 ignores BargeIn event with reason="disabled".
    gw_android.send(GatewayFrame::ModeReport {
        device: ANDROID_ID.into(),
        duplex: DuplexState {
            capability: "full".into(),
            requested: "full".into(),
            negotiated: "full".into(),
            effective: "full".into(),
        },
        aec: AecState {
            owner: "device".into(),
            reference: "dac".into(),
        },
    });
    assert!(
        wait_until(2_000, || {
            state.voice.mode_snapshot()["devices"]
                .as_array()
                .is_some_and(|ds| {
                    ds.iter()
                        .any(|d| d["device"] == ANDROID_ID && d["duplex"]["effective"] == "full")
                })
        })
        .await,
        "android-2 ModeReport(full) should be observed"
    );
    gw_android.send(GatewayFrame::BargeIn {
        device: ANDROID_ID.into(),
        ts: now + 950,
        seq: 4,
        pcm_b64: base64::engine::general_purpose::STANDARD.encode(vec![0u8; 640]),
        sample_rate: 16_000,
        channels: 1,
        corr: 0.1,
        energy_db: -18.0,
        ref_db: -35.0,
        residual_db: -20.0,
        ref_locked: true,
    });
    assert!(
        wait_until(2_000, || {
            state.voice.recent(50).iter().any(|e| {
                e["kind"] == "barge_in_ignored"
                    && e["device"] == ANDROID_ID
                    && e["reason"] == "disabled"
            })
        })
        .await,
        "android-2 must honor per-device barge_in=false"
    );

    // Behavioral check 5: SQLite reload persistence! Reconstruct fresh VoicePipeline from same DB.
    let reloaded = VoicePipeline::new(
        DeviceHub::new(),
        state.store.clone(),
        state.tasks.clone(),
        Arc::new(|| Err("unused".into())),
        Arc::new(|| Ok(Arc::new(InstantTts))),
        Arc::new(|| Ok(Arc::new(NullAsr))),
        dir.0.clone(),
    );
    let reloaded_settings = reloaded.settings_snapshot();
    let re_android = reloaded_settings.effective_for(ANDROID_ID);
    let re_dot2 = reloaded_settings.effective_for(DOT2_ID);
    assert_eq!(re_android.tail_blind_ms, 950);
    assert_eq!(re_android.kws_threshold, 0.42);
    assert!(!re_android.barge_in);
    assert!(!re_android.proactive_speech);
    assert!(!re_android.wake_ack.enabled);
    assert_eq!(re_android.follow_window_ms, 15000);
    assert_eq!(re_android.wake_words, vec!["你好小艺".to_string()]);
    assert_eq!(re_dot2.tail_blind_ms, 300);
    assert_eq!(re_dot2.kws_threshold, 0.25);
    assert!(re_dot2.barge_in);
    assert!(re_dot2.proactive_speech);
    assert!(re_dot2.wake_ack.enabled);
    assert_eq!(re_dot2.follow_window_ms, 9000);
    assert_eq!(re_dot2.wake_words, vec!["你好高达".to_string()]);

    gw_dot2.disconnect();
    gw_android.disconnect();
}

/// 2. `MG-P1-02`: All 5 `config_delivery` v2 defects fixed + `DeviceJoined` KWS reconciliation.
#[tokio::test]
async fn mg_p1_02_config_delivery_isolates_failures_duplex_offline_and_caps() {
    let dir = TempDir::new("mg_p1_02");
    let (state, token) = make_app(&dir);
    let delivery = ConfigDelivery::for_app(&state);
    let enrolled = delivery.enroll(DOT2_ID).unwrap();
    let hh = enrolled["household_id"].as_str().unwrap().to_string();

    let (mut lines_dot2, mut wr_dot2) = connect_raw_v2(
        &state.hub,
        SocketAddr::from(([127, 0, 0, 1], 42001)),
        DOT2_ID,
        vec![
            "audio_out",
            "config_apply_v2",
            "config_block_kws",
            "config_block_duplex",
            "config_block_audio_eq",
            "config_block_led",
            "config_block_acoustics",
        ],
        None,
    )
    .await;
    let (mut lines_and, mut wr_and) = connect_raw_v2(
        &state.hub,
        SocketAddr::from(([192, 168, 5, 50], 42002)),
        "android-b",
        vec![
            "audio_out",
            "config_apply_v2",
            "config_block_kws",
            "config_block_duplex",
        ],
        Some(token.clone()),
    )
    .await;
    let _worker = ConfigApplyCoordinator::start_for_app(&state);

    let dot2_frames = Arc::new(Mutex::new(Vec::<(ConfigBlock, String, Value)>::new()));
    let and_frames = Arc::new(Mutex::new(Vec::<(ConfigBlock, String, Value)>::new()));
    let fail_and_once = Arc::new(AtomicBool::new(true));

    let dot2_frames_t = dot2_frames.clone();
    let gw_dot2 = tokio::spawn(async move {
        let mut rev = 1u64;
        while let Ok(Some(line)) = lines_dot2.next_line().await {
            let Ok(frame) = serde_json::from_str::<CoreFrame>(&line) else {
                continue;
            };
            match frame {
                CoreFrame::ConfigStateGet { request_id, device } => {
                    send_raw_gw(
                        &mut wr_dot2,
                        &GatewayFrame::ConfigState {
                            request_id: Some(request_id),
                            device,
                            gateway_boot_id: Some("boot-dot2".into()),
                            state_revision: Some(rev),
                            capability_fingerprint: Some("fp-dot2".into()),
                            blocks: vec![],
                            ts: 100,
                        },
                    )
                    .await;
                }
                CoreFrame::ConfigValidate {
                    request_id,
                    device,
                    block,
                    schema_version,
                    block_hash,
                    ..
                } => {
                    send_raw_gw(
                        &mut wr_dot2,
                        &GatewayFrame::ConfigValidated {
                            request_id,
                            device,
                            block,
                            schema_version,
                            block_hash,
                            ok: true,
                            supported: true,
                            blockers: vec![],
                            state: ConfigReceiptStatus::Validated,
                            reason: None,
                            retryable: false,
                            gateway_boot_id: Some("boot-dot2".into()),
                            state_revision: Some(rev),
                            capability_fingerprint: None,
                            evidence: None,
                            ts: 101,
                        },
                    )
                    .await;
                }
                CoreFrame::ConfigApply {
                    device,
                    core_node_id,
                    data_epoch,
                    application_id,
                    operation_id,
                    desired_revision,
                    block_generation,
                    block,
                    schema_version,
                    block_hash,
                    values,
                    ..
                } => {
                    dot2_frames_t.lock().unwrap().push((
                        block,
                        core_node_id.clone(),
                        values.clone(),
                    ));
                    rev += 1;
                    for st in [ConfigReceiptStatus::Persisted, ConfigReceiptStatus::Active] {
                        send_raw_gw(
                            &mut wr_dot2,
                            &GatewayFrame::ConfigReceipt {
                                device: device.clone(),
                                core_node_id: core_node_id.clone(),
                                data_epoch,
                                application_id: application_id.clone(),
                                operation_id: operation_id.clone(),
                                desired_revision,
                                block_generation,
                                block,
                                schema_version,
                                block_hash: block_hash.clone(),
                                state: st,
                                reason: None,
                                retryable: false,
                                gateway_boot_id: Some("boot-dot2".into()),
                                state_revision: Some(rev),
                                persisted_hash: Some(block_hash.clone()),
                                active_hash: (st == ConfigReceiptStatus::Active)
                                    .then(|| block_hash.clone()),
                                active_values: (st == ConfigReceiptStatus::Active)
                                    .then(|| values.clone()),
                                evidence: None,
                                ts: 102,
                            },
                        )
                        .await;
                    }
                }
                _ => {}
            }
        }
    });

    let and_frames_t = and_frames.clone();
    let fail_and_t = fail_and_once.clone();
    let gw_and = tokio::spawn(async move {
        let mut rev = 1u64;
        while let Ok(Some(line)) = lines_and.next_line().await {
            let Ok(frame) = serde_json::from_str::<CoreFrame>(&line) else {
                continue;
            };
            match frame {
                CoreFrame::ConfigStateGet { request_id, device } => {
                    send_raw_gw(
                        &mut wr_and,
                        &GatewayFrame::ConfigState {
                            request_id: Some(request_id),
                            device,
                            gateway_boot_id: Some("boot-and".into()),
                            state_revision: Some(rev),
                            capability_fingerprint: Some("fp-and".into()),
                            blocks: vec![],
                            ts: 100,
                        },
                    )
                    .await;
                }
                CoreFrame::ConfigValidate {
                    request_id,
                    device,
                    block,
                    schema_version,
                    block_hash,
                    ..
                } => {
                    send_raw_gw(
                        &mut wr_and,
                        &GatewayFrame::ConfigValidated {
                            request_id,
                            device,
                            block,
                            schema_version,
                            block_hash,
                            ok: true,
                            supported: true,
                            blockers: vec![],
                            state: ConfigReceiptStatus::Validated,
                            reason: None,
                            retryable: false,
                            gateway_boot_id: Some("boot-and".into()),
                            state_revision: Some(rev),
                            capability_fingerprint: None,
                            evidence: None,
                            ts: 101,
                        },
                    )
                    .await;
                }
                CoreFrame::ConfigApply {
                    device,
                    core_node_id,
                    data_epoch,
                    application_id,
                    operation_id,
                    desired_revision,
                    block_generation,
                    block,
                    schema_version,
                    block_hash,
                    values,
                    ..
                } => {
                    and_frames_t.lock().unwrap().push((
                        block,
                        core_node_id.clone(),
                        values.clone(),
                    ));
                    if fail_and_t.swap(false, Ordering::SeqCst) {
                        send_raw_gw(
                            &mut wr_and,
                            &GatewayFrame::ConfigReceipt {
                                device,
                                core_node_id,
                                data_epoch,
                                application_id,
                                operation_id,
                                desired_revision,
                                block_generation,
                                block,
                                schema_version,
                                block_hash,
                                state: ConfigReceiptStatus::Failed,
                                reason: Some("simulated_android_fail".into()),
                                retryable: false,
                                gateway_boot_id: Some("boot-and".into()),
                                state_revision: Some(rev),
                                persisted_hash: None,
                                active_hash: None,
                                active_values: None,
                                evidence: None,
                                ts: 102,
                            },
                        )
                        .await;
                    } else {
                        rev += 1;
                        for st in [ConfigReceiptStatus::Persisted, ConfigReceiptStatus::Active] {
                            send_raw_gw(
                                &mut wr_and,
                                &GatewayFrame::ConfigReceipt {
                                    device: device.clone(),
                                    core_node_id: core_node_id.clone(),
                                    data_epoch,
                                    application_id: application_id.clone(),
                                    operation_id: operation_id.clone(),
                                    desired_revision,
                                    block_generation,
                                    block,
                                    schema_version,
                                    block_hash: block_hash.clone(),
                                    state: st,
                                    reason: None,
                                    retryable: false,
                                    gateway_boot_id: Some("boot-and".into()),
                                    state_revision: Some(rev),
                                    persisted_hash: Some(block_hash.clone()),
                                    active_hash: (st == ConfigReceiptStatus::Active)
                                        .then(|| block_hash.clone()),
                                    active_values: (st == ConfigReceiptStatus::Active)
                                        .then(|| values.clone()),
                                    evidence: None,
                                    ts: 102,
                                },
                            )
                            .await;
                        }
                    }
                }
                _ => {}
            }
        }
    });

    let commit_and_wait = |app_id: &str, blocks: Vec<Block>| {
        let pkg = make_v2_pkg(&hh, Purpose::NodeConfig, DOT2_ID, Some(DOT2_ID), blocks);
        let pre = delivery.preflight("admin", pkg, json!({})).unwrap();
        delivery
            .commit(
                "admin",
                &Commit {
                    plan_id: pre["plan_id"].as_str().unwrap().into(),
                    plan_hash: pre["plan_hash"].as_str().unwrap().into(),
                    expected_revision: pre["expected_revision"].as_u64().unwrap(),
                    application_id: app_id.into(),
                },
            )
            .unwrap();
    };
    let wait_terminal = |app_id: String| {
        let delivery = ConfigDelivery::for_app(&state);
        async move {
            for _ in 0..100 {
                let rb = delivery.readback("admin", &app_id).unwrap();
                let st = rb["publication"]["state"].as_str().unwrap_or("");
                if st == "applied" || st == "failed" {
                    return rb;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            delivery.readback("admin", &app_id).unwrap()
        }
    };

    // Fix 2 (run while baseline is clean): updating duplex_requested for dot2-d ONLY dispatches
    // Duplex to dot2-d, NOT unrelated sibling android-b!
    commit_and_wait(
        "44444444-4444-4444-8444-444444444400",
        vec![Block {
            name: "mode".into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: format!("mode.duplex_requested.{DOT2_ID}"),
                value: json!("full"),
            }],
        }],
    );
    let rb2 = wait_terminal("44444444-4444-4444-8444-444444444400".into()).await;
    assert_eq!(rb2["publication"]["state"], "applied", "{rb2}");
    {
        let df = dot2_frames.lock().unwrap();
        assert_eq!(df.len(), 1, "dot2-d should receive only Duplex: {df:?}");
        assert_eq!(df[0].0, ConfigBlock::Duplex);
        assert_eq!(df[0].2["duplex_requested"], "full");
        assert_eq!(df[0].1, DOT2_ID);
        let af = and_frames.lock().unwrap();
        assert!(
            af.is_empty(),
            "per-device duplex_requested change for dot2-d must NOT fan out to sibling android-b: {af:?}"
        );
    }

    // Fix 1 & Fix 5: android-b fails KWS apply -> healthy sibling dot2-d is NEVER rolled back,
    // AND every ConfigApply carries real `core_node_id == DOT2_ID` (NOT target device "android-b").
    dot2_frames.lock().unwrap().clear();
    and_frames.lock().unwrap().clear();
    commit_and_wait(
        "44444444-4444-4444-8444-444444444401",
        vec![Block {
            name: "mode".into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: "mode.kws_threshold".into(),
                value: json!(0.31),
            }],
        }],
    );
    let rb1 = wait_terminal("44444444-4444-4444-8444-444444444401".into()).await;
    assert_eq!(rb1["publication"]["state"], "failed", "{rb1}");
    {
        let df = dot2_frames.lock().unwrap();
        assert_eq!(
            df.len(),
            1,
            "healthy dot2-d must receive ONLY the forward apply, never a sibling rollback: {df:?}"
        );
        assert_eq!(df[0].0, ConfigBlock::Kws);
        assert_eq!(df[0].1, DOT2_ID, "core_node_id must be real Core node ID");
        let af = and_frames.lock().unwrap();
        assert_eq!(af.len(), 1);
        assert_eq!(
            af[0].1, DOT2_ID,
            "android-b ConfigApply must carry core_node_id == enrolled Core node ID, NOT 'android-b'"
        );
    }

    // Disconnect dot2-d so ONLY android-b (1 device, lacking audio_eq/led) is online.
    gw_dot2.abort();
    for _ in 0..50 {
        if state.hub.online_device_count() == 1
            && !state.hub.device_online_with_cap(DOT2_ID, "config_apply_v2")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!state.hub.device_online_with_cap(DOT2_ID, "config_apply_v2"));
    assert_eq!(state.hub.online_device_count(), 1);

    // Fix 3: modifying LED config for offline dot2-d records skipped_offline (never fails
    // or pushes to unrelated online android-b).
    and_frames.lock().unwrap().clear();
    commit_and_wait(
        "44444444-4444-4444-8444-444444444403",
        vec![Block {
            name: "device_led_configs".into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: format!("device_led_configs.{DOT2_ID}.night_enabled"),
                value: json!(true),
            }],
        }],
    );
    let rb3 = wait_terminal("44444444-4444-4444-8444-444444444403".into()).await;
    assert_eq!(rb3["publication"]["state"], "applied", "{rb3}");
    assert!(
        !and_frames
            .lock()
            .unwrap()
            .iter()
            .any(|(b, _, _)| *b == ConfigBlock::Led),
        "offline dot2-d LED update must NEVER be sent to android-b: frames={:?} rb3={rb3}",
        *and_frames.lock().unwrap()
    );
    let led_pub = rb3["publications"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["block"] == "device_led_configs")
        .unwrap_or_else(|| panic!("missing device_led_configs publication in {rb3}"));
    assert!(
        led_pub["gateway_receipt"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["device"] == DOT2_ID && r["state"] == "skipped_offline"),
        "{led_pub}"
    );

    // Fix 4: with ONLY 1 Android device online, `audio_eq` cleanly skips
    // unsupported block without failing the application!
    and_frames.lock().unwrap().clear();
    commit_and_wait(
        "44444444-4444-4444-8444-444444444404",
        vec![Block {
            name: "audio_eq".into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: "audio_eq.curve".into(),
                value: json!("official"),
            }],
        }],
    );
    let rb4 = wait_terminal("44444444-4444-4444-8444-444444444404".into()).await;
    assert_eq!(
        rb4["publication"]["state"], "applied",
        "single online Android without config_block_audio_eq must skip cleanly, not fail: {rb4}"
    );
    assert!(
        rb4["publication"]["gateway_receipt"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["device"] == "android-b" && r["state"] == "skipped_unsupported"),
        "{rb4}"
    );
    assert!(and_frames.lock().unwrap().is_empty());

    // DeviceJoined KWS reconciliation:
    // (a) per-device wake_words override reconciles exact keywords + populates reflex.deploy,
    //     and a subsequent DeviceStatus(online=true) does NOT re-trigger ConfigApply(Kws).
    state
        .voice
        .set_mode(
            "android-rejoin",
            None,
            None,
            Some(vec!["你好小艺".into()]),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "test",
        )
        .unwrap();
    let (mut lines_rejoin, mut wr_rejoin) = connect_raw_v2(
        &state.hub,
        SocketAddr::from(([192, 168, 5, 51], 42003)),
        "android-rejoin",
        vec!["audio_out", "config_apply_v2", "config_block_kws"],
        Some(token.clone()),
    )
    .await;
    let mut rejoin_kws_values: Option<Value> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let Ok(Ok(Some(line))) =
            tokio::time::timeout(Duration::from_millis(500), lines_rejoin.next_line()).await
        else {
            break;
        };
        let Ok(frame) = serde_json::from_str::<CoreFrame>(&line) else {
            continue;
        };
        match frame {
            CoreFrame::ConfigStateGet { request_id, device } => {
                send_raw_gw(
                    &mut wr_rejoin,
                    &GatewayFrame::ConfigState {
                        request_id: Some(request_id),
                        device,
                        gateway_boot_id: Some("boot-rejoin".into()),
                        state_revision: Some(1),
                        capability_fingerprint: Some("fp-rejoin".into()),
                        blocks: vec![],
                        ts: 200,
                    },
                )
                .await;
            }
            CoreFrame::ConfigValidate {
                request_id,
                device,
                block,
                schema_version,
                block_hash,
                ..
            } => {
                send_raw_gw(
                    &mut wr_rejoin,
                    &GatewayFrame::ConfigValidated {
                        request_id,
                        device,
                        block,
                        schema_version,
                        block_hash,
                        ok: true,
                        supported: true,
                        blockers: vec![],
                        state: ConfigReceiptStatus::Validated,
                        reason: None,
                        retryable: false,
                        gateway_boot_id: Some("boot-rejoin".into()),
                        state_revision: Some(1),
                        capability_fingerprint: None,
                        evidence: None,
                        ts: 201,
                    },
                )
                .await;
            }
            CoreFrame::ConfigApply {
                device,
                core_node_id,
                data_epoch,
                application_id,
                operation_id,
                desired_revision,
                block_generation,
                block,
                schema_version,
                block_hash,
                values,
                ..
            } if block == ConfigBlock::Kws => {
                assert_eq!(core_node_id, DOT2_ID);
                rejoin_kws_values = Some(values.clone());
                send_raw_gw(
                    &mut wr_rejoin,
                    &GatewayFrame::ConfigReceipt {
                        device,
                        core_node_id,
                        data_epoch,
                        application_id,
                        operation_id,
                        desired_revision,
                        block_generation,
                        block,
                        schema_version,
                        block_hash: block_hash.clone(),
                        state: ConfigReceiptStatus::Active,
                        reason: None,
                        retryable: false,
                        gateway_boot_id: Some("boot-rejoin".into()),
                        state_revision: Some(2),
                        persisted_hash: Some(block_hash.clone()),
                        active_hash: Some(block_hash),
                        active_values: Some(values),
                        evidence: None,
                        ts: 202,
                    },
                )
                .await;
                break;
            }
            _ => {}
        }
    }
    let rejoin_vals = rejoin_kws_values
        .expect("newly joined v2 gateway with config_block_kws must receive KWS reconciliation");
    let rejoin_kw = rejoin_vals["keywords"].as_str().unwrap_or("");
    assert!(
        rejoin_kw.contains("你好小艺") && !rejoin_kw.contains("你好高达"),
        "per-device override KWS reconciliation must carry device wake word: {rejoin_vals}"
    );
    assert!(
        wait_until(2_000, || {
            let deployed = state.voice.mode_snapshot()["devices"]
                .as_array()
                .is_some_and(|ds| {
                    ds.iter().any(|d| {
                        d["device"] == "android-rejoin"
                            && d["reflex"]["deploy"]["state"] == "applied"
                    })
                });
            let noted = state.voice.recent(50).iter().any(|e| {
                e["kind"] == "kws_deploy_sent"
                    && e["device"] == "android-rejoin"
                    && e["via"] == "config_apply_v2"
            });
            deployed && noted
        })
        .await,
        "reconnect KWS reconciliation must populate reflex.deploy and emit kws_deploy_sent: {:?}",
        state.voice.mode_snapshot()["devices"]
    );

    // A subsequent DeviceStatus(online=true) on the same live connection MUST NOT re-trigger
    // a duplicate ConfigApply(Kws) dispatch!
    send_raw_gw(
        &mut wr_rejoin,
        &GatewayFrame::DeviceStatus {
            device: "android-rejoin".into(),
            online: true,
            reason: "registered".into(),
            descriptor: None,
            northbound: None,
        },
    )
    .await;
    let dup_deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    while tokio::time::Instant::now() < dup_deadline {
        let rem = dup_deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(rem, lines_rejoin.next_line()).await {
            Ok(Ok(Some(line))) => {
                if let Ok(frame) = serde_json::from_str::<CoreFrame>(&line) {
                    assert!(
                        !matches!(
                            frame,
                            CoreFrame::ConfigStateGet { .. }
                                | CoreFrame::ConfigValidate { .. }
                                | CoreFrame::ConfigApply {
                                    block: ConfigBlock::Kws,
                                    ..
                                }
                        ),
                        "post-join DeviceStatus(online=true) must not re-trigger KWS reconciliation: {frame:?}"
                    );
                }
            }
            _ => break,
        }
    }

    // (b) Global mode.wake_words (without REFLEX_KEY and without per-device override) also
    // reconciles the updated global wake word (not default "你好高达") on DeviceJoined!
    state
        .voice
        .set_mode(
            "global",
            None,
            None,
            Some(vec!["你好管家".into()]),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "test-global",
        )
        .unwrap();
    let (mut lines_global, mut wr_global) = connect_raw_v2(
        &state.hub,
        SocketAddr::from(([192, 168, 5, 52], 42004)),
        "android-global-rejoin",
        vec!["audio_out", "config_apply_v2", "config_block_kws"],
        Some(token.clone()),
    )
    .await;
    let mut global_kws_values: Option<Value> = None;
    let deadline2 = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline2 {
        let Ok(Ok(Some(line))) =
            tokio::time::timeout(Duration::from_millis(500), lines_global.next_line()).await
        else {
            break;
        };
        let Ok(frame) = serde_json::from_str::<CoreFrame>(&line) else {
            continue;
        };
        match frame {
            CoreFrame::ConfigStateGet { request_id, device } => {
                send_raw_gw(
                    &mut wr_global,
                    &GatewayFrame::ConfigState {
                        request_id: Some(request_id),
                        device,
                        gateway_boot_id: Some("boot-global".into()),
                        state_revision: Some(1),
                        capability_fingerprint: Some("fp-global".into()),
                        blocks: vec![],
                        ts: 210,
                    },
                )
                .await;
            }
            CoreFrame::ConfigValidate {
                request_id,
                device,
                block,
                schema_version,
                block_hash,
                ..
            } => {
                send_raw_gw(
                    &mut wr_global,
                    &GatewayFrame::ConfigValidated {
                        request_id,
                        device,
                        block,
                        schema_version,
                        block_hash,
                        ok: true,
                        supported: true,
                        blockers: vec![],
                        state: ConfigReceiptStatus::Validated,
                        reason: None,
                        retryable: false,
                        gateway_boot_id: Some("boot-global".into()),
                        state_revision: Some(1),
                        capability_fingerprint: None,
                        evidence: None,
                        ts: 211,
                    },
                )
                .await;
            }
            CoreFrame::ConfigApply {
                device,
                core_node_id,
                data_epoch,
                application_id,
                operation_id,
                desired_revision,
                block_generation,
                block,
                schema_version,
                block_hash,
                values,
                ..
            } if block == ConfigBlock::Kws => {
                assert_eq!(core_node_id, DOT2_ID);
                global_kws_values = Some(values.clone());
                send_raw_gw(
                    &mut wr_global,
                    &GatewayFrame::ConfigReceipt {
                        device,
                        core_node_id,
                        data_epoch,
                        application_id,
                        operation_id,
                        desired_revision,
                        block_generation,
                        block,
                        schema_version,
                        block_hash: block_hash.clone(),
                        state: ConfigReceiptStatus::Active,
                        reason: None,
                        retryable: false,
                        gateway_boot_id: Some("boot-global".into()),
                        state_revision: Some(2),
                        persisted_hash: Some(block_hash.clone()),
                        active_hash: Some(block_hash),
                        active_values: Some(values),
                        evidence: None,
                        ts: 212,
                    },
                )
                .await;
                break;
            }
            _ => {}
        }
    }
    let global_vals = global_kws_values
        .expect("global wake_words change without REFLEX_KEY must reconcile on DeviceJoined");
    let global_kw = global_vals["keywords"].as_str().unwrap_or("");
    assert!(
        global_kw.contains("你好管家") && !global_kw.contains("你好高达"),
        "global mode.wake_words reconciliation must deploy global wake word, not default: {global_vals}"
    );
    assert!(
        wait_until(2_000, || {
            state.voice.mode_snapshot()["devices"]
                .as_array()
                .is_some_and(|ds| {
                    ds.iter().any(|d| {
                        d["device"] == "android-global-rejoin"
                            && d["reflex"]["deploy"]["state"] == "applied"
                    })
                })
        })
        .await,
        "global KWS reconciliation must populate reflex.deploy"
    );
    gw_and.abort();
}

/// 3. `MG-P1-03`: Per-device bounded FIFO audio queues & independent VAD workers
/// prevent a burst + slow VAD on Device B from dropping or reordering Device A frames.
#[tokio::test]
async fn mg_p1_03_per_device_audio_pump_no_cross_device_frame_drops() {
    let dir = TempDir::new("mg_p1_03");
    let (state, token) = make_app(&dir);

    let gw_dot2 = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([127, 0, 0, 1], 43001)),
        DOT2_ID,
        None,
        default_caps(),
        None,
        true,
    )
    .await;
    let gw_android = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([192, 168, 5, 60], 43002)),
        ANDROID_ID,
        Some("android-v2"),
        default_caps(),
        Some(token),
        true,
    )
    .await;

    let seen_dot2 = Arc::new(Mutex::new(Vec::<u64>::new()));
    let seen_android = Arc::new(Mutex::new(Vec::<u64>::new()));
    let s_dot2 = seen_dot2.clone();
    let s_and = seen_android.clone();
    state
        .voice
        .set_test_audio_chunk_hook(Some(Arc::new(move |dev, seq| {
            if dev == ANDROID_ID {
                // Simulate slow Silero ONNX inference on Device B.
                std::thread::sleep(Duration::from_millis(8));
                s_and.lock().unwrap().push(seq);
            } else if dev == DOT2_ID {
                s_dot2.lock().unwrap().push(seq);
            }
        })));

    let quiet_b64 = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 320]);
    let now = hm_core::id::now_ms();
    // Flood Device B with 450 chunks (exceeding DEVICE_AUDIO_QUEUE_CAP=256) while Device A
    // sends 100 chunks concurrently.
    for i in 1..=450u64 {
        gw_android.send(audio_chunk_frame(ANDROID_ID, quiet_b64.clone(), now + i, i));
        if i <= 100 {
            gw_dot2.send(audio_chunk_frame(DOT2_ID, quiet_b64.clone(), now + i, i));
        }
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if seen_dot2.lock().unwrap().len() >= 100 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let dot2_seqs = seen_dot2.lock().unwrap().clone();
    assert_eq!(
        dot2_seqs.len(),
        100,
        "dot2-d must receive all 100 frames with zero drops despite android-2 burst/slow VAD"
    );
    assert!(
        dot2_seqs.windows(2).all(|w| w[0] < w[1]),
        "dot2-d frames must be processed in strict FIFO order"
    );
    let drops = state.voice.audio_dropped_by_device();
    assert_eq!(
        drops.get(DOT2_ID).copied().unwrap_or(0),
        0,
        "dot2-d drop counter must be 0: {drops:?}"
    );
    assert!(
        drops.get(ANDROID_ID).copied().unwrap_or(0) > 0,
        "android-2 burst (>256 with slow VAD) should overflow ONLY android-2's own queue: {drops:?}"
    );

    state.voice.set_test_audio_chunk_hook(None);
    gw_dot2.disconnect();
    gw_android.disconnect();
}

/// 4. `MG-P1-05`: `tool/learning.rs` passes `device_id` on all outbound requests and
/// prevents sibling devices from hijacking or stopping an active learning session.
#[tokio::test]
async fn mg_p1_05_learning_tool_passes_device_id_and_blocks_sibling_hijack() {
    let dir = TempDir::new("mg_p1_05");
    let (state, token) = make_app(&dir);

    let gw_dot2 = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([127, 0, 0, 1], 44001)),
        DOT2_ID,
        None,
        default_caps(),
        None,
        true,
    )
    .await;
    let gw_android = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([192, 168, 5, 70], 44002)),
        ANDROID_ID,
        Some("android-v2"),
        default_caps(),
        Some(token),
        true,
    )
    .await;

    #[derive(Clone, Default)]
    struct LearningMock {
        hits: Arc<Mutex<Vec<(String, String, Value)>>>,
    }
    let mock = LearningMock::default();
    let app = Router::new()
        .route(
            "/api/v1/learning/device/decks",
            get(|AxumState(m): AxumState<LearningMock>, uri: Uri| async move {
                m.hits
                    .lock()
                    .unwrap()
                    .push(("GET".into(), uri.to_string(), json!(null)));
                Json(json!({ "decks": [{ "id": "d1", "name": "CET4" }] }))
            }),
        )
        .route(
            "/api/v1/learning/device/sessions/start",
            post(
                |AxumState(m): AxumState<LearningMock>, uri: Uri, Json(body): Json<Value>| async move {
                    m.hits
                        .lock()
                        .unwrap()
                        .push(("POST".into(), uri.to_string(), body));
                    Json(json!({
                        "status": "active",
                        "session_id": "sess-1",
                        "attempt": { "attempt_id": "att-1", "word": "apple" }
                    }))
                },
            ),
        )
        .route(
            "/api/v1/learning/device/current",
            get(|AxumState(m): AxumState<LearningMock>, uri: Uri| async move {
                m.hits
                    .lock()
                    .unwrap()
                    .push(("GET".into(), uri.to_string(), json!(null)));
                Json(json!({
                    "active": true,
                    "status": "active",
                    "session_id": "sess-1",
                    "attempt": { "attempt_id": "att-1", "word": "apple" }
                }))
            }),
        )
        .route(
            "/api/v1/learning/device/current/answer",
            post(
                |AxumState(m): AxumState<LearningMock>, uri: Uri, Json(body): Json<Value>| async move {
                    m.hits
                        .lock()
                        .unwrap()
                        .push(("POST".into(), uri.to_string(), body));
                    Json(json!({
                        "status": "active",
                        "session_id": "sess-1",
                        "correct": true,
                        "attempt": { "attempt_id": "att-2", "word": "banana" }
                    }))
                },
            ),
        )
        .route(
            "/api/v1/learning/device/current/reveal",
            post(
                |AxumState(m): AxumState<LearningMock>, uri: Uri, Json(body): Json<Value>| async move {
                    m.hits
                        .lock()
                        .unwrap()
                        .push(("POST".into(), uri.to_string(), body));
                    Json(json!({ "status": "active", "answer": "apple" }))
                },
            ),
        )
        .route(
            "/api/v1/learning/device/current/control",
            post(
                |AxumState(m): AxumState<LearningMock>, uri: Uri, Json(body): Json<Value>| async move {
                    m.hits
                        .lock()
                        .unwrap()
                        .push(("POST".into(), uri.to_string(), body.clone()));
                    let action = body["action"].as_str().unwrap_or("");
                    let status = if matches!(action, "pause" | "end") {
                        "ended"
                    } else {
                        "active"
                    };
                    Json(json!({ "status": status, "session_id": "sess-1" }))
                },
            ),
        )
        .with_state(mock.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let cfg_svc = ConfigService::new(state.store.clone());
    cfg_svc
        .save_personal_endpoint(
            LEARNING_KEY,
            LEARNING_TOKEN_SECRET,
            PersonalEndpointConfig {
                enabled: true,
                url: mock_url,
                timeout_ms: 5_000,
            },
            None,
            "test",
        )
        .unwrap();

    let dot2_ctx = ctx_for("T-learn-dot2", Some(DOT2_ID));
    let and_ctx = ctx_for("T-learn-and", Some(ANDROID_ID));

    // 1. GET decks carries ?device_id=dot2-d.
    let decks_res = state
        .registry
        .call("learning.decks", json!({}), &dot2_ctx)
        .await;
    assert_eq!(decks_res.status, ToolStatus::Completed, "{decks_res:?}");
    {
        let hits = mock.hits.lock().unwrap();
        assert!(
            hits.last()
                .unwrap()
                .1
                .contains(&format!("device_id={DOT2_ID}")),
            "GET learning.decks must carry device_id query param: {hits:?}"
        );
    }

    // 2. dot2-d starts active session -> POST body carries device_id = dot2-d.
    let start_res = state
        .registry
        .call("learning.start", json!({ "deck_id": "d1" }), &dot2_ctx)
        .await;
    assert_eq!(start_res.status, ToolStatus::Completed, "{start_res:?}");
    {
        let hits = mock.hits.lock().unwrap();
        assert_eq!(hits.last().unwrap().2["device_id"], DOT2_ID);
    }

    // 3. Sibling android-2 attempting start/current/answer/reveal/control is blocked fail-closed!
    let hits_before_hijack = mock.hits.lock().unwrap().len();
    for (tool, args) in [
        ("learning.start", json!({ "deck_id": "d1" })),
        ("learning.current", json!({})),
        ("learning.answer", json!({ "answer": "apple" })),
        ("learning.reveal", json!({})),
        ("learning.control", json!({ "action": "end" })),
    ] {
        let res = state.registry.call(tool, args, &and_ctx).await;
        assert_eq!(
            res.status,
            ToolStatus::Failed,
            "sibling {tool} must fail closed while dot2-d owns active learning session: {res:?}"
        );
        assert_eq!(
            res.error.as_ref().map(|e| e.code.as_str()),
            Some("LEARNING_DEVICE_BUSY")
        );
    }
    assert_eq!(
        mock.hits.lock().unwrap().len(),
        hits_before_hijack,
        "blocked sibling learning calls must never hit learning-svc"
    );

    // 4. Owner dot2-d answers & ends session -> POST bodies carry device_id = dot2-d.
    let ans_res = state
        .registry
        .call("learning.answer", json!({ "answer": "apple" }), &dot2_ctx)
        .await;
    assert_eq!(ans_res.status, ToolStatus::Completed, "{ans_res:?}");
    assert_eq!(
        mock.hits.lock().unwrap().last().unwrap().2["device_id"],
        DOT2_ID
    );

    let end_res = state
        .registry
        .call("learning.control", json!({ "action": "end" }), &dot2_ctx)
        .await;
    assert_eq!(end_res.status, ToolStatus::Completed, "{end_res:?}");
    assert_eq!(
        mock.hits.lock().unwrap().last().unwrap().2["device_id"],
        DOT2_ID
    );

    // 5. After dot2-d ends its session, android-2 can now start cleanly with device_id = android-2.
    let and_start = state
        .registry
        .call("learning.start", json!({ "deck_id": "d1" }), &and_ctx)
        .await;
    assert_eq!(and_start.status, ToolStatus::Completed, "{and_start:?}");
    assert_eq!(
        mock.hits.lock().unwrap().last().unwrap().2["device_id"],
        ANDROID_ID
    );

    gw_dot2.disconnect();
    gw_android.disconnect();
}

/// 5. `MG-P1-06`: Bounded oldest-entry FIFO eviction on Music & Personal Media
/// candidate caches preserves sibling in-flight candidates instead of bulk `.clear()`.
#[tokio::test]
async fn mg_p1_06_candidate_cache_fifo_eviction_preserves_sibling_inflight() {
    let dir = TempDir::new("mg_p1_06");
    let store = Arc::new(Store::open(&dir.0.join("hm.db")).unwrap());
    let hub = DeviceHub::new();

    let music = MusicRuntime::new(store.clone(), hub.clone());
    let mk_track = |id: &str| Track {
        source: MusicSource::Library,
        id: id.into(),
        title: format!("Song {id}"),
        artist: "Artist".into(),
        binding: "bind-1".into(),
    };
    music.remember_candidates("dev-a-oldest", &[mk_track("tr-old")]);
    for i in 0..60 {
        music.remember_candidates(&format!("dev-b-{i}"), &[mk_track(&format!("tr-b-{i}"))]);
    }
    music.remember_candidates("dev-a-inflight", &[mk_track("tr-a-live")]);
    for i in 60..70 {
        music.remember_candidates(&format!("dev-b-{i}"), &[mk_track(&format!("tr-b-{i}"))]);
    }
    assert!(
        !music.has_candidate("dev-a-oldest", MusicSource::Library, "tr-old"),
        "oldest music candidate beyond cap 64 must be evicted FIFO"
    );
    assert!(
        music.has_candidate("dev-a-inflight", MusicSource::Library, "tr-a-live"),
        "recent sibling in-flight music candidate MUST survive FIFO eviction (no bulk clear)"
    );
    assert!(
        music.has_candidate("dev-b-69", MusicSource::Library, "tr-b-69"),
        "newest music candidate must remain cached"
    );

    let pm = PersonalMediaRuntime::new(store, hub);
    let mut oldest_story = json!({
        "results": [{ "id": "st-oldest", "title": "Old Story", "kind": "episode" }]
    });
    pm.remember_story_items_for(Some("dev-a"), &mut oldest_story);

    let mut batch1 = Vec::new();
    for i in 0..500 {
        batch1.push(
            json!({ "id": format!("st-b-{i}"), "title": format!("Story {i}"), "kind": "episode" }),
        );
    }
    let mut p1 = json!({ "results": batch1 });
    pm.remember_story_items_for(Some("dev-b"), &mut p1);

    let mut inflight_story = json!({
        "results": [{ "id": "st-a-inflight", "title": "Live Story", "kind": "album" }]
    });
    pm.remember_story_items_for(Some("dev-a"), &mut inflight_story);

    let mut batch2 = Vec::new();
    for i in 500..525 {
        batch2.push(
            json!({ "id": format!("st-b-{i}"), "title": format!("Story {i}"), "kind": "episode" }),
        );
    }
    let mut p2 = json!({ "results": batch2 });
    pm.remember_story_items_for(Some("dev-b"), &mut p2);

    assert!(pm.lookup_title("st-oldest").is_none());
    assert!(pm.lookup_kind("st-oldest").is_none());
    assert_eq!(
        pm.lookup_title("st-a-inflight").as_deref(),
        Some("Live Story")
    );
    assert_eq!(pm.lookup_kind("st-a-inflight").as_deref(), Some("album"));

    pm.remember_radio_items(&json!({ "results": [{ "id": "rad-old", "title": "Old FM" }] }));
    let mut r1 = Vec::new();
    for i in 0..500 {
        r1.push(json!({ "id": format!("rad-b-{i}"), "title": format!("FM {i}") }));
    }
    pm.remember_radio_items(&json!({ "results": r1 }));
    pm.remember_radio_items(&json!({ "results": [{ "id": "rad-a-live", "title": "Live FM" }] }));
    let mut r2 = Vec::new();
    for i in 500..525 {
        r2.push(json!({ "id": format!("rad-b-{i}"), "title": format!("FM {i}") }));
    }
    pm.remember_radio_items(&json!({ "results": r2 }));

    assert!(pm.lookup_radio_title("rad-old").is_none());
    assert_eq!(
        pm.lookup_radio_title("rad-a-live").as_deref(),
        Some("Live FM")
    );
}

/// 6. `MG-P2-01`: Concurrent per-device diagnostic `mic_capture` runs independently
/// without blocking or overwriting sibling captures.
#[tokio::test]
async fn mg_p2_01_concurrent_mic_capture_isolated_per_device() {
    let dir = TempDir::new("mg_p2_01");
    let (state, token) = make_app(&dir);

    let gw_dot2 = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([127, 0, 0, 1], 45001)),
        DOT2_ID,
        None,
        default_caps(),
        None,
        true,
    )
    .await;
    let gw_android = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([192, 168, 5, 80], 45002)),
        ANDROID_ID,
        Some("android-v2"),
        default_caps(),
        Some(token),
        true,
    )
    .await;

    let start_a = state.voice.start_mic_capture(DOT2_ID, 1).unwrap();
    let start_b = state.voice.start_mic_capture(ANDROID_ID, 1).unwrap();
    assert_eq!(start_a["state"], "recording");
    assert_eq!(start_b["state"], "recording");
    assert_ne!(
        start_a["path"], start_b["path"],
        "concurrent captures must write to distinct files"
    );
    assert!(state.voice.start_mic_capture(DOT2_ID, 1).is_err());

    let pcm_a_bytes: Vec<u8> = std::iter::repeat_n(1234i16, 1600)
        .flat_map(|s| s.to_le_bytes())
        .collect();
    let pcm_b_bytes: Vec<u8> = std::iter::repeat_n(-2345i16, 1600)
        .flat_map(|s| s.to_le_bytes())
        .collect();
    let b64_a = base64::engine::general_purpose::STANDARD.encode(&pcm_a_bytes);
    let b64_b = base64::engine::general_purpose::STANDARD.encode(&pcm_b_bytes);
    let now = hm_core::id::now_ms();
    for seq in 1..=12u64 {
        gw_dot2.send(audio_chunk_frame(
            DOT2_ID,
            b64_a.clone(),
            now + seq * 100,
            seq,
        ));
        gw_android.send(audio_chunk_frame(
            ANDROID_ID,
            b64_b.clone(),
            now + seq * 100,
            seq,
        ));
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let sa = state.voice.mic_capture_status(DOT2_ID);
        let sb = state.voice.mic_capture_status(ANDROID_ID);
        if sa["state"] == "done" && sb["state"] == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state.voice.mic_capture_status(DOT2_ID)["state"], "done");
    assert_eq!(state.voice.mic_capture_status(ANDROID_ID)["state"], "done");

    let wav_a = state.voice.mic_capture_wav(DOT2_ID).expect("dot2 wav");
    let wav_b = state
        .voice
        .mic_capture_wav(ANDROID_ID)
        .expect("android wav");
    assert_eq!(&wav_a[0..4], b"RIFF");
    assert_eq!(&wav_b[0..4], b"RIFF");
    let first_sample_a = i16::from_le_bytes([wav_a[44], wav_a[45]]);
    let first_sample_b = i16::from_le_bytes([wav_b[44], wav_b[45]]);
    assert_eq!(first_sample_a, 1234);
    assert_eq!(first_sample_b, -2345);

    gw_dot2.disconnect();
    gw_android.disconnect();
}

/// 7. `MG-P2-02`: `HubState.volume_pending` and `volume_pending_gen` are unconditionally
/// cleaned when `VolumeReceipt { device: Some(...) }` arrives (zero memory leak).
#[tokio::test]
async fn mg_p2_02_volume_pending_cleaned_on_receipt_no_leak() {
    let dir = TempDir::new("mg_p2_02");
    let (state, token) = make_app(&dir);

    let gw_dot2 = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([127, 0, 0, 1], 46001)),
        DOT2_ID,
        None,
        default_caps(),
        None,
        true,
    )
    .await;
    let gw_android = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([192, 168, 5, 90], 46002)),
        ANDROID_ID,
        Some("android-v2"),
        default_caps(),
        Some(token),
        true,
    )
    .await;

    for i in 1..=20u32 {
        let v = (i as f64) * 0.03;
        state.hub.volume_set(DOT2_ID, v).unwrap();
        state.hub.volume_set(ANDROID_ID, v).unwrap();
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let snap = state.hub.snapshot();
        if snap["pending_volume"] == 0 && snap["pending_volume_gen"] == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let snap = state.hub.snapshot();
    assert_eq!(
        snap["pending_volume"], 0,
        "volume_pending must drain to 0 when VolumeReceipt carries device=Some(..): {snap}"
    );
    assert_eq!(
        snap["pending_volume_gen"], 0,
        "volume_pending_gen must drain to 0 alongside volume_pending: {snap}"
    );

    gw_dot2.disconnect();
    gw_android.disconnect();
}

/// 8. `MG-P2-03`: Idle-gated offline-KWS gateways (e.g. `android-v2`) report healthy
/// `mic.state == "ready"` (`idle_gated: true`), while stalled continuous `dot2-d` AND
/// active-session stalled `android-v2` report `"stale"`.
#[tokio::test]
async fn mg_p2_03_idle_gated_android_not_flagged_mic_stale() {
    let dir = TempDir::new("mg_p2_03");
    let (state, token) = make_app(&dir);
    let base = spawn_http(state.clone()).await;
    let http = auth_http(&token);

    let gw_dot2 = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([127, 0, 0, 1], 47001)),
        DOT2_ID,
        None,
        default_caps(),
        None,
        true,
    )
    .await;
    let gw_android = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([192, 168, 5, 95], 47002)),
        ANDROID_ID,
        Some("android-v2"),
        default_caps(),
        Some(token),
        true,
    )
    .await;

    for (gw, dev) in [(&gw_dot2, DOT2_ID), (&gw_android, ANDROID_ID)] {
        gw.send(GatewayFrame::MicReport {
            device: dev.into(),
            active: true,
            layout: Some("mono".into()),
            sample_rate: Some(16_000),
            channels: Some(1),
            detail: Some("started".into()),
        });
        gw.send(GatewayFrame::KwsReport {
            device: dev.into(),
            state: "running".into(),
            detail: "ready".into(),
            restarts: 0,
            detections: 0,
            guarded: 0,
            epoch: 1,
            loaded_hash: String::new(),
            loaded_ms: 0,
            local_exit: 0,
            threshold: 0.25,
            score: 3.0,
        });
    }
    assert!(
        wait_until(2_000, || {
            state.voice.mode_snapshot()["devices"]
                .as_array()
                .is_some_and(|ds| {
                    [DOT2_ID, ANDROID_ID].iter().all(|id| {
                        ds.iter().any(|d| {
                            d["device"] == *id
                                && d["mic"]["session"] == true
                                && d["kws"]["state"] == "running"
                        })
                    })
                })
        })
        .await,
        "both devices must report active mic session and running KWS"
    );

    let diag_idle: Value = http
        .post(format!("{base}/api/v0/diagnostics/self_check"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let find_mic = |doc: &Value, id: &str| -> Value {
        let mic_stage = doc["stages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["stage"] == "mic")
            .unwrap_or_else(|| panic!("missing mic stage in {doc}"));
        mic_stage["evidence"]["all_devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["device"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("missing mic device {id} in {mic_stage}"))
    };
    let and_idle = find_mic(&diag_idle, ANDROID_ID);
    let dot_idle = find_mic(&diag_idle, DOT2_ID);
    assert_eq!(and_idle["state"], "ready", "{and_idle}");
    assert_eq!(and_idle["idle_gated"], true, "{and_idle}");
    assert_eq!(dot_idle["state"], "stale", "{dot_idle}");
    assert_eq!(dot_idle["idle_gated"], false, "{dot_idle}");

    // Active-session stall on idle-gated gateway MUST still flag `"stale"`!
    state.voice.test_activate_session(ANDROID_ID);
    let diag_active: Value = http
        .post(format!("{base}/api/v0/diagnostics/self_check"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let and_active = find_mic(&diag_active, ANDROID_ID);
    assert_eq!(
        and_active["state"], "stale",
        "active-session idle-gated gateway with missing chunks must still flag stale: {and_active}"
    );
    assert_eq!(and_active["idle_gated"], false, "{and_active}");

    // Also verify top-level `stages[mic]` directly when ONLY android-v2 is online!
    gw_dot2.disconnect();
    for _ in 0..50 {
        if state.hub.online_device_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let diag_and_only_active: Value = http
        .post(format!("{base}/api/v0/diagnostics/self_check"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let top_mic_active = diag_and_only_active["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stage"] == "mic")
        .unwrap();
    assert_eq!(top_mic_active["state"], "stale", "{top_mic_active}");
    gw_android.disconnect();
}

/// 9. `MG-P2-04` (API side): Multi-device fail-closed `400 DEVICE_REQUIRED` on
/// reminders & audible diagnostics, per-device `wifi_provisioning` on `/api/v0/status`,
/// and deterministic sorted ordering in `ManagementRuntime::resolve_device`.
#[tokio::test]
async fn mg_p2_04_api_multidevice_fail_closed_and_wifi_provisioning_per_device() {
    let dir = TempDir::new("mg_p2_04");
    let (state, token) = make_app(&dir);
    let base = spawn_http(state.clone()).await;
    let http = auth_http(&token);

    let gw_dot2 = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([127, 0, 0, 1], 48001)),
        DOT2_ID,
        None,
        default_caps(),
        None,
        true,
    )
    .await;
    let gw_android = MockGw::connect(
        state.hub.clone(),
        SocketAddr::from(([192, 168, 5, 99], 48002)),
        ANDROID_ID,
        Some("android-v2"),
        default_caps(),
        Some(token),
        true,
    )
    .await;

    // 1. With 2 devices online, POST /api/v0/reminders without `device` fails closed 400 DEVICE_REQUIRED.
    let rem_fail = http
        .post(format!("{base}/api/v0/reminders"))
        .json(&json!({ "note": "喝水", "delay_minutes": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(rem_fail.status(), 400);
    let rem_fail_body: Value = rem_fail.json().await.unwrap();
    assert_eq!(rem_fail_body["code"], "DEVICE_REQUIRED", "{rem_fail_body}");

    let rem_ok = http
        .post(format!("{base}/api/v0/reminders"))
        .json(&json!({ "note": "喝水", "delay_minutes": 1, "device": ANDROID_ID }))
        .send()
        .await
        .unwrap();
    assert_eq!(rem_ok.status(), 200);
    let rem_ok_body: Value = rem_ok.json().await.unwrap();
    assert_eq!(rem_ok_body["device"], ANDROID_ID, "{rem_ok_body}");

    // Audible diagnostics without `device` fail closed 400 DEVICE_REQUIRED when >1 online.
    for (path, body) in [
        ("/api/v0/diagnostics/tests/voice_play", json!({})),
        (
            "/api/v0/diagnostics/tests/play_asset",
            json!({ "asset": "test.wav" }),
        ),
        ("/api/v0/diagnostics/tests/playback_loopback", json!({})),
        ("/api/v0/diagnostics/tests/mics", json!({})),
    ] {
        let resp = http
            .post(format!("{base}{path}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "{path} must fail closed 400 without device when >1 device online"
        );
        let j: Value = resp.json().await.unwrap();
        assert_eq!(j["code"], "DEVICE_REQUIRED", "{path}: {j}");
    }

    // 2. Per-device `wifi_provisioning` on GET /api/v0/status.
    gw_dot2.send(GatewayFrame::Button {
        device: DOT2_ID.into(),
        action: "mute_hold_wifi_setup".into(),
        ts: hm_core::id::now_ms(),
        seq: 1,
    });
    assert!(
        wait_until(2_000, || {
            state.voice.wifi_provisioning_map().contains_key(DOT2_ID)
        })
        .await,
        "dot2-d Wi-Fi provisioning button event should be recorded"
    );

    let st_dot2: Value = http
        .get(format!("{base}/api/v0/status?device={DOT2_ID}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st_dot2["wifi_provisioning"]["requested"], true, "{st_dot2}");
    assert_eq!(st_dot2["wifi_provisioning"]["device"], DOT2_ID);

    let st_android: Value = http
        .get(format!("{base}/api/v0/status?device={ANDROID_ID}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        st_android["wifi_provisioning"]["requested"], false,
        "dot2-d Wi-Fi provisioning must not bleed into android-2: {st_android}"
    );
    assert_eq!(st_android["wifi_provisioning"]["device"], ANDROID_ID);
    assert_eq!(
        st_android["wifi_provisioning_by_device"][DOT2_ID]["requested"],
        true
    );
    assert!(
        st_android["wifi_provisioning_by_device"]
            .get(ANDROID_ID)
            .is_none(),
        "{st_android}"
    );

    // 3. Multi-device fail-closed AND deterministic sorted ordering in ManagementRuntime::resolve_device.
    let unbound_ctx = ctx_for("T-mgmt-order", None);
    let ambig = state
        .registry
        .call("system.volume_reading", json!({}), &unbound_ctx)
        .await;
    assert_eq!(ambig.status, ToolStatus::Failed, "{ambig:?}");
    let ambig_detail = ambig
        .error
        .as_ref()
        .map(|e| e.detail.clone())
        .unwrap_or_default();
    assert!(
        ambig_detail.contains("AMBIGUOUS_DEVICE"),
        "expected fail-closed AMBIGUOUS_DEVICE when >1 device online: {ambig_detail}"
    );
    let mut errors = Vec::new();
    for _ in 0..8 {
        let res = state
            .registry
            .call(
                "system.volume_reading",
                json!({ "device": "nonexistent-room" }),
                &unbound_ctx,
            )
            .await;
        assert_eq!(res.status, ToolStatus::Failed, "{res:?}");
        errors.push(
            res.error
                .as_ref()
                .map(|e| e.detail.clone())
                .unwrap_or_default(),
        );
    }
    let first = &errors[0];
    assert!(
        errors.iter().all(|e| e == first),
        "ManagementRuntime::resolve_device error ordering must be deterministic across calls: {errors:?}"
    );
    let idx_and = first.find(ANDROID_ID).expect("android id in error");
    let idx_dot = first.find(DOT2_ID).expect("dot2 id in error");
    assert!(
        idx_and < idx_dot,
        "sorted device order must list '{ANDROID_ID}' before '{DOT2_ID}': {first}"
    );

    // 4. Single-device fallback still works once android-2 disconnects.
    gw_android.disconnect();
    for _ in 0..50 {
        if state.hub.online_device_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state.hub.online_device_count(), 1);
    let single_rem = http
        .post(format!("{base}/api/v0/reminders"))
        .json(&json!({ "note": "单设备默认", "delay_minutes": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(single_rem.status(), 200);
    let single_body: Value = single_rem.json().await.unwrap();
    assert_eq!(single_body["device"], DOT2_ID, "{single_body}");

    gw_dot2.disconnect();
}

/// Real wire-level E2E regression for LX06 KWS isolation (`DeviceKwsClass::Lx06` = 0.20 / 2.50):
/// 1) Fresh DB (no REFLEX_KEY, global default 0.25/3.0): connecting a v2 LX06 gateway
///    triggers `on_device_online` KWS reconciliation pushing `threshold=0.20, score=2.5`
///    and preserving `"你好高达"` (never forcing `"你好小智"`).
/// 2) With global `kws_threshold=0.32, kws_score=2.50` and an explicit per-device override on
///    `lx06-custom` (`0.17 / 2.80`), four concurrent gateways (Dot2, Android, LX06, LX06-override)
///    each receive their exact isolated `ConfigApply(ConfigBlock::Kws)` payload over the wire.
#[tokio::test]
async fn lx06_kws_reconnect_reconcile_and_concurrent_multi_family_wire_deploy() {
    use hm_core::gateway::ConfigReceiptStatus;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    async fn connect_v2_device(
        hub: &Arc<DeviceHub>,
        peer: SocketAddr,
        gateway_name: &str,
        stable_id: &str,
        model: &str,
        audio_profile: &str,
        profile_version: Option<&str>,
        token: Option<String>,
    ) -> (
        tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
        tokio::net::tcp::OwnedWriteHalf,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hub_c = hub.clone();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let _ = hub_c.serve_connection_from(s, Some(peer)).await;
        });
        let stream = TcpStream::connect(addr).await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let lines = BufReader::new(rd).lines();
        let hello = GatewayFrame::Hello {
            protocol: 2,
            min_protocol: 0,
            supported_versions: vec![0, 2],
            gateway: gateway_name.into(),
            gateway_version: "0.0.53".into(),
            devices: vec![HelloDevice {
                stable_id: stable_id.into(),
                model: model.into(),
                firmware: Some("hm-firmware 0.0.53".into()),
                capabilities: vec![
                    "audio_in".into(),
                    "audio_out".into(),
                    "config_apply_v2".into(),
                    "config_block_kws".into(),
                ],
                audio_profile: audio_profile.into(),
                wifi_ssid: None,
            }],
            aec_tail_ms: None,
            profile_version: profile_version.map(String::from),
            auth_token: token,
        };
        wr.write_all(format!("{}\n", serde_json::to_string(&hello).unwrap()).as_bytes())
            .await
            .unwrap();
        wr.flush().await.unwrap();
        assert!(
            wait_until(2000, || hub.device_exists(stable_id)).await,
            "{stable_id} must register"
        );
        (lines, wr)
    }

    async fn drive_v2_kws_apply(
        lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
        wr: &mut tokio::net::tcp::OwnedWriteHalf,
        boot_id: &str,
    ) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok(Some(line))) =
                tokio::time::timeout(Duration::from_millis(600), lines.next_line()).await
            else {
                break;
            };
            let Ok(frame) = serde_json::from_str::<CoreFrame>(&line) else {
                continue;
            };
            match frame {
                CoreFrame::ConfigStateGet { request_id, device } => {
                    send_raw_gw(
                        wr,
                        &GatewayFrame::ConfigState {
                            request_id: Some(request_id),
                            device,
                            gateway_boot_id: Some(boot_id.into()),
                            state_revision: Some(1),
                            capability_fingerprint: Some(format!("fp-{boot_id}")),
                            blocks: vec![],
                            ts: 100,
                        },
                    )
                    .await;
                }
                CoreFrame::ConfigValidate {
                    request_id,
                    device,
                    block,
                    schema_version,
                    block_hash,
                    ..
                } => {
                    send_raw_gw(
                        wr,
                        &GatewayFrame::ConfigValidated {
                            request_id,
                            device,
                            block,
                            schema_version,
                            block_hash,
                            ok: true,
                            supported: true,
                            blockers: vec![],
                            state: ConfigReceiptStatus::Validated,
                            reason: None,
                            retryable: false,
                            gateway_boot_id: Some(boot_id.into()),
                            state_revision: Some(1),
                            capability_fingerprint: None,
                            evidence: None,
                            ts: 101,
                        },
                    )
                    .await;
                }
                CoreFrame::ConfigApply {
                    device,
                    core_node_id,
                    data_epoch,
                    application_id,
                    operation_id,
                    desired_revision,
                    block_generation,
                    block,
                    schema_version,
                    block_hash,
                    values,
                    ..
                } if block == ConfigBlock::Kws => {
                    send_raw_gw(
                        wr,
                        &GatewayFrame::ConfigReceipt {
                            device,
                            core_node_id,
                            data_epoch,
                            application_id,
                            operation_id,
                            desired_revision,
                            block_generation,
                            block,
                            schema_version,
                            block_hash: block_hash.clone(),
                            state: ConfigReceiptStatus::Active,
                            reason: None,
                            retryable: false,
                            gateway_boot_id: Some(boot_id.into()),
                            state_revision: Some(2),
                            persisted_hash: Some(block_hash.clone()),
                            active_hash: Some(block_hash),
                            active_values: Some(values.clone()),
                            evidence: None,
                            ts: 102,
                        },
                    )
                    .await;
                    return values;
                }
                _ => {}
            }
        }
        panic!("timed out awaiting ConfigApply(Kws) for {boot_id}");
    }

    // --- Part 1: Fresh DB (no REFLEX_KEY, global 0.25/3.0) LX06 reconnect auto-reconcile ---
    let dir1 = TempDir::new("hm-lx06-fresh");
    let (state1, token1) = make_app(&dir1);
    assert!(
        state1
            .store
            .config_get(hm_core::config::REFLEX_KEY)
            .unwrap()
            .is_none(),
        "fresh DB must have no REFLEX_KEY"
    );
    let (mut lx_lines, mut lx_wr) = connect_v2_device(
        &state1.hub,
        SocketAddr::from(([127, 0, 0, 1], 43101)),
        "home-mate-device-gateway 0.0.53 (lx06-direct)",
        "lx06",
        "LX06",
        "lx06_shared_dmix",
        None,
        Some(token1),
    )
    .await;
    let fresh_vals = drive_v2_kws_apply(&mut lx_lines, &mut lx_wr, "boot-lx06-fresh").await;
    assert_eq!(
        fresh_vals["threshold"],
        json!(0.20),
        "fresh-DB LX06 reconnect must reconcile threshold=0.20 (not 0.25/0.32): {fresh_vals}"
    );
    assert_eq!(
        fresh_vals["score"],
        json!(2.5),
        "fresh-DB LX06 reconnect must reconcile score=2.5: {fresh_vals}"
    );
    let kw_str = fresh_vals["keywords"].as_str().unwrap_or("");
    assert!(
        kw_str.contains("你好高达") && !kw_str.contains("你好小智"),
        "LX06 wake word must stay 你好高达 and never be forced to 你好小智: {kw_str}"
    );
    drop(lx_wr);
    drop(lx_lines);

    // --- Part 2: Concurrent Dot2 (0.32/2.5), Android (0.22/3.0), LX06 (0.20/2.5), LX06 override (0.17/2.8) ---
    let dir2 = TempDir::new("hm-lx06-multi");
    let (state2, token2) = make_app(&dir2);
    state2
        .voice
        .set_mode_with_follow_window(
            "global",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            hm_core::voice::FollowWindowPatch {
                kws_threshold: Some(0.32),
                kws_score: Some(2.5),
                ..Default::default()
            },
            "test-global-dot2",
        )
        .unwrap();
    state2
        .voice
        .set_mode_with_follow_window(
            "lx06-custom",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            hm_core::voice::FollowWindowPatch {
                kws_threshold: Some(0.17),
                kws_score: Some(2.8),
                ..Default::default()
            },
            "test-lx06-override",
        )
        .unwrap();

    let (mut dot2_l, mut dot2_w) = connect_v2_device(
        &state2.hub,
        SocketAddr::from(([127, 0, 0, 1], 43201)),
        "home-mate-device-gateway 0.0.53 (dot2-direct)",
        "dot2-d",
        "AEOBC",
        "fireos_builtin_speaker",
        None,
        Some(token2.clone()),
    )
    .await;
    let (mut and_l, mut and_w) = connect_v2_device(
        &state2.hub,
        SocketAddr::from(([192, 168, 5, 61], 43202)),
        "home-mate-gateway-android 2.3.10",
        "android-fold",
        "samsung-SM-F9760",
        "android_farfield_v1",
        Some("android-v2"),
        Some(token2.clone()),
    )
    .await;
    let (mut lx_l, mut lx_w) = connect_v2_device(
        &state2.hub,
        SocketAddr::from(([192, 168, 5, 62], 43203)),
        "home-mate-device-gateway 0.0.53 (lx06-direct)",
        "lx06-living",
        "LX06",
        "lx06_shared_dmix",
        None,
        Some(token2.clone()),
    )
    .await;
    let (mut lx_ov_l, mut lx_ov_w) = connect_v2_device(
        &state2.hub,
        SocketAddr::from(([192, 168, 5, 63], 43204)),
        "home-mate-device-gateway 0.0.53 (lx06-direct)",
        "lx06-custom",
        "LX06",
        "lx06_shared_dmix",
        None,
        Some(token2),
    )
    .await;

    // Drain Dot2, LX06, and LX06-custom auto-reconcile ConfigApply frames concurrently,
    // and trigger explicit deploy_reflex_auto on Android (since 0.22/3.0 matches Android base).
    let (dot2_vals, lx_vals, lx_ov_vals) = tokio::join!(
        drive_v2_kws_apply(&mut dot2_l, &mut dot2_w, "boot-dot2"),
        drive_v2_kws_apply(&mut lx_l, &mut lx_w, "boot-lx06"),
        drive_v2_kws_apply(&mut lx_ov_l, &mut lx_ov_w, "boot-lx06-ov"),
    );
    assert_eq!(dot2_vals["threshold"], json!(0.32));
    assert_eq!(dot2_vals["score"], json!(2.5));
    assert_eq!(lx_vals["threshold"], json!(0.20));
    assert_eq!(lx_vals["score"], json!(2.5));
    assert_eq!(lx_ov_vals["threshold"], json!(0.17));
    assert_eq!(lx_ov_vals["score"], json!(2.8));

    let voice_for_and = state2.voice.clone();
    let deploy_and = tokio::spawn(async move {
        voice_for_and
            .deploy_reflex_auto("android-fold")
            .await
            .unwrap()
    });
    let and_vals = drive_v2_kws_apply(&mut and_l, &mut and_w, "boot-android").await;
    let _ = deploy_and.await.unwrap();
    assert_eq!(and_vals["threshold"], json!(0.22));
    assert_eq!(and_vals["score"], json!(3.0));
}
