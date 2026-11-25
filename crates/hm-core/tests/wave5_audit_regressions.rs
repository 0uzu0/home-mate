//! Wave 5 Full-Codebase Deep Audit Remediation Regression Suite:
//! - Cross-Repo P0-1: Preroll `AudioChunk` drain barrier before `DeviceEvent::Keyword`.
//! - Core P0-01: `POST /api/v0/chat` under `lan_gateway_exempt` blocks loopback speakers & mutations.
//! - P1-02: `config_delivery` rollback `Clear` removes `mode.duplex_requested.<dev>`.
//! - P1-03: Per-device `device_overrides` survive global `config_delivery` & `retreat_words` wired.
//! - P1-05 & P2-04: Device offline aborts in-flight `mic_capture` and clears `learning` ownership.
//! - P1-06: Fresh `wake_cut_cursor` (< 5 s) survives `IDLE_GAP_RESET_MS`.
//! - P1-08: `hub.play()` mid-WAV backpressure emits `CoreFrame::Stop` on Control tier.
//! - P1-11: `PUT /api/v0/devices/{id}/profile` LED push gates on v2 `config_block_led`.
//! - P1-13: Online priority on room-name resolution & `AMBIGUOUS_DEVICE` (`retryable: false`) on reminders.
//! - P2-07: `enforce_exact_unit_gate` allows registered Hub devices on NAS Core while blocking foreign/family.
//! - P2-08: `PUT /api/v0/mode` partial `spoken` patch preserves custom `detail_words` / `long_form_words`.

use axum::extract::ConnectInfo;
use base64::Engine;
use hm_core::api::{router, AppState};
use hm_core::config::{
    EffectLevel, HomeBackend, HomeConfig, HomeEntity, MasterBrainConfig, MasterBrainProtocol,
    OpenApiConfig, OpenApiOperation,
};
use hm_core::config_delivery::{
    Block, Commit, Compatibility, ConfigApplyCoordinator, ConfigDelivery, Operation, Package,
    Purpose, Source as CfgSource, Target,
};
use hm_core::gateway::{
    AudioMeta, ConfigBlock, ConfigReceiptStatus, CoreFrame, DeviceHub, GatewayFrame, HelloDevice,
    InputOrigin, PlayRequest,
};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::mic::{FeedOutcome, MicProcessor, VadConfig, WakeSliceOutcome};
use hm_core::speech::{AsrClient, SpeechError, TtsAudio, TtsClient};
use hm_core::store::Store;
use hm_core::tool::mocks::MockInject;
use hm_core::tool::{ToolCtx, ToolStatus};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const DOT2_ID: &str = "dot2-d";
const ANDROID_ID: &str = "android-lan-1";

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hm-wave5-{tag}-{}-{}",
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

struct CountingAsr(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl AsrClient for CountingAsr {
    async fn transcribe(&self, _wav: &[u8]) -> Result<String, SpeechError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(String::new())
    }
    fn provider(&self) -> &str {
        "counting-asr"
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
    rx: mpsc::UnboundedReceiver<CoreFrame>,
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
        let (rx_tx, rx) = mpsc::unbounded_channel::<CoreFrame>();
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
        let mut gw = Self {
            tx,
            rx,
            _srv: srv,
            _r: r,
            _w: w,
        };
        let ack = tokio::time::timeout(Duration::from_secs(3), gw.rx.recv())
            .await
            .expect("hello_ack timeout")
            .expect("connection closed before hello_ack");
        assert!(matches!(ack, CoreFrame::HelloAck { .. }), "got {ack:?}");
        gw
    }

    fn send(&self, frame: GatewayFrame) {
        self.tx.send(frame).unwrap();
    }

    fn disconnect(self) {
        drop(self.tx);
        self._w.abort();
        self._r.abort();
    }

    async fn recv_matching<F>(&mut self, timeout: Duration, mut pred: F) -> Option<CoreFrame>
    where
        F: FnMut(&CoreFrame) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            if rem.is_zero() {
                return None;
            }
            match tokio::time::timeout(rem, self.rx.recv()).await {
                Ok(Some(frame)) if pred(&frame) => return Some(frame),
                Ok(Some(_)) => continue,
                _ => return None,
            }
        }
    }
}

fn hello_frame(
    device_id: &str,
    caps: &[&str],
    auth_token: Option<&str>,
    profile_version: Option<&str>,
) -> GatewayFrame {
    let is_v2 = caps.contains(&"config_apply_v2");
    GatewayFrame::Hello {
        protocol: if is_v2 { 2 } else { 1 },
        supported_versions: if is_v2 { vec![0, 1, 2] } else { vec![0, 1] },
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
        profile_version: profile_version.map(str::to_string),
        auth_token: auth_token.map(str::to_string),
    }
}

fn build_state(
    dir: &TempDir,
    llm: Option<Arc<dyn hm_core::llm::LlmClient>>,
    asr: Option<Arc<dyn AsrClient>>,
) -> Arc<AppState> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let state = AppState::with_overrides(
        store,
        dir.0.clone(),
        MockInject::default(),
        llm,
        Some(Arc::new(InstantTts)),
        asr,
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

async fn post_chat_sse(base: &str, token: Option<&str>, text: &str) -> (u16, Vec<Value>) {
    let client = reqwest::Client::new();
    let mut req = client
        .post(format!("{base}/api/v0/chat"))
        .json(&json!({ "text": text }));
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    let events: Vec<Value> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .collect();
    (status, events)
}

#[tokio::test]
async fn p0_1_keyword_awaits_preceding_preroll_audio_drain_before_sampling_cursor() {
    let dir = TempDir::new("p0-1-drain");
    let asr_calls = Arc::new(AtomicUsize::new(0));
    let state = build_state(
        &dir,
        Some(Arc::new(FakeLlm::new(vec![]))),
        Some(Arc::new(CountingAsr(asr_calls.clone()))),
    );
    // Slow each audio chunk by 15 ms inside the audio VAD worker to reliably
    // expose the race where `Keyword` on the control channel would otherwise
    // overtake preroll `AudioChunk`s still queued for VAD.
    let processed_chunks = Arc::new(AtomicUsize::new(0));
    let processed_clone = processed_chunks.clone();
    state
        .voice
        .set_test_audio_chunk_hook(Some(Arc::new(move |dev, _seq| {
            if dev == DOT2_ID {
                std::thread::sleep(Duration::from_millis(15));
                processed_clone.fetch_add(1, Ordering::SeqCst);
            }
        })));

    let gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50001".parse().unwrap(),
        hello_frame(
            DOT2_ID,
            &["audio_in", "audio_out", "volume", "stop"],
            None,
            None,
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let now = hm_core::id::now_ms();
    let loud_pcm: Vec<u8> = [9000i16; 1280]
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
    let loud_b64 = base64::engine::general_purpose::STANDARD.encode(&loud_pcm);
    let quiet_pcm: Vec<u8> = vec![0u8; 1280 * 2];
    let quiet_b64 = base64::engine::general_purpose::STANDARD.encode(&quiet_pcm);

    // Send 8 loud preroll chunks (640 ms) immediately followed by Keyword on TCP.
    let mut seq = 1u64;
    for i in 0..8u64 {
        gw.send(GatewayFrame::AudioChunk {
            origin: InputOrigin::Legacy,
            device: DOT2_ID.into(),
            seq,
            ts: now + i * 80,
            format: "s16le".into(),
            sample_rate: 16_000,
            channels: 1,
            layout: "mono".into(),
            pcm_b64: loud_b64.clone(),
            vad_speech: None,
            vad_prob: None,
        });
        seq += 1;
    }
    gw.send(GatewayFrame::Keyword {
        origin: InputOrigin::Legacy,
        device: DOT2_ID.into(),
        ts: now + 640,
        seq,
        keyword: "你好高达".into(),
        fed_ms: Some(640),
        self_heard: false,
        rms_dbfs: Some(-20.0),
        lag_ms: Some(0),
        bundle_id: None,
        kws_generation: None,
        kws_tier: Some("real".into()),
        wake_dur_ms: Some(640),
        max_gap_ms: Some(80),
        l2_passed: Some(true),
    });
    seq += 1;

    // Wait for Keyword to finish processing (must wait for all 8 preroll chunks = ~120ms).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let evs = state.voice.recent(40);
        if evs
            .iter()
            .any(|e| e["kind"] == "keyword_wake" && e["device"] == DOT2_ID)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for keyword_wake; events={evs:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        processed_chunks.load(Ordering::SeqCst) >= 8,
        "Keyword MUST NOT run before all 8 preceding preroll AudioChunks complete VAD (got {})",
        processed_chunks.load(Ordering::SeqCst)
    );

    // Feed trailing silence to close the segment -> must confirm BareWake with 0 ASR calls!
    for i in 8..22u64 {
        gw.send(GatewayFrame::AudioChunk {
            origin: InputOrigin::Legacy,
            device: DOT2_ID.into(),
            seq,
            ts: now + i * 80,
            format: "s16le".into(),
            sample_rate: 16_000,
            channels: 1,
            layout: "mono".into(),
            pcm_b64: quiet_b64.clone(),
            vad_speech: None,
            vad_prob: None,
        });
        seq += 1;
    }

    let deadline2 = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let evs = state.voice.recent(60);
        if evs
            .iter()
            .any(|e| e["kind"] == "kws_bare_wake_confirmed" && e["device"] == DOT2_ID)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline2,
            "expected kws_bare_wake_confirmed after synchronized preroll drain; events={evs:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        asr_calls.load(Ordering::SeqCst),
        0,
        "synchronized bare wake must not invoke ASR"
    );
}

#[tokio::test]
async fn p0_01_chat_under_lan_gateway_exempt_blocks_loopback_speakers_and_mutations() {
    let dir = TempDir::new("p0-01-lan-chat");
    let llm = Arc::new(FakeLlm::new(vec![
        // Turn 1 (unauth LAN): try mutating loopback speaker by room name "书房"
        Scripted::Calls {
            calls: vec![(
                "system.volume".into(),
                json!({ "percent": 20, "device": "书房" }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["done1".into()],
        },
        // Turn 2 (unauth LAN): try mutating smart home device via home.set
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                json!({ "entity": "书房灯", "state": "off" }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["done2".into()],
        },
        // Turn 3 (unauth LAN): try mutating OpenAPI operation
        Scripted::Calls {
            calls: vec![(
                "openapi.call".into(),
                json!({ "operation": "toggle_alarm", "arguments": {} }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["done3".into()],
        },
        // Turn 4 (unauth LAN): try delegating to Master Brain
        Scripted::Calls {
            calls: vec![(
                "brain.delegate".into(),
                json!({ "task": "reboot server" }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["done4".into()],
        },
        // Turn 5 (unauth LAN): try reminder.set resolving default loopback speaker
        Scripted::Calls {
            calls: vec![(
                "reminder.set".into(),
                json!({ "note": "喝水", "delay_minutes": 5 }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["done5".into()],
        },
        // Turn 6 (authenticated Bearer): reminder.set on dot2-d MUST SUCCEED
        Scripted::Calls {
            calls: vec![(
                "reminder.set".into(),
                json!({ "note": "喝水", "delay_minutes": 5 }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["done6".into()],
        },
        // Turn 7 (unauth LAN): read-only query on non-loopback LAN device MUST SUCCEED
        Scripted::Calls {
            calls: vec![(
                "system.volume_reading".into(),
                json!({ "device": ANDROID_ID }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["done7".into()],
        },
    ]));

    let state = build_state(&dir, Some(llm), None);
    state.auth.set_lan_gateway_exempt(true).unwrap();
    let token = state.auth.provision("admin-lan").unwrap();

    // Configure device metadata ("书房" -> dot2-d)
    state
        .config
        .save_device_meta(DOT2_ID, "书房音箱", "书房", "test")
        .unwrap();

    // Configure Home Assistant whitelist, OpenAPI Mutate op, and Master Brain
    state
        .config
        .save_home(
            HomeConfig {
                backend: HomeBackend::HomeAssistant,
                base_url: "http://127.0.0.1:9".into(),
                entities: vec![HomeEntity {
                    entity: "light.study".into(),
                    name: "书房灯".into(),
                }],
                ..Default::default()
            },
            "test",
        )
        .unwrap();
    state.config.save_home_token(Some("ha-tok")).unwrap();
    state
        .config
        .save_openapi(
            OpenApiConfig {
                enabled: true,
                base_url: "http://127.0.0.1:9".into(),
                spec_url: "http://127.0.0.1:9/openapi.json".into(),
                operations: vec![OpenApiOperation {
                    id: "toggle_alarm".into(),
                    method: "post".into(),
                    path: "/alarm".into(),
                    summary: "toggle".into(),
                    params: vec![],
                    effect_level: EffectLevel::Mutate,
                    verification: None,
                }],
                ..Default::default()
            },
            Some("oa-tok"),
            "test",
        )
        .unwrap();
    state
        .config
        .save_master_brain(
            MasterBrainConfig {
                enabled: true,
                protocol: MasterBrainProtocol::Hermes,
                url: "http://127.0.0.1:9".into(),
                ..Default::default()
            },
            Some("mb-tok"),
            "test",
        )
        .unwrap();

    // Connect ONLY loopback dot2-d so single-device default ALSO resolves to loopback speaker.
    let _gw_dot2 = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50002".parse().unwrap(),
        hello_frame(
            DOT2_ID,
            &["audio_in", "audio_out", "volume", "stop"],
            None,
            None,
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(state.hub.device_is_loopback_protected(DOT2_ID));

    let lan_peer: SocketAddr = "192.168.1.77:51000".parse().unwrap();
    let (base, _srv) = spawn_http_with_peer(state.clone(), lan_peer).await;

    let assert_tool_denied = |events: &[Value], tool_name: &str| {
        let hit = events.iter().find(|e| {
            e["type"] == "tool_result"
                && e["tool"] == tool_name
                && e["status"] == "failed"
                && e["summary"]
                    .as_str()
                    .is_some_and(|s| s.contains("[PAIRING_REQUIRED]"))
        });
        assert!(
            hit.is_some(),
            "expected {tool_name} to fail with PAIRING_REQUIRED under lan_gateway_exempt; events={events:?}"
        );
    };

    for (prompt, expected_tool) in [
        ("把书房音量调到20", "system.volume"),
        ("关掉书房灯", "home.set"),
        ("开关报警器", "openapi.call"),
        ("让主脑重启服务器", "brain.delegate"),
        ("5分钟后提醒我喝水", "reminder.set"),
    ] {
        let (status, events) = post_chat_sse(&base, None, prompt).await;
        assert_eq!(status, 200);
        assert_tool_denied(&events, expected_tool);
    }

    // Authenticated Bearer chat MUST succeed for reminder.set!
    let (status, events) = post_chat_sse(&base, Some(&token), "5分钟后提醒我喝水").await;
    assert_eq!(status, 200);
    let ok = events.iter().find(|e| {
        e["type"] == "tool_result" && e["tool"] == "reminder.set" && e["status"] == "completed"
    });
    assert!(
        ok.is_some(),
        "authenticated Bearer chat must be allowed to set reminders; events={events:?}"
    );

    // Non-loopback LAN device read-only tool call under unauthenticated LAN chat MUST succeed!
    let gw_android = MockGw::connect(
        state.hub.clone(),
        "192.168.1.88:50002".parse().unwrap(),
        hello_frame(ANDROID_ID, &["audio_out", "volume"], None, None),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!state.hub.device_is_loopback_protected(ANDROID_ID));
    gw_android.send(GatewayFrame::VolumeReceipt {
        task_id: "vol:lan:1".into(),
        device: Some(ANDROID_ID.into()),
        stage: "device_confirmed".into(),
        effective: 0.4,
        unit: "fraction".into(),
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (status, events) = post_chat_sse(&base, None, "查一下平板音量").await;
    assert_eq!(status, 200);
    let ok_lan = events.iter().find(|e| {
        e["type"] == "tool_result"
            && e["tool"] == "system.volume_reading"
            && e["status"] == "completed"
    });
    assert!(
        ok_lan.is_some(),
        "unauthenticated LAN chat must keep non-loopback read-only device queries working; events={events:?}"
    );
}

#[tokio::test]
async fn p1_02_config_delivery_rollback_clear_removes_duplex_requested_entry() {
    let dir = TempDir::new("p1-02-duplex-clear");
    let state = build_state(&dir, Some(Arc::new(FakeLlm::new(vec![]))), None);
    let mut gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50003".parse().unwrap(),
        hello_frame(
            DOT2_ID,
            &[
                "audio_in",
                "audio_out",
                "volume",
                "stop",
                "config_apply_v2",
                "config_block_duplex",
            ],
            None,
            None,
        ),
    )
    .await;
    let _gw_responder = tokio::spawn(async move {
        let mut rev = 1u64;
        while let Some(frame) = gw.rx.recv().await {
            match frame {
                CoreFrame::ConfigStateGet { request_id, device } => {
                    gw.send(GatewayFrame::ConfigState {
                        request_id: Some(request_id),
                        device,
                        gateway_boot_id: Some("boot-dup".into()),
                        state_revision: Some(rev),
                        capability_fingerprint: None,
                        blocks: vec![],
                        ts: hm_core::id::now_ms(),
                    });
                }
                CoreFrame::ConfigValidate {
                    request_id,
                    device,
                    block,
                    schema_version,
                    block_hash,
                    ..
                } => {
                    gw.send(GatewayFrame::ConfigValidated {
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
                        gateway_boot_id: Some("boot-dup".into()),
                        state_revision: Some(rev),
                        capability_fingerprint: None,
                        evidence: None,
                        ts: hm_core::id::now_ms(),
                    });
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
                    ..
                } => {
                    rev += 1;
                    gw.send(GatewayFrame::ConfigReceipt {
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
                        gateway_boot_id: Some("boot-dup".into()),
                        state_revision: Some(rev),
                        persisted_hash: Some(block_hash.clone()),
                        active_hash: Some(block_hash),
                        active_values: None,
                        evidence: None,
                        ts: hm_core::id::now_ms(),
                    });
                }
                _ => {}
            }
        }
    });
    let delivery = ConfigDelivery::for_app(&state);
    let enrolled = delivery.enroll(DOT2_ID).unwrap();
    let household = enrolled["household_id"].as_str().unwrap().to_string();
    let _worker = ConfigApplyCoordinator::start_for_app(&state);

    let set_pkg = Package {
        format: "home-mate-config".into(),
        version: 2,
        package_id: "11111111-1111-4111-8111-111111111111".into(),
        package_revision: 1,
        created_ms: hm_core::id::now_ms(),
        purpose: Purpose::NodeConfig,
        source: CfgSource {
            node_id: DOT2_ID.into(),
            config_revision: 1,
            core_version: hm_core::CORE_VERSION.into(),
            schema_version: 1,
        },
        target: Target {
            household_id: household.clone(),
            node_id: Some(DOT2_ID.into()),
        },
        compatibility: Compatibility {
            core_api_schema: 2,
            gateway_protocol_min: 0,
            gateway_protocol_max: 0,
            required_capabilities: vec![],
        },
        change_reason: "set full duplex".into(),
        blocks: vec![Block {
            name: "mode".into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: format!("mode.duplex_requested.{DOT2_ID}"),
                value: json!("full"),
            }],
        }],
        bindings: vec![],
        content_hash: None,
    };
    let plan1 = delivery.preflight("admin", set_pkg, json!({})).unwrap();
    let app1_id = "22222222-2222-4222-8222-222222222222";
    delivery
        .commit(
            "admin",
            &Commit {
                plan_id: plan1["plan_id"].as_str().unwrap().into(),
                plan_hash: plan1["plan_hash"].as_str().unwrap().into(),
                expected_revision: plan1["expected_revision"].as_u64().unwrap(),
                application_id: app1_id.into(),
            },
        )
        .unwrap();

    // Wait for publication to apply "full".
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while state.voice.requested_duplex(DOT2_ID) != "full" {
        assert!(
            tokio::time::Instant::now() < deadline,
            "full duplex not applied"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Now roll back app1 (which issues `Operation::Clear { path: "mode.duplex_requested.dot2-d" }`).
    let cur_rev = delivery.state("admin").unwrap()["config_revision"]
        .as_u64()
        .unwrap();
    let rb_plan = delivery
        .rollback_preflight("admin", app1_id, cur_rev)
        .unwrap();
    delivery
        .commit(
            "admin",
            &Commit {
                plan_id: rb_plan["plan_id"].as_str().unwrap().into(),
                plan_hash: rb_plan["plan_hash"].as_str().unwrap().into(),
                expected_revision: rb_plan["expected_revision"].as_u64().unwrap(),
                application_id: "33333333-3333-4333-8333-333333333333".into(),
            },
        )
        .unwrap();

    let deadline2 = tokio::time::Instant::now() + Duration::from_secs(3);
    while state.voice.requested_duplex(DOT2_ID) != "half" {
        assert!(
            tokio::time::Instant::now() < deadline2,
            "rollback Clear must remove mode.duplex_requested.{DOT2_ID} from in-memory VoiceSettings"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Verify SQLite persisted map also removed `dot2-d`!
    let raw = state.store.config_get("voice_mode").unwrap().unwrap();
    let persisted: Value = serde_json::from_str(&raw).unwrap();
    assert!(
        persisted["duplex_requested"].get(DOT2_ID).is_none(),
        "rollback Clear must remove {DOT2_ID} from SQLite voice_mode: {persisted}"
    );
}

#[tokio::test]
async fn p1_03_per_device_overrides_survive_global_config_delivery_and_retreat_words_wired() {
    let dir = TempDir::new("p1-03-overrides");
    let state = build_state(&dir, Some(Arc::new(FakeLlm::new(vec![]))), None);
    let mut gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50004".parse().unwrap(),
        hello_frame(
            DOT2_ID,
            &[
                "audio_in",
                "audio_out",
                "volume",
                "stop",
                "config_apply_v2",
                "config_block_kws",
            ],
            None,
            None,
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Set per-device overrides on dot2-d: custom threshold, score, wake_words, retreat_words.
    state
        .voice
        .set_mode_with_follow_window(
            DOT2_ID,
            None,
            None,
            Some(vec!["你好小花".into()]),
            None,
            None,
            None,
            None,
            Some(vec!["安静一下".into()]),
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
                kws_threshold: Some(0.42),
                kws_score: Some(2.6),
                ..Default::default()
            },
            "test",
        )
        .unwrap();

    // Verify mode_snapshot exposes per-device retreat_words inside devices[*].reflex.words.
    let snap = state.voice.mode_snapshot();
    let dev_entry = snap["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == DOT2_ID)
        .unwrap();
    let reflex_words = dev_entry["reflex"]["words"].as_array().unwrap();
    assert!(
        reflex_words
            .iter()
            .any(|w| w["text"] == "安静一下" && w["intent"] == "retreat"),
        "devices[*].reflex.words must reflect per-device retreat_words override: {reflex_words:?}"
    );
    assert!(
        reflex_words
            .iter()
            .any(|w| w["text"] == "你好小花" && w["intent"] == "wake"),
        "devices[*].reflex.words must reflect per-device wake_words override: {reflex_words:?}"
    );

    // Push a GLOBAL mode change via config_delivery changing global kws_threshold=0.25.
    // Gateway Kws payload for dot2-d MUST keep dot2-d's per-device override (0.42 / 2.6 / 你好小花 / 安静一下).
    let delivery = ConfigDelivery::for_app(&state);
    let enrolled = delivery.enroll(DOT2_ID).unwrap();
    let household = enrolled["household_id"].as_str().unwrap().to_string();
    let _worker = ConfigApplyCoordinator::start_for_app(&state);

    let pkg = Package {
        format: "home-mate-config".into(),
        version: 2,
        package_id: "44444444-4444-4444-8444-444444444444".into(),
        package_revision: 1,
        created_ms: hm_core::id::now_ms(),
        purpose: Purpose::NodeConfig,
        source: CfgSource {
            node_id: DOT2_ID.into(),
            config_revision: 1,
            core_version: hm_core::CORE_VERSION.into(),
            schema_version: 1,
        },
        target: Target {
            household_id: household,
            node_id: Some(DOT2_ID.into()),
        },
        compatibility: Compatibility {
            core_api_schema: 2,
            gateway_protocol_min: 0,
            gateway_protocol_max: 0,
            required_capabilities: vec![],
        },
        change_reason: "global kws threshold update".into(),
        blocks: vec![Block {
            name: "mode".into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: "mode.kws_threshold".into(),
                value: json!(0.18),
            }],
        }],
        bindings: vec![],
        content_hash: None,
    };
    let plan = delivery.preflight("admin", pkg, json!({})).unwrap();
    delivery
        .commit(
            "admin",
            &Commit {
                plan_id: plan["plan_id"].as_str().unwrap().into(),
                plan_hash: plan["plan_hash"].as_str().unwrap().into(),
                expected_revision: plan["expected_revision"].as_u64().unwrap(),
                application_id: "55555555-5555-4555-8555-555555555555".into(),
            },
        )
        .unwrap();

    let mut saw_validate_values: Option<Value> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < deadline {
        let Some(frame) = gw
            .recv_matching(Duration::from_millis(500), |f| {
                matches!(
                    f,
                    CoreFrame::ConfigStateGet { .. }
                        | CoreFrame::ConfigValidate { .. }
                        | CoreFrame::ConfigApply { .. }
                )
            })
            .await
        else {
            continue;
        };
        match frame {
            CoreFrame::ConfigStateGet { request_id, .. } => {
                gw.send(GatewayFrame::ConfigState {
                    request_id: Some(request_id),
                    device: DOT2_ID.into(),
                    gateway_boot_id: Some("boot-1".into()),
                    state_revision: Some(1),
                    capability_fingerprint: None,
                    blocks: vec![],
                    ts: hm_core::id::now_ms(),
                });
            }
            CoreFrame::ConfigValidate {
                request_id,
                block: ConfigBlock::Kws,
                block_hash,
                values,
                ..
            } => {
                saw_validate_values = Some(values);
                gw.send(GatewayFrame::ConfigValidated {
                    request_id,
                    device: DOT2_ID.into(),
                    block: ConfigBlock::Kws,
                    schema_version: 1,
                    block_hash,
                    ok: true,
                    supported: true,
                    blockers: vec![],
                    state: ConfigReceiptStatus::Validated,
                    reason: None,
                    retryable: false,
                    gateway_boot_id: Some("boot-1".into()),
                    state_revision: Some(1),
                    capability_fingerprint: None,
                    evidence: None,
                    ts: hm_core::id::now_ms(),
                });
                break;
            }
            _ => {}
        }
    }
    let vals = saw_validate_values.expect("expected ConfigValidate for ConfigBlock::Kws");
    assert!(
        (vals["threshold"].as_f64().unwrap() - 0.42).abs() < 1e-6,
        "global mode update must not clobber per-device kws_threshold: {vals}"
    );
    assert!(
        (vals["score"].as_f64().unwrap() - 2.6).abs() < 1e-6,
        "global mode update must not clobber per-device kws_score: {vals}"
    );
    let retreat_arr = vals["retreat_words"].as_array().unwrap();
    assert!(
        retreat_arr.iter().any(|w| w == "安静一下"),
        "KWS block must preserve per-device retreat_words: {vals}"
    );
}

#[tokio::test]
async fn p1_11_device_profile_led_push_respects_v2_capabilities() {
    let dir = TempDir::new("p1-11-led-v2");
    let state = build_state(&dir, Some(Arc::new(FakeLlm::new(vec![]))), None);
    let mut gw_v2_led = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50005".parse().unwrap(),
        hello_frame(
            DOT2_ID,
            &[
                "audio_in",
                "audio_out",
                "config_apply_v2",
                "config_block_led",
            ],
            None,
            None,
        ),
    )
    .await;
    let mut gw_v2_no_led = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50006".parse().unwrap(),
        hello_frame(
            ANDROID_ID,
            &["audio_in", "audio_out", "config_apply_v2"],
            None,
            Some("android-v2"),
        ),
    )
    .await;
    let mut gw_legacy = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50007".parse().unwrap(),
        hello_frame("dot2-legacy", &["audio_in", "audio_out"], None, None),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (base, _srv) =
        spawn_http_with_peer(state.clone(), "127.0.0.1:52000".parse().unwrap()).await;
    let client = reqwest::Client::new();

    // 1) V2 gateway WITH config_block_led -> dispatches via config_apply_v2 (ConfigApply(Led))
    let responder = tokio::spawn(async move {
        let mut saw_v2_apply = false;
        let mut saw_legacy_led = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        while tokio::time::Instant::now() < deadline {
            let Some(frame) = gw_v2_led
                .recv_matching(Duration::from_millis(400), |_| true)
                .await
            else {
                if saw_v2_apply {
                    break;
                }
                continue;
            };
            match frame {
                CoreFrame::LedConfig { .. } => saw_legacy_led = true,
                CoreFrame::ConfigStateGet { request_id, .. } => {
                    gw_v2_led.send(GatewayFrame::ConfigState {
                        request_id: Some(request_id),
                        device: DOT2_ID.into(),
                        gateway_boot_id: Some("boot-led".into()),
                        state_revision: Some(1),
                        capability_fingerprint: None,
                        blocks: vec![],
                        ts: hm_core::id::now_ms(),
                    });
                }
                CoreFrame::ConfigValidate {
                    request_id,
                    block,
                    block_hash,
                    ..
                } => {
                    gw_v2_led.send(GatewayFrame::ConfigValidated {
                        request_id,
                        device: DOT2_ID.into(),
                        block,
                        schema_version: 1,
                        block_hash,
                        ok: true,
                        supported: true,
                        blockers: vec![],
                        state: ConfigReceiptStatus::Validated,
                        reason: None,
                        retryable: false,
                        gateway_boot_id: Some("boot-led".into()),
                        state_revision: Some(1),
                        capability_fingerprint: None,
                        evidence: None,
                        ts: hm_core::id::now_ms(),
                    });
                }
                CoreFrame::ConfigApply {
                    core_node_id,
                    data_epoch,
                    application_id,
                    operation_id,
                    block,
                    desired_revision,
                    block_generation,
                    block_hash,
                    ..
                } => {
                    saw_v2_apply = true;
                    gw_v2_led.send(GatewayFrame::ConfigReceipt {
                        device: DOT2_ID.into(),
                        core_node_id,
                        data_epoch,
                        application_id,
                        operation_id,
                        desired_revision,
                        block_generation,
                        block,
                        schema_version: 1,
                        block_hash: block_hash.clone(),
                        state: ConfigReceiptStatus::Active,
                        reason: None,
                        retryable: false,
                        gateway_boot_id: Some("boot-led".into()),
                        state_revision: Some(2),
                        persisted_hash: Some(block_hash.clone()),
                        active_hash: Some(block_hash),
                        active_values: None,
                        evidence: None,
                        ts: hm_core::id::now_ms(),
                    });
                    break;
                }
                _ => {}
            }
        }
        (saw_v2_apply, saw_legacy_led)
    });

    let r1: Value = client
        .put(format!("{base}/api/v0/devices/{DOT2_ID}/profile"))
        .json(&json!({ "led": { "night_enabled": true, "night_brightness_pct": 15 } }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (saw_v2_apply, saw_legacy_led) = responder.await.unwrap();
    assert!(saw_v2_apply, "v2 LED gateway must receive ConfigApply(Led)");
    assert!(
        !saw_legacy_led,
        "v2 LED gateway must NOT receive legacy LedConfig"
    );
    assert_eq!(r1["led_pushed"], json!(true));

    // 2) V2 gateway WITHOUT config_block_led (Android) -> skips cleanly without sending frames
    let r2: Value = client
        .put(format!("{base}/api/v0/devices/{ANDROID_ID}/profile"))
        .json(&json!({ "led": { "night_enabled": true } }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r2["led_pushed"], json!(false));
    let unwanted = gw_v2_no_led
        .recv_matching(Duration::from_millis(200), |f| {
            matches!(
                f,
                CoreFrame::LedConfig { .. }
                    | CoreFrame::ConfigApply {
                        block: ConfigBlock::Led,
                        ..
                    }
            )
        })
        .await;
    assert!(
        unwanted.is_none(),
        "Android v2 without config_block_led must not receive LED frames: {unwanted:?}"
    );

    // 3) Legacy pre-v2 gateway -> uses legacy CoreFrame::LedConfig
    let r3: Value = client
        .put(format!("{base}/api/v0/devices/dot2-legacy/profile"))
        .json(&json!({ "led": { "night_enabled": true } }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r3["led_pushed"], json!(true));
    let legacy_led = gw_legacy
        .recv_matching(Duration::from_secs(1), |f| {
            matches!(f, CoreFrame::LedConfig { .. })
        })
        .await;
    assert!(
        legacy_led.is_some(),
        "pre-v2 gateway must still receive legacy CoreFrame::LedConfig"
    );
}

#[tokio::test]
async fn p1_05_and_p2_04_device_offline_clears_inflight_mic_capture_and_learning_wait() {
    let dir = TempDir::new("p1-05-offline");
    let state = build_state(&dir, Some(Arc::new(FakeLlm::new(vec![]))), None);
    let gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50008".parse().unwrap(),
        hello_frame(DOT2_ID, &["audio_in", "audio_out"], None, None),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    state.voice.start_mic_capture(DOT2_ID, 10).unwrap();
    assert_eq!(
        state.voice.mic_capture_status(DOT2_ID)["state"],
        json!("recording")
    );
    let now = hm_core::id::now_ms();
    state.learning.sync_wait_from_response_for(
        Some(DOT2_ID),
        &json!({ "status": "active", "session_id": "s-1", "attempt": { "id": "a-1" } }),
        now,
    );
    assert_eq!(
        state.learning.active_owner_device(now).as_deref(),
        Some(DOT2_ID)
    );

    gw.disconnect();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let cap_idle = state.voice.mic_capture_status(DOT2_ID)["state"] == json!("idle");
        let owner_cleared = state
            .learning
            .active_owner_device(hm_core::id::now_ms())
            .is_none();
        if cap_idle && owner_cleared {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "on_device_offline must abort in-flight mic_capture and clear learning ownership (cap_idle={cap_idle}, owner_cleared={owner_cleared})"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[test]
fn p1_06_fresh_wake_cut_cursor_survives_idle_gap_reset() {
    let mut mic = MicProcessor::new(VadConfig {
        start_db: -40.0,
        end_db: -45.0,
        start_blocks: 2,
        end_blocks: 4,
        min_blocks: 2,
        max_blocks: 50,
        ..Default::default()
    });
    let quiet = vec![0i16; 1280];
    let loud = vec![9000i16; 1280];
    let t0 = Instant::now();
    let _ = mic.feed(quiet.clone(), 16_000);
    let cursor = mic.current_block_cursor();
    mic.arm_wake_slice(cursor + 3);
    // Idle gap >= IDLE_GAP_RESET_MS (1 s) but < 5 s: freshly armed cursor MUST survive.
    let _ = mic.reset_stale_preroll_if_gap(t0 + Duration::from_millis(1_000));
    let mut closed = None;
    for _ in 0..3 {
        let _ = mic.feed(loud.clone(), 16_000);
    }
    for _ in 0..8 {
        if let FeedOutcome::Segment(Some(seg)) = mic.feed(quiet.clone(), 16_000) {
            closed = Some(seg);
            break;
        }
    }
    assert_eq!(
        closed.expect("expected segment to close").wake_slice,
        WakeSliceOutcome::BareWake,
        "freshly armed wake_cut_cursor (< 5 s) must survive IDLE_GAP_RESET_MS"
    );

    // Stale wake_cut_cursor (> 5 s) MUST be cleared on idle gap reset.
    mic.arm_wake_slice(mic.current_block_cursor() + 3);
    let _ = mic.reset_stale_preroll_if_gap(Instant::now() + Duration::from_secs(6));
    let mut closed2 = None;
    for _ in 0..3 {
        let _ = mic.feed(loud.clone(), 16_000);
    }
    for _ in 0..8 {
        if let FeedOutcome::Segment(Some(seg)) = mic.feed(quiet.clone(), 16_000) {
            closed2 = Some(seg);
            break;
        }
    }
    assert_eq!(
        closed2.expect("second segment").wake_slice,
        WakeSliceOutcome::None,
        "stale wake_cut_cursor (>= 5 s) must be cleared on idle gap reset"
    );
}

#[tokio::test]
async fn p1_08_play_backpressure_mid_wav_emits_control_stop() {
    let hub = DeviceHub::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hub_srv = hub.clone();
    let _srv = tokio::spawn(async move {
        if let Ok((stream, peer)) = listener.accept().await {
            let _ = hub_srv.serve_connection_from(stream, Some(peer)).await;
        }
    });
    let stream = TcpStream::connect(addr).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let hello =
        serde_json::to_string(&hello_frame(DOT2_ID, &["audio_out", "stop"], None, None)).unwrap();
    wr.write_all(hello.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();

    let mut lines = BufReader::new(rd).lines();
    let first = lines.next_line().await.unwrap().unwrap();
    assert!(
        first.contains("hello_ack"),
        "expected hello_ack, got {first}"
    );

    // Submit a burst WAV exceeding the outbound Voice tier queue so PlayBegin queues
    // and a mid-WAV PlayChunk hits Full -> triggers cleanup(true) -> CoreFrame::Stop on Control!
    let big_pcm = vec![0u8; 8192 * 600];
    let big_wav = hm_core::speech::pcm_to_wav(&big_pcm, 16_000, 1);
    let err = hub
        .play(PlayRequest {
            device: DOT2_ID.into(),
            content_ref: "test:big".into(),
            audio: Some(big_wav),
            audio_meta: AudioMeta {
                format: "wav".into(),
                sample_rate: 16_000,
                channels: 1,
            },
            channel: "voice".into(),
            text: None,
            simulated_ms: None,
        })
        .err()
        .expect("oversized burst WAV must hit outbound Voice backpressure");
    assert!(
        err.contains("full") || err.contains("backpressure"),
        "err={err}"
    );

    let mut saw_stop = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                if let Ok(CoreFrame::Stop { flush: true, .. }) = serde_json::from_str(&line) {
                    saw_stop = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        saw_stop,
        "mid-WAV backpressure after PlayBegin must push CoreFrame::Stop onto Control tier"
    );
}

#[tokio::test]
async fn p1_13_management_online_priority_and_reminder_ambiguous_device_code() {
    let dir = TempDir::new("p1-13-multi");
    let state = build_state(&dir, Some(Arc::new(FakeLlm::new(vec![]))), None);

    state
        .config
        .save_device_meta("dot2-a", "旧音箱", "客厅", "test")
        .unwrap();
    state
        .config
        .save_device_meta("dot2-b", "新音箱", "客厅", "test")
        .unwrap();

    let gw_a = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50009".parse().unwrap(),
        hello_frame("dot2-a", &["audio_out", "volume"], None, None),
    )
    .await;
    let gw_b = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50010".parse().unwrap(),
        hello_frame("dot2-b", &["audio_out", "volume"], None, None),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 1) While BOTH dot2-a and dot2-b are online, unbound reminder.set / list / cancel
    //    must return AMBIGUOUS_DEVICE (retryable: false), NOT REMINDER_UNAVAILABLE (retryable: true).
    let ctx = ToolCtx::new("t1", "c1", Arc::new(AtomicBool::new(false)));
    for (tool, args) in [
        (
            "reminder.set",
            json!({ "note": "关火", "delay_minutes": 5 }),
        ),
        ("reminder.list", json!({})),
        ("reminder.cancel", json!({ "all": true })),
    ] {
        let res = state.registry.call(tool, args, &ctx).await;
        assert_eq!(res.status, ToolStatus::Failed, "{tool} should fail");
        let err = res.error.expect("error present");
        assert_eq!(err.code, "AMBIGUOUS_DEVICE", "{tool} wrong code: {err:?}");
        assert!(
            !err.retryable,
            "{tool} AMBIGUOUS_DEVICE must not be retryable"
        );
    }

    // 2) Disconnect dot2-a so only dot2-b is online in "客厅", send volume receipt on dot2-b.
    gw_a.disconnect();
    tokio::time::sleep(Duration::from_millis(120)).await;
    gw_b.send(GatewayFrame::VolumeReceipt {
        task_id: "vol:boot:1".into(),
        device: Some("dot2-b".into()),
        stage: "device_confirmed".into(),
        effective: 0.55,
        unit: "fraction".into(),
    });
    tokio::time::sleep(Duration::from_millis(60)).await;

    let res = state
        .registry
        .call("system.volume_reading", json!({ "device": "客厅" }), &ctx)
        .await;
    assert_eq!(
        res.status,
        ToolStatus::Completed,
        "room resolution must prioritize online dot2-b over offline dot2-a: {res:?}"
    );
    assert_eq!(res.result.unwrap()["device"], json!("dot2-b"));
}

#[tokio::test]
async fn p2_07_exact_unit_gate_allows_registered_hub_device_on_nas_core_and_blocks_foreign() {
    let dir = TempDir::new("p2-07-nas-unit");
    let state = build_state(&dir, Some(Arc::new(FakeLlm::new(vec![]))), None);
    let _gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50011".parse().unwrap(),
        hello_frame("dot2-b", &["audio_out", "config_apply_v2"], None, None),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Enroll Core as NAS node "nas-core" (different from speaker stable_id "dot2-b").
    let delivery = ConfigDelivery::for_app(&state);
    let enrolled = delivery.enroll("nas-core").unwrap();
    let household = enrolled["household_id"].as_str().unwrap().to_string();

    let mk_pkg = |purpose: Purpose, target_node: Option<&str>, path: String, val: Value| Package {
        format: "home-mate-config".into(),
        version: 2,
        package_id: "66666666-6666-4666-8666-666666666666".into(),
        package_revision: 1,
        created_ms: hm_core::id::now_ms(),
        purpose,
        source: CfgSource {
            node_id: "nas-core".into(),
            config_revision: 1,
            core_version: hm_core::CORE_VERSION.into(),
            schema_version: 1,
        },
        target: Target {
            household_id: household.clone(),
            node_id: target_node.map(str::to_string),
        },
        compatibility: Compatibility {
            core_api_schema: 2,
            gateway_protocol_min: 0,
            gateway_protocol_max: 0,
            required_capabilities: vec![],
        },
        change_reason: "unit profile update".into(),
        blocks: vec![Block {
            name: path.split('.').next().unwrap().into(),
            schema_version: 1,
            operations: vec![Operation::Set { path, value: val }],
        }],
        bindings: vec![],
        content_hash: None,
    };

    // 1) NodeConfig targeting registered Hub device "dot2-b" acoustics -> ALLOWED!
    let ok_pkg = mk_pkg(
        Purpose::NodeConfig,
        Some("nas-core"),
        "unit_acoustic_profiles.dot2-b.volume_cap".into(),
        json!(0.85),
    );
    assert!(
        delivery.preflight("admin", ok_pkg, json!({})).is_ok(),
        "NAS Core must accept 1:1 NodeConfig acoustics package for registered Hub device dot2-b"
    );

    // 2) NodeConfig targeting unregistered foreign device "dot2-foreign" -> FORBIDDEN_SCOPE!
    let foreign_pkg = mk_pkg(
        Purpose::NodeConfig,
        Some("nas-core"),
        "unit_acoustic_profiles.dot2-foreign.volume_cap".into(),
        json!(0.85),
    );
    let err = delivery
        .preflight("admin", foreign_pkg, json!({}))
        .unwrap_err();
    assert_eq!(err.code.as_ref(), "FORBIDDEN_SCOPE");

    // 3) FamilyPolicy targeting unit acoustics -> STILL FORBIDDEN_SCOPE!
    let fam_pkg = mk_pkg(
        Purpose::FamilyPolicy,
        None,
        "unit_acoustic_profiles.dot2-b.volume_cap".into(),
        json!(0.85),
    );
    let err_fam = delivery.preflight("admin", fam_pkg, json!({})).unwrap_err();
    assert_eq!(err_fam.code.as_ref(), "FORBIDDEN_SCOPE");
}

#[tokio::test]
async fn p2_08_mode_put_spoken_partial_patch_preserves_custom_word_lists() {
    let dir = TempDir::new("p2-08-spoken-patch");
    let state = build_state(&dir, Some(Arc::new(FakeLlm::new(vec![]))), None);
    let (base, _srv) =
        spawn_http_with_peer(state.clone(), "127.0.0.1:52001".parse().unwrap()).await;
    let client = reqwest::Client::new();

    // Save custom detail_words and long_form_words.
    let resp1 = client
        .put(format!("{base}/api/v0/mode"))
        .json(&json!({
            "spoken": {
                "max_sentences": 2,
                "max_chars": 120,
                "detail_words": ["定制展开词"],
                "long_form_words": ["定制长文词"]
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status().as_u16(), 200);

    // Send a partial update omitting detail_words and long_form_words.
    let resp2 = client
        .put(format!("{base}/api/v0/mode"))
        .json(&json!({
            "spoken": {
                "max_sentences": 3
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status().as_u16(), 200);

    let snap = state.voice.settings_snapshot();
    assert_eq!(snap.spoken.max_sentences, 3);
    assert_eq!(
        snap.spoken.detail_words,
        vec!["定制展开词".to_string()],
        "omitting detail_words in PUT /api/v0/mode must preserve stored list"
    );
    assert_eq!(
        snap.spoken.long_form_words,
        vec!["定制长文词".to_string()],
        "omitting long_form_words in PUT /api/v0/mode must preserve stored list"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocker_1_device_audio_sync_wait_processed_no_lost_wakeup_race() {
    use hm_core::gateway::DeviceAudioSync;

    // Hammer concurrent `wait_processed` vs `mark_processed` across 200 tight
    // iterations on a multi-threaded runtime to catch any lost `Notify` wakeup.
    for iter in 1..=200u64 {
        let sync = DeviceAudioSync::new();
        let sync_w = sync.clone();
        let waiter = tokio::spawn(async move {
            sync_w.wait_processed(iter).await;
        });
        let notifier = tokio::spawn(async move {
            tokio::task::yield_now().await;
            sync.mark_processed(iter);
        });
        tokio::time::timeout(Duration::from_millis(500), async {
            waiter.await.unwrap();
            notifier.await.unwrap();
        })
        .await
        .unwrap_or_else(|_| panic!("lost wakeup in wait_processed/mark_processed at iter {iter}"));
    }

    // Hammer concurrent `wait_processed` vs `mark_closed` across 200 iterations.
    for iter in 1..=200u64 {
        let sync = DeviceAudioSync::new();
        let sync_w = sync.clone();
        let waiter = tokio::spawn(async move {
            sync_w.wait_processed(999).await;
        });
        let closer = tokio::spawn(async move {
            tokio::task::yield_now().await;
            sync.mark_closed();
        });
        tokio::time::timeout(Duration::from_millis(500), async {
            waiter.await.unwrap();
            closer.await.unwrap();
        })
        .await
        .unwrap_or_else(|_| panic!("lost wakeup in wait_processed/mark_closed at iter {iter}"));
    }
}

#[tokio::test]
async fn blocker_2_open_socket_mutate_blocked_under_lan_exempt_while_query_allowed() {
    use axum::routing::{get, post};
    use hm_core::execution::Authority;
    use hm_core::tool::open_socket::{HttpMethod, OpenSocketContract};
    use hm_core::tool::SideEffect;

    let mutate_hits = Arc::new(AtomicUsize::new(0));
    let query_hits = Arc::new(AtomicUsize::new(0));
    let m_hits = mutate_hits.clone();
    let q_hits = query_hits.clone();
    let stub = axum::Router::new()
        .route(
            "/unlock",
            post(move || {
                let m_hits = m_hits.clone();
                async move {
                    m_hits.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({ "confirmed": true, "value": "unlocked" }))
                }
            }),
        )
        .route(
            "/sensor",
            get(move || {
                let q_hits = q_hits.clone();
                async move {
                    q_hits.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({ "temp": 23.5 }))
                }
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stub_addr = listener.local_addr().unwrap();
    let _stub_srv = tokio::spawn(async move {
        let _ = axum::serve(listener, stub).await;
    });

    let dir = TempDir::new("blocker-2-open-socket");
    let state = build_state(&dir, Some(Arc::new(FakeLlm::new(vec![]))), None);

    state
        .open_sockets
        .register(
            &state.registry,
            OpenSocketContract {
                name: "custom.door_unlock".into(),
                service: "open_socket".into(),
                description: "Unlock front door".into(),
                parameters: json!({ "type": "object", "properties": {} }),
                effect: SideEffect::Mutate,
                endpoint_url: format!("http://{stub_addr}/unlock"),
                method: HttpMethod::POST,
                enabled: true,
                timeout_ms: 5_000,
                auth_header: String::new(),
                default_subject: "前门".into(),
                default_action: "unlock".into(),
            },
            None,
            "test",
        )
        .unwrap();
    state
        .open_sockets
        .register(
            &state.registry,
            OpenSocketContract {
                name: "custom.sensor_read".into(),
                service: "open_socket".into(),
                description: "Read room sensor".into(),
                parameters: json!({ "type": "object", "properties": {} }),
                effect: SideEffect::Query,
                endpoint_url: format!("http://{stub_addr}/sensor"),
                method: HttpMethod::GET,
                enabled: true,
                timeout_ms: 5_000,
                auth_header: String::new(),
                default_subject: "传感器".into(),
                default_action: "query".into(),
            },
            None,
            "test",
        )
        .unwrap();

    let lan_ctx = ToolCtx::new("t-lan", "c-lan", Arc::new(AtomicBool::new(false))).with_authority(
        Authority::anonymous(state.store.clone(), None)
            .unwrap()
            .with_lan_exempt(true),
    );
    let denied = state
        .registry
        .call("custom.door_unlock", json!({}), &lan_ctx)
        .await;
    assert_eq!(denied.status, ToolStatus::Failed);
    let err = denied.error.expect("expected PAIRING_REQUIRED error");
    assert_eq!(err.code, "PAIRING_REQUIRED");
    assert!(!err.retryable);
    assert_eq!(
        mutate_hits.load(Ordering::SeqCst),
        0,
        "mutating open socket MUST NOT issue HTTP request under lan_exempt"
    );

    // Read-only Query open socket stays allowed under lan_exempt.
    let ok_query = state
        .registry
        .call("custom.sensor_read", json!({}), &lan_ctx)
        .await;
    assert_eq!(
        ok_query.status,
        ToolStatus::Completed,
        "read-only Query open socket should remain allowed under lan_exempt: {ok_query:?}"
    );
    assert_eq!(query_hits.load(Ordering::SeqCst), 1);

    // Authenticated caller (lan_exempt == false) CAN execute mutating open socket.
    let auth_ctx = ToolCtx::new("t-auth", "c-auth", Arc::new(AtomicBool::new(false)))
        .with_authority(Authority::anonymous(state.store.clone(), Some("admin".into())).unwrap());
    let ok_mut = state
        .registry
        .call("custom.door_unlock", json!({}), &auth_ctx)
        .await;
    assert_eq!(
        ok_mut.status,
        ToolStatus::Completed,
        "authenticated caller must be allowed to call mutating open socket: {ok_mut:?}"
    );
    assert_eq!(mutate_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn blocker_3_learning_fails_closed_under_lan_exempt_and_offline_loopback_device() {
    use axum::routing::get;
    use hm_core::config::{PersonalEndpointConfig, LEARNING_KEY, LEARNING_TOKEN_SECRET};
    use hm_core::execution::Authority;
    use hm_core::tool::learning::{
        LearningAnswerTool, LearningControlTool, LearningCurrentTool, LearningDecksTool,
        LearningRevealTool, LearningRuntime, LearningStartTool,
    };
    use hm_core::tool::Tool;

    let hits = Arc::new(AtomicUsize::new(0));
    let hits_g = hits.clone();
    let hits_p = hits.clone();
    let stub = axum::Router::new().route(
        "/api/v1/learning/{*rest}",
        get(move || {
            let hits_g = hits_g.clone();
            async move {
                hits_g.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({ "status": "idle", "decks": [] }))
            }
        })
        .post(move || {
            let hits_p = hits_p.clone();
            async move {
                hits_p.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({ "status": "idle" }))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stub_addr = listener.local_addr().unwrap();
    let _stub_srv = tokio::spawn(async move {
        let _ = axum::serve(listener, stub).await;
    });

    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let cfg = PersonalEndpointConfig {
        enabled: true,
        url: format!("http://{stub_addr}"),
        timeout_ms: 5_000,
    };
    store
        .config_set(LEARNING_KEY, &serde_json::to_string(&cfg).unwrap())
        .unwrap();
    store
        .secret_set(LEARNING_TOKEN_SECRET, "learn-tok")
        .unwrap();

    let lan_ctx = |dev: Option<&str>| {
        ToolCtx::new("t-lan", "c-lan", Arc::new(AtomicBool::new(false)))
            .with_provenance(dev.map(str::to_string), None)
            .with_authority(
                Authority::anonymous(store.clone(), None)
                    .unwrap()
                    .with_lan_exempt(true),
            )
    };

    // 1) hub == None -> EVERY Learning*Tool must fail closed with PAIRING_REQUIRED.
    let rt_no_hub = LearningRuntime::new(store.clone());
    let tools_no_hub: Vec<Box<dyn Tool>> = vec![
        Box::new(LearningDecksTool::new(rt_no_hub.clone())),
        Box::new(LearningStartTool::new(rt_no_hub.clone())),
        Box::new(LearningCurrentTool::new(rt_no_hub.clone())),
        Box::new(LearningAnswerTool::new(rt_no_hub.clone())),
        Box::new(LearningRevealTool::new(rt_no_hub.clone())),
        Box::new(LearningControlTool::new(rt_no_hub)),
    ];
    for tool in &tools_no_hub {
        let res = tool
            .execute(json!({ "answer": "a", "action": "pause" }), &lan_ctx(None))
            .await;
        assert_eq!(
            res.status,
            ToolStatus::Failed,
            "{} must fail closed when hub is None",
            tool.descriptor().name
        );
        assert_eq!(
            res.error.as_ref().map(|e| e.code.as_str()),
            Some("PAIRING_REQUIRED"),
            "{} must return PAIRING_REQUIRED when hub is None",
            tool.descriptor().name
        );
    }

    // 2) hub present, no device connected (device_id == None) & unknown offline device -> fail closed.
    let dir = TempDir::new("blocker-3-learning");
    let state = build_state(&dir, Some(Arc::new(FakeLlm::new(vec![]))), None);
    state.auth.set_lan_gateway_exempt(true).unwrap();
    let _token = state.auth.provision("admin-lan").unwrap();
    let hub = state.hub.clone();
    assert!(
        !hub.device_is_known_lan_device("dot2-offline"),
        "unknown/offline device must fail closed in device_is_known_lan_device"
    );
    let rt_with_hub = LearningRuntime::new(store.clone()).with_hub(hub.clone());
    let current_tool = LearningCurrentTool::new(rt_with_hub.clone());
    for dev in [None, Some("dot2-offline")] {
        let res = current_tool.execute(json!({}), &lan_ctx(dev)).await;
        assert_eq!(res.status, ToolStatus::Failed);
        assert_eq!(
            res.error.as_ref().map(|e| e.code.as_str()),
            Some("PAIRING_REQUIRED"),
            "learning.current with device={dev:?} must fail closed under lan_exempt"
        );
    }

    // Also verify reminder.set fails closed with PAIRING_REQUIRED when targeting
    // an offline/unregistered speaker before its local gateway reconnects.
    let state_lan_ctx = ToolCtx::new("t-lan-s", "c-lan-s", Arc::new(AtomicBool::new(false)))
        .with_provenance(Some("dot2-offline".into()), None)
        .with_authority(
            Authority::anonymous(state.store.clone(), None)
                .unwrap()
                .with_lan_exempt(true),
        );
    let rem_res = state
        .registry
        .call(
            "reminder.set",
            json!({ "note": "离线测试", "delay_minutes": 5 }),
            &state_lan_ctx,
        )
        .await;
    assert_eq!(rem_res.status, ToolStatus::Failed);
    assert_eq!(
        rem_res.error.as_ref().map(|e| e.code.as_str()),
        Some("PAIRING_REQUIRED"),
        "reminder.set must fail closed with PAIRING_REQUIRED for offline/unregistered speaker"
    );

    // 3) Loopback speaker connected -> fail closed.
    let _gw_dot2 = MockGw::connect(
        hub.clone(),
        "127.0.0.1:50021".parse().unwrap(),
        hello_frame(DOT2_ID, &["audio_in", "audio_out"], None, None),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    let res_dot2 = current_tool
        .execute(json!({}), &lan_ctx(Some(DOT2_ID)))
        .await;
    assert_eq!(res_dot2.status, ToolStatus::Failed);
    assert_eq!(
        res_dot2.error.as_ref().map(|e| e.code.as_str()),
        Some("PAIRING_REQUIRED")
    );

    // 4) Non-loopback LAN device connected:
    //    - supplying `binding_id` STILL fails closed with PAIRING_REQUIRED (no network call)
    //    - omitting `binding_id` on the non-loopback LAN device succeeds.
    let _gw_android = MockGw::connect(
        hub.clone(),
        "192.168.1.91:50022".parse().unwrap(),
        hello_frame(ANDROID_ID, &["audio_in", "audio_out"], None, None),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(!hub.device_is_loopback_protected(ANDROID_ID));

    let res_binding = current_tool
        .execute(
            json!({ "binding_id": "11111111-1111-4111-8111-111111111111" }),
            &lan_ctx(Some(ANDROID_ID)),
        )
        .await;
    assert_eq!(res_binding.status, ToolStatus::Failed);
    assert_eq!(
        res_binding.error.as_ref().map(|e| e.code.as_str()),
        Some("PAIRING_REQUIRED"),
        "binding_id must be rejected under lan_exempt even on a non-loopback LAN device"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "all rejected learning calls must issue zero outbound requests"
    );

    let res_android = current_tool
        .execute(json!({}), &lan_ctx(Some(ANDROID_ID)))
        .await;
    assert_eq!(
        res_android.status,
        ToolStatus::Completed,
        "non-loopback LAN device speaker-route learning call must remain allowed: {res_android:?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn blocker_4_brain_status_and_all_brain_tools_blocked_under_lan_exempt() {
    use axum::routing::get;
    use hm_core::execution::Authority;

    let hits = Arc::new(AtomicUsize::new(0));
    let hits_c = hits.clone();
    let stub = axum::Router::new().route(
        "/v1/{*rest}",
        get({
            let hits_c = hits_c.clone();
            move || {
                let hits_c = hits_c.clone();
                async move {
                    hits_c.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({ "status": "running" }))
                }
            }
        })
        .post(move || {
            let hits_c = hits_c.clone();
            async move {
                hits_c.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({ "run_id": "run-1", "status": "running" }))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stub_addr = listener.local_addr().unwrap();
    let _stub_srv = tokio::spawn(async move {
        let _ = axum::serve(listener, stub).await;
    });

    let dir = TempDir::new("blocker-4-brain-status");
    let llm = Arc::new(FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "brain.status".into(),
                json!({ "task_id": "run-1" }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["denied".into()],
        },
    ]));
    let state = build_state(&dir, Some(llm), None);
    state.auth.set_lan_gateway_exempt(true).unwrap();
    let _token = state.auth.provision("admin-lan").unwrap();
    state
        .config
        .save_master_brain(
            MasterBrainConfig {
                enabled: true,
                protocol: MasterBrainProtocol::Hermes,
                url: format!("http://{stub_addr}"),
                ..Default::default()
            },
            Some("mb-tok"),
            "test",
        )
        .unwrap();

    // Direct registry checks for all four Master Brain tools under lan_exempt.
    let lan_ctx = ToolCtx::new("t-lan", "c-lan", Arc::new(AtomicBool::new(false))).with_authority(
        Authority::anonymous(state.store.clone(), None)
            .unwrap()
            .with_lan_exempt(true),
    );
    for (tool, args) in [
        ("brain.delegate", json!({ "task": "list files" })),
        ("brain.status", json!({ "task_id": "run-1" })),
        (
            "brain.steer",
            json!({ "task_id": "run-1", "guidance": "ok" }),
        ),
        ("brain.cancel", json!({ "task_id": "run-1" })),
    ] {
        let res = state.registry.call(tool, args, &lan_ctx).await;
        assert_eq!(
            res.status,
            ToolStatus::Failed,
            "{tool} must fail under lan_exempt"
        );
        let err = res.error.expect("PAIRING_REQUIRED error");
        assert_eq!(err.code, "PAIRING_REQUIRED", "{tool} returned {err:?}");
        assert!(!err.retryable);
    }

    // End-to-end HTTP POST /api/v0/chat from private LAN peer invoking brain.status.
    let (base, _srv) =
        spawn_http_with_peer(state.clone(), "192.168.1.77:51004".parse().unwrap()).await;
    let (status, events) = post_chat_sse(&base, None, "查一下主脑任务状态").await;
    assert_eq!(status, 200);
    let denied = events.iter().find(|e| {
        e["type"] == "tool_result"
            && e["tool"] == "brain.status"
            && e["status"] == "failed"
            && e["summary"]
                .as_str()
                .is_some_and(|s| s.contains("[PAIRING_REQUIRED]"))
    });
    assert!(
        denied.is_some(),
        "brain.status over unauthenticated LAN chat must fail with PAIRING_REQUIRED; events={events:?}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "rejected Master Brain tools must never contact the Master Brain HTTP server"
    );
}

#[tokio::test]
async fn p1_1_unauthenticated_lan_gateway_voice_turn_propagates_lan_exempt() {
    use axum::routing::post;

    let ha_hits = Arc::new(AtomicUsize::new(0));
    let ha_hits_c = ha_hits.clone();
    let ha_stub = axum::Router::new().route(
        "/api/services/light/turn_off",
        post(move || {
            let ha_hits_c = ha_hits_c.clone();
            async move {
                ha_hits_c.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!([{ "entity_id": "light.study", "state": "off" }]))
            }
        }),
    );
    let ha_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ha_addr = ha_listener.local_addr().unwrap();
    let _ha_srv = tokio::spawn(async move {
        let _ = axum::serve(ha_listener, ha_stub).await;
    });

    let mb_hits = Arc::new(AtomicUsize::new(0));
    let mb_hits_c = mb_hits.clone();
    let mb_stub = axum::Router::new().route(
        "/v1/{*rest}",
        post(move || {
            let mb_hits_c = mb_hits_c.clone();
            async move {
                mb_hits_c.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({ "run_id": "run-gw-1", "status": "running" }))
            }
        }),
    );
    let mb_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mb_addr = mb_listener.local_addr().unwrap();
    let _mb_srv = tokio::spawn(async move {
        let _ = axum::serve(mb_listener, mb_stub).await;
    });

    let dir = TempDir::new("p1-1-gateway-lan-exempt");
    let llm = Arc::new(FakeLlm::new(vec![
        // Turn 1: Unauthenticated LAN :8932 gateway attempts home.set, brain.delegate,
        // and cross-targeting the local loopback speaker (dot2-d) via system.volume.
        Scripted::Calls {
            calls: vec![
                (
                    "home.set".into(),
                    json!({ "entity": "书房灯", "state": "off" }).to_string(),
                ),
                (
                    "brain.delegate".into(),
                    json!({ "task": "reboot server" }).to_string(),
                ),
                (
                    "system.volume".into(),
                    json!({ "percent": 20, "device": "书房" }).to_string(),
                ),
            ],
        },
        Scripted::Text {
            chunks: vec!["需要配对后才能执行这些操作。".into()],
        },
        // Turn 2: Authenticated LAN :8932 gateway invokes home.set -> MUST SUCCEED.
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                json!({ "entity": "书房灯", "state": "off" }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["好的。".into()],
        },
        // Turn 3: Local loopback speaker invokes home.set -> MUST SUCCEED.
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                json!({ "entity": "书房灯", "state": "off" }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["好的。".into()],
        },
    ]));

    let state = build_state(&dir, Some(llm), None);
    state.auth.set_lan_gateway_exempt(true).unwrap();
    let token = state.auth.provision("admin-lan").unwrap();

    state
        .config
        .save_device_meta(DOT2_ID, "书房音箱", "书房", "test")
        .unwrap();
    state
        .config
        .save_home(
            HomeConfig {
                backend: HomeBackend::HomeAssistant,
                base_url: format!("http://{ha_addr}"),
                entities: vec![HomeEntity {
                    entity: "light.study".into(),
                    name: "书房灯".into(),
                }],
                ..Default::default()
            },
            "test",
        )
        .unwrap();
    state.config.save_home_token(Some("ha-tok")).unwrap();
    state
        .config
        .save_master_brain(
            MasterBrainConfig {
                enabled: true,
                protocol: MasterBrainProtocol::Hermes,
                url: format!("http://{mb_addr}"),
                ..Default::default()
            },
            Some("mb-tok"),
            "test",
        )
        .unwrap();

    // 1. Connect local loopback speaker (dot2-d) -> lan_exempt = false.
    let mut gw_dot2 = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50101".parse().unwrap(),
        hello_frame(
            DOT2_ID,
            &["audio_in", "audio_out", "volume", "stop"],
            None,
            None,
        ),
    )
    .await;
    // 2. Connect unauthenticated private-LAN :8932 gateway (auth_token: None) -> lan_exempt = true.
    let unauth_id = "android-unauth-gw";
    let gw_unauth = MockGw::connect(
        state.hub.clone(),
        "192.168.1.88:50102".parse().unwrap(),
        hello_frame(
            unauth_id,
            &["audio_in", "audio_out", "volume", "stop"],
            None,
            Some("android-v2"),
        ),
    )
    .await;
    // 3. Connect authenticated private-LAN :8932 gateway (auth_token: Some(token)) -> lan_exempt = false.
    let auth_id = "android-auth-gw";
    let gw_auth = MockGw::connect(
        state.hub.clone(),
        "192.168.1.89:50103".parse().unwrap(),
        hello_frame(
            auth_id,
            &["audio_in", "audio_out", "volume", "stop"],
            Some(&token),
            Some("android-v2"),
        ),
    )
    .await;

    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(!state.hub.is_device_lan_exempt(DOT2_ID));
    assert!(
        state.hub.is_device_lan_exempt(unauth_id),
        "unauthenticated private-LAN :8932 gateway must report is_device_lan_exempt=true"
    );
    assert!(
        !state.hub.is_device_lan_exempt(auth_id),
        "authenticated private-LAN :8932 gateway must report is_device_lan_exempt=false"
    );

    let wait_for_traces = |expected: usize| {
        let traces = state.traces.clone();
        async move {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let list = traces.list(20).unwrap_or_default();
                let done = list
                    .iter()
                    .filter(|t| t["status"].as_str() == Some("completed"))
                    .count();
                if done >= expected {
                    return list;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {expected} completed trace(s); traces={list:?}"
                );
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        }
    };

    // --- Turn 1: Unauthenticated LAN :8932 gateway voice turn ---
    gw_unauth.send(GatewayFrame::Wake {
        device: unauth_id.into(),
        origin: InputOrigin::Legacy,
        ts: hm_core::id::now_ms(),
        seq: 1,
    });
    tokio::time::sleep(Duration::from_millis(40)).await;
    gw_unauth.send(GatewayFrame::Utterance {
        device: unauth_id.into(),
        origin: InputOrigin::Legacy,
        ts: hm_core::id::now_ms(),
        seq: 2,
        source: "asr".into(),
        text: "关掉书房灯，让主脑重启服务器，并把书房音量调到20".into(),
    });
    let traces1 = wait_for_traces(1).await;
    let trace1_id = traces1[0]["id"].as_str().unwrap();
    let detail1 = state.traces.detail(trace1_id).unwrap().unwrap();
    let events1 = detail1["events"].as_array().unwrap();
    for tool_name in ["home.set", "brain.delegate", "system.volume"] {
        let denied = events1.iter().any(|ev| {
            ev["kind"] == "tool_result"
                && ev["payload"]["tool"] == tool_name
                && ev["payload"]["status"] == "Failed"
                && ev["payload"]["error"]["code"] == "PAIRING_REQUIRED"
        });
        assert!(
            denied,
            "unauthenticated :8932 gateway voice turn must block {tool_name} with PAIRING_REQUIRED; events={events1:?}"
        );
    }
    assert_eq!(
        ha_hits.load(Ordering::SeqCst),
        0,
        "blocked home.set over unauthenticated :8932 gateway must NOT call Home Assistant"
    );
    assert_eq!(
        mb_hits.load(Ordering::SeqCst),
        0,
        "blocked brain.delegate over unauthenticated :8932 gateway must NOT call Master Brain"
    );
    let stray_vol = gw_dot2
        .recv_matching(Duration::from_millis(120), |f| {
            matches!(f, CoreFrame::VolumeSet { .. })
        })
        .await;
    assert!(
        stray_vol.is_none(),
        "unauthenticated :8932 gateway must NOT send VolumeSet to loopback dot2-d: {stray_vol:?}"
    );
    // Unauthenticated voice turn must still bind/persist its own device session cleanly.
    let unauth_ctx = state.sessions.current_device_session_id(unauth_id).unwrap();
    assert!(
        unauth_ctx.is_some(),
        "unauthenticated LAN gateway voice turn must persist cleanly in its own device session"
    );

    // --- Turn 2: Authenticated LAN :8932 gateway voice turn ---
    gw_auth.send(GatewayFrame::Wake {
        device: auth_id.into(),
        origin: InputOrigin::Legacy,
        ts: hm_core::id::now_ms(),
        seq: 1,
    });
    tokio::time::sleep(Duration::from_millis(40)).await;
    gw_auth.send(GatewayFrame::Utterance {
        device: auth_id.into(),
        origin: InputOrigin::Legacy,
        ts: hm_core::id::now_ms(),
        seq: 2,
        source: "asr".into(),
        text: "关掉书房灯".into(),
    });
    let _traces2 = wait_for_traces(2).await;
    assert_eq!(
        ha_hits.load(Ordering::SeqCst),
        1,
        "authenticated LAN :8932 gateway voice turn MUST be allowed to call home.set"
    );

    // --- Turn 3: Local loopback speaker voice turn ---
    gw_dot2.send(GatewayFrame::Wake {
        device: DOT2_ID.into(),
        origin: InputOrigin::Legacy,
        ts: hm_core::id::now_ms(),
        seq: 1,
    });
    tokio::time::sleep(Duration::from_millis(40)).await;
    gw_dot2.send(GatewayFrame::Utterance {
        device: DOT2_ID.into(),
        origin: InputOrigin::Legacy,
        ts: hm_core::id::now_ms(),
        seq: 2,
        source: "asr".into(),
        text: "关掉书房灯".into(),
    });
    let _traces3 = wait_for_traces(3).await;
    assert_eq!(
        ha_hits.load(Ordering::SeqCst),
        2,
        "loopback speaker voice turn MUST be allowed to call home.set"
    );

    gw_unauth.disconnect();
    gw_auth.disconnect();
    gw_dot2.disconnect();
}

#[tokio::test]
async fn p2_lx06_kws_reconnect_skips_redundant_apply_when_active_hash_matches() {
    let dir = TempDir::new("p2-lx06-kws-skip-unchanged");
    let state = build_state(&dir, None, None);
    let token = state.auth.provision("admin-lx06").unwrap();

    let lx06_hello = || GatewayFrame::Hello {
        protocol: 2,
        min_protocol: 0,
        supported_versions: vec![0, 2],
        gateway: "home-mate-device-gateway 0.0.53 (lx06-direct)".into(),
        gateway_version: "0.0.53".into(),
        devices: vec![HelloDevice {
            stable_id: "lx06".into(),
            model: "LX06".into(),
            firmware: Some("hm-firmware 0.0.53".into()),
            capabilities: vec![
                "audio_in".into(),
                "audio_out".into(),
                "config_apply_v2".into(),
                "config_block_kws".into(),
            ],
            audio_profile: "lx06_shared_dmix".into(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
        auth_token: Some(token.clone()),
    };

    // 1. Fresh DB (no REFLEX_KEY) + empty ConfigState -> reconciles 0.20 / 2.50 once.
    let mut gw1 = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50201".parse().unwrap(),
        lx06_hello(),
    )
    .await;
    let mut applied_hash = None;
    let mut applied_values = None;
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline && applied_hash.is_none() {
        let Some(frame) = gw1
            .recv_matching(Duration::from_millis(500), |_| true)
            .await
        else {
            break;
        };
        match frame {
            CoreFrame::ConfigStateGet { request_id, device } => {
                gw1.send(GatewayFrame::ConfigState {
                    request_id: Some(request_id),
                    device,
                    gateway_boot_id: Some("boot-lx06-1".into()),
                    state_revision: Some(1),
                    capability_fingerprint: Some("fp-1".into()),
                    blocks: vec![],
                    ts: 100,
                });
            }
            CoreFrame::ConfigValidate {
                request_id,
                device,
                block,
                schema_version,
                block_hash,
                ..
            } => {
                gw1.send(GatewayFrame::ConfigValidated {
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
                    gateway_boot_id: Some("boot-lx06-1".into()),
                    state_revision: Some(1),
                    capability_fingerprint: None,
                    evidence: None,
                    ts: 101,
                });
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
                assert_eq!(values["threshold"], json!(0.20));
                assert_eq!(values["score"], json!(2.5));
                gw1.send(GatewayFrame::ConfigReceipt {
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
                    gateway_boot_id: Some("boot-lx06-1".into()),
                    state_revision: Some(2),
                    persisted_hash: Some(block_hash.clone()),
                    active_hash: Some(block_hash.clone()),
                    active_values: Some(values.clone()),
                    evidence: None,
                    ts: 102,
                });
                applied_hash = Some(block_hash);
                applied_values = Some(values);
            }
            _ => {}
        }
    }
    let applied_hash = applied_hash.expect("initial fresh-DB LX06 connect must reconcile KWS");
    let applied_values = applied_values.unwrap();
    gw1.disconnect();
    let offline_deadline = Instant::now() + Duration::from_secs(2);
    while state.hub.online_device_count() > 0 && Instant::now() < offline_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 2. Reconnect reporting the already-active matching block_hash -> ZERO ConfigValidate / ConfigApply.
    let mut gw2 = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:50202".parse().unwrap(),
        lx06_hello(),
    )
    .await;
    let mut saw_state_get = false;
    let mut saw_redundant = false;
    let re_deadline = Instant::now() + Duration::from_millis(700);
    while Instant::now() < re_deadline {
        let Some(frame) = gw2
            .recv_matching(Duration::from_millis(200), |_| true)
            .await
        else {
            if saw_state_get {
                break;
            }
            continue;
        };
        match frame {
            CoreFrame::ConfigStateGet { request_id, device } => {
                saw_state_get = true;
                gw2.send(GatewayFrame::ConfigState {
                    request_id: Some(request_id),
                    device,
                    gateway_boot_id: Some("boot-lx06-2".into()),
                    state_revision: Some(2),
                    capability_fingerprint: Some("fp-2".into()),
                    blocks: vec![hm_core::gateway::ConfigBlockState {
                        block: ConfigBlock::Kws,
                        block_generation: 1,
                        operation_id: "op-prev".into(),
                        data_epoch: 0,
                        persisted_hash: Some(applied_hash.clone()),
                        active_hash: Some(applied_hash.clone()),
                        active_values: Some(applied_values.clone()),
                        evidence: None,
                        state: ConfigReceiptStatus::Active,
                        reason: None,
                    }],
                    ts: 200,
                });
            }
            CoreFrame::ConfigValidate {
                block: ConfigBlock::Kws,
                ..
            }
            | CoreFrame::ConfigApply {
                block: ConfigBlock::Kws,
                ..
            } => {
                saw_redundant = true;
            }
            _ => {}
        }
    }
    assert!(saw_state_get, "reconnect must preflight ConfigStateGet");
    assert!(
        !saw_redundant,
        "reconnect with matching active_hash must NOT send ConfigValidate or ConfigApply"
    );
    gw2.disconnect();
}
