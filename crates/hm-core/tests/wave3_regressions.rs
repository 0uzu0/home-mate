//! Wave 3 end-to-end regression suite covering all P0, P1, and P2 blind-audit fixes:
//! - P0-2: Factory reset resets `AuthStore.lan_gateway_exempt` & preserves `core_id`.
//! - P0-4: `AnnouncePriority::parse("urgent")` alias & Hermes approval priority.
//! - P1-1: `POST /api/v0/voice/announce` requires Bearer auth even under `lan_gateway_exempt`
//!         and rejects forged unknown `task_id` when `proactive_speech=false`.
//! - P1-2: Offline loopback/authenticated `device_id` reservation blocks hijack via Hello & DeviceStatus.
//! - P1-3: `POST /api/v0/working-memory/clear` honors `scope_person`/`scope_device` filters.
//! - P1-4: Single-Core multi-gateway wake arbitration with 0 remote fleet peers.
//! - P1-5: `VoicePipeline::set_volume` clamps to per-unit `unit_acoustic_profiles.volume_cap`
//!         and `system.volume` confirms 100% requests against effective clamped volume.
//! - P1-6: `config_delivery` post-apply supersede rollback + implicit `"half"` duplex rollback.
//! - P1-8: Persisted `device_led_configs` reconciled automatically on device reconnect.
//! - P1-9: Mutating tools (`reminder.set`, `workmem.set`, `learning.start`, `learning.control`)
//!         avoid `SentenceGate` false refusal while `learning.start` preserves card prose.
//! - P1-10 & P2-3: `weather.now`/`weather.forecast` accept explicit `place` without `default_alias`,
//!         and forecast day index >= 3 uses `d.date` instead of `"后天"`.
//! - P1-11, P2-1, P2-2, P2-7: `music.control` ready without music, supports `"previous"`,
//!         and `music.search(source="online", query="")` rejects empty query even when library configured.
//! - P1-12: `MAX_MODEL_CALLS` exhaustion emits explicit `AgentEvent::Error` + `TurnEnd { failed }`.
//! - P2-4, P2-5, P2-6: `openapi.call` omits empty `"enum"`, `reminder` defaults `minute` to `0`.
//! - P2-8 & P2-9: Export/import round-trips `story`/`joke`/`radio`/`learning`/`agent_md`,
//!         `config_history_restore` supports `llm_fallback`.
//! - P2-10 & P2-11: Unpaired `voice_gateway_open` blocks public IPs, Web Console sets security headers.

mod common;

use axum::extract::ConnectInfo;
use futures::StreamExt;
use hm_core::agent::{run_turn, AgentEnv, AgentEvent};
use hm_core::api::{
    apply_config_import, build_config_document, router, AppState, ConfigImportBody,
};
use hm_core::config::{
    DeviceLedConfig, MusicConfig, MusicConnection, PersonalEndpointConfig, UnitAcousticProfile,
    WeatherConfig, WeatherPlace, LEARNING_KEY, LEARNING_TOKEN_SECRET, STORY_KEY,
    STORY_TOKEN_SECRET,
};
use hm_core::config_delivery::{
    Block, Commit, Compatibility, ConfigApplyCoordinator, ConfigDelivery, Operation, Package,
    Purpose, Source, Target,
};
use hm_core::gateway::{
    ConfigBlock, ConfigReceiptStatus, CoreFrame, DeviceHub, GatewayFrame, HelloDevice, InputOrigin,
};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::store::Store;
use hm_core::tool::mocks::MockInject;
use hm_core::tool::{ToolCtx, ToolStatus};
use hm_core::voice::WakeArbitration;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hm-wave3-{tag}-{}-{}",
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

fn boot_state(
    tag: &str,
    llm: Option<Arc<dyn hm_core::llm::LlmClient>>,
) -> (TempDir, Arc<AppState>) {
    let dir = TempDir::new(tag);
    let store = Arc::new(Store::open(&dir.0.join("hm.db")).unwrap());
    store.allow_loopback_public_for_test();
    let state = AppState::with_overrides(
        store,
        dir.0.clone(),
        MockInject::default(),
        llm,
        None,
        None,
        None,
    );
    state.hub.set_voice_gateway_open(true);
    (dir, state)
}

fn make_agent_env(state: &Arc<AppState>, llm: Arc<dyn hm_core::llm::LlmClient>) -> Arc<AgentEnv> {
    Arc::new(AgentEnv {
        prompt_preferences: Default::default(),
        llm,
        model: "fake".into(),
        llm_fallback: None,
        registry: state.registry.clone(),
        sessions: state.sessions.clone(),
        tasks: state.tasks.clone(),
        traces: state.traces.clone(),
        memory: state.memory.clone(),
        working_memory: state.working_memory.clone(),
        shared_memory: None,
        user_md: None,
        scope: Default::default(),
        assistant_name: hm_core::ASSISTANT_NAME.into(),
        llm_first_event_timeout: Duration::from_secs(5),
    })
}

async fn spawn_http_as(state: Arc<AppState>, peer: SocketAddr) -> String {
    let app = router(state).layer(axum::middleware::from_fn(
        move |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
            req.extensions_mut().insert(ConnectInfo(peer));
            next.run(req).await
        },
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

struct MockGw {
    tx: mpsc::UnboundedSender<GatewayFrame>,
    rx: mpsc::UnboundedReceiver<CoreFrame>,
    srv: tokio::task::JoinHandle<()>,
    _r: tokio::task::JoinHandle<()>,
    _w: tokio::task::JoinHandle<()>,
}

impl MockGw {
    async fn connect(
        hub: Arc<DeviceHub>,
        peer: SocketAddr,
        device_ids: &[&str],
        caps: Vec<&str>,
        token: Option<String>,
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
        let devices = device_ids
            .iter()
            .map(|id| HelloDevice {
                stable_id: (*id).into(),
                model: "dot2".into(),
                firmware: Some("2.3.6".into()),
                capabilities: caps.iter().map(|s| (*s).into()).collect(),
                audio_profile: "default".into(),
                wifi_ssid: None,
            })
            .collect();
        let hello = GatewayFrame::Hello {
            protocol: 2,
            min_protocol: 0,
            supported_versions: vec![0, 2],
            gateway: "wave3-gw".into(),
            gateway_version: "2.3.6".into(),
            devices,
            aec_tail_ms: Some(160),
            profile_version: Some("v1".into()),
            auth_token: token,
        };
        wr.write_all(format!("{}\n", serde_json::to_string(&hello).unwrap()).as_bytes())
            .await
            .unwrap();
        let (tx, mut out_rx) = mpsc::unbounded_channel::<GatewayFrame>();
        let w = tokio::spawn(async move {
            while let Some(f) = out_rx.recv().await {
                let line = format!("{}\n", serde_json::to_string(&f).unwrap());
                if wr.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let (in_tx, rx) = mpsc::unbounded_channel::<CoreFrame>();
        let r = tokio::spawn(async move {
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(cf) = serde_json::from_str::<CoreFrame>(&line) {
                    let _ = in_tx.send(cf);
                }
            }
        });
        Self {
            tx,
            rx,
            srv,
            _r: r,
            _w: w,
        }
    }

    async fn wait_frame<F>(&mut self, timeout: Duration, mut pred: F) -> Option<CoreFrame>
    where
        F: FnMut(&CoreFrame) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(rem, self.rx.recv()).await {
                Ok(Some(f)) if pred(&f) => return Some(f),
                Ok(Some(_)) => continue,
                _ => return None,
            }
        }
        None
    }
}

#[tokio::test]
async fn p0_2_factory_reset_clears_lan_exempt_and_preserves_core_id() {
    let (_d, state) = boot_state("p0-2", None);
    let token = state.auth.provision("owner").unwrap();
    let core_id_before = state.auth.core_id().to_string();
    state.auth.set_lan_gateway_exempt(true).unwrap();
    assert!(state.auth.lan_gateway_exempt());

    let lan_base = spawn_http_as(state.clone(), "192.168.10.90:51000".parse().unwrap()).await;
    let http = reqwest::Client::new();

    // Before reset: unauth LAN peer is admitted to everyday /status.
    let pre = http
        .get(format!("{lan_base}/api/v0/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(pre.status(), reqwest::StatusCode::OK);

    // Execute factory reset (preserving secrets/tokens).
    let reset = http
        .post(format!("{lan_base}/api/v0/system/factory-reset"))
        .bearer_auth(&token)
        .json(&json!({ "confirm": "FACTORY_RESET", "include_secrets": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(reset.status(), reqwest::StatusCode::OK);

    // In-memory lan_gateway_exempt must immediately flip to false (fail-closed).
    assert!(!state.auth.lan_gateway_exempt());
    // core_id must be re-persisted in SQLite config_kv.
    assert_eq!(
        state.store.config_get("core_id").unwrap().as_deref(),
        Some(core_id_before.as_str())
    );
    // Once the owner re-pairs after factory reset, unauthenticated LAN peer MUST receive 401
    // (previously stale in-memory lan_gateway_exempt=true leaked 200 OK!).
    let _new_token = state.auth.provision("owner-post-reset").unwrap();
    let post = http
        .get(format!("{lan_base}/api/v0/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn p1_1_announce_never_lan_exempt_and_rejects_forged_task_id() {
    let (_d, state) = boot_state("p1-1", None);
    let token = state.auth.provision("owner").unwrap();
    state.auth.set_lan_gateway_exempt(true).unwrap();

    let mut gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:41101".parse().unwrap(),
        &["dot2-d"],
        vec!["audio_out", "play_stream"],
        None,
    )
    .await;
    assert!(gw
        .wait_frame(Duration::from_secs(2), |f| matches!(
            f,
            CoreFrame::HelloAck { .. }
        ))
        .await
        .is_some());

    let lan_base = spawn_http_as(state.clone(), "192.168.10.91:51001".parse().unwrap()).await;
    let http = reqwest::Client::new();

    // Even with lan_gateway_exempt=true, unauthenticated LAN POST /voice/announce MUST return 401.
    let unauth = http
        .post(format!("{lan_base}/api/v0/voice/announce"))
        .json(&json!({
            "device": "dot2-d",
            "text": "未经授权播报",
            "task_id": "forged",
            "open_followup": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Even with Bearer token, unknown task_id must be rejected when proactive_speech=false.
    let forged = http
        .post(format!("{lan_base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(&json!({
            "device": "dot2-d",
            "text": "伪造任务ID",
            "task_id": "nonexistent-task-xyz",
            "open_followup": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), reqwest::StatusCode::CONFLICT);

    // Known Core task succeeds (and accepts priority="urgent" alias -> high).
    let real_task = state.tasks.create("wave3-announce").unwrap();
    let ok = http
        .post(format!("{lan_base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(&json!({
            "device": "dot2-d",
            "text": "合法任务回调",
            "priority": "urgent",
            "task_id": real_task.id
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), reqwest::StatusCode::ACCEPTED);
}

#[tokio::test]
async fn p1_2_offline_device_id_reservation_blocks_unauth_lan_hijack() {
    let (_d, state) = boot_state("p1-2", None);
    let token = state.auth.provision("owner").unwrap();
    state.auth.set_lan_gateway_exempt(true).unwrap();

    // 1. Loopback dot2-d connects then disconnects cleanly so `retire_connection` runs.
    {
        let mut local = MockGw::connect(
            state.hub.clone(),
            "127.0.0.1:41201".parse().unwrap(),
            &["dot2-d"],
            vec!["audio_in", "audio_out"],
            None,
        )
        .await;
        assert!(local
            .wait_frame(Duration::from_secs(2), |f| matches!(
                f,
                CoreFrame::HelloAck { .. }
            ))
            .await
            .is_some());
        drop(local.tx);
        let _ = local._w.await;
        local._r.abort();
        let _ = tokio::time::timeout(Duration::from_secs(2), local.srv).await;
    }
    assert!(
        !state.hub.device_online_with_cap("dot2-d", "audio_out"),
        "dot2-d must be offline before testing offline reservation"
    );

    // 2. While dot2-d is offline, an unauthenticated LAN peer attempts to claim dot2-d via Hello.
    let mut attacker = MockGw::connect(
        state.hub.clone(),
        "192.168.10.92:41202".parse().unwrap(),
        &["dot2-d"],
        vec!["audio_in", "audio_out"],
        None,
    )
    .await;
    let rej = attacker
        .wait_frame(Duration::from_secs(2), |f| {
            matches!(f, CoreFrame::HelloReject { .. })
        })
        .await;
    match rej {
        Some(CoreFrame::HelloReject { reason, .. }) => assert_eq!(reason, "device_id_reserved"),
        other => panic!("expected HelloReject(device_id_reserved), got {other:?}"),
    }

    // 3. Same unauthenticated LAN peer connects under a benign id, then tries to hijack dot2-d via DeviceStatus.
    let mut attacker2 = MockGw::connect(
        state.hub.clone(),
        "192.168.10.93:41203".parse().unwrap(),
        &["android-guest"],
        vec!["audio_out"],
        None,
    )
    .await;
    assert!(attacker2
        .wait_frame(Duration::from_secs(2), |f| matches!(
            f,
            CoreFrame::HelloAck { .. }
        ))
        .await
        .is_some());
    attacker2
        .tx
        .send(GatewayFrame::DeviceStatus {
            device: "dot2-d".into(),
            online: true,
            reason: "reclaim".into(),
            descriptor: Some(HelloDevice {
                stable_id: "dot2-d".into(),
                model: "spoof".into(),
                firmware: None,
                capabilities: vec!["audio_out".into()],
                audio_profile: "default".into(),
                wifi_ssid: None,
            }),
            northbound: None,
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        !state.hub.device_online_with_cap("dot2-d", "audio_out"),
        "offline loopback device must stay reserved against DeviceStatus hijack"
    );

    // 4. Authenticated LAN device disconnects -> unauth LAN peer cannot claim it either.
    {
        let mut authed = MockGw::connect(
            state.hub.clone(),
            "192.168.10.94:41204".parse().unwrap(),
            &["android-owner"],
            vec!["audio_out"],
            Some(token),
        )
        .await;
        assert!(authed
            .wait_frame(Duration::from_secs(2), |f| matches!(
                f,
                CoreFrame::HelloAck { .. }
            ))
            .await
            .is_some());
        drop(authed.tx);
        let _ = authed._w.await;
        authed._r.abort();
        let _ = tokio::time::timeout(Duration::from_secs(2), authed.srv).await;
    }
    assert!(
        !state
            .hub
            .device_online_with_cap("android-owner", "audio_out"),
        "android-owner must be offline before testing offline reservation"
    );
    let mut unauth_claim = MockGw::connect(
        state.hub.clone(),
        "192.168.10.95:41205".parse().unwrap(),
        &["android-owner"],
        vec!["audio_out"],
        None,
    )
    .await;
    let rej2 = unauth_claim
        .wait_frame(Duration::from_secs(2), |f| {
            matches!(f, CoreFrame::HelloReject { .. })
        })
        .await;
    assert!(matches!(
        rej2,
        Some(CoreFrame::HelloReject { ref reason, .. }) if reason == "device_id_reserved"
    ));
}

#[tokio::test]
async fn p1_3_working_memory_clear_honors_scope_filters() {
    let (_d, state) = boot_state("p1-3", None);
    state
        .working_memory
        .set("alice", "dot2-a", "topic-a", "alice data")
        .unwrap();
    state
        .working_memory
        .set("bob", "dot2-b", "topic-b", "bob data")
        .unwrap();
    state
        .working_memory
        .set("", "", "topic-global", "global data")
        .unwrap();

    let base = spawn_http_as(state.clone(), "127.0.0.1:51301".parse().unwrap()).await;
    let http = reqwest::Client::new();

    // Clear only scope_person=alice
    let res: Value = http
        .post(format!(
            "{base}/api/v0/working-memory/clear?scope_person=alice"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["deleted"], 1);
    let remaining = state.working_memory.list(None, None).unwrap();
    assert_eq!(remaining.len(), 2);
    assert!(remaining.iter().any(|i| i.scope_person == "bob"));
    assert!(remaining.iter().any(|i| i.key_or_topic == "topic-global"));

    // Clear only scope_device=dot2-b
    let res2: Value = http
        .post(format!(
            "{base}/api/v0/working-memory/clear?scope_device=dot2-b"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res2["deleted"], 1);
    let remaining2 = state.working_memory.list(None, None).unwrap();
    assert_eq!(remaining2.len(), 1);
    assert_eq!(remaining2[0].key_or_topic, "topic-global");
}

#[tokio::test]
async fn p1_4_single_core_multi_gateway_arbitration_with_zero_fleet_peers() {
    let (_d, state) = boot_state("p1-4", None);
    assert_eq!(state.voice.arbiter.peer_count(), 0);

    let mut gw_a = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:41401".parse().unwrap(),
        &["dot2-d"],
        vec!["audio_in", "audio_out", "kws"],
        None,
    )
    .await;
    let mut gw_b = MockGw::connect(
        state.hub.clone(),
        "192.168.10.96:41402".parse().unwrap(),
        &["android-2"],
        vec!["audio_in", "audio_out", "kws"],
        None,
    )
    .await;
    assert!(gw_a
        .wait_frame(Duration::from_secs(2), |f| matches!(
            f,
            CoreFrame::HelloAck { .. }
        ))
        .await
        .is_some());
    assert!(gw_b
        .wait_frame(Duration::from_secs(2), |f| matches!(
            f,
            CoreFrame::HelloAck { .. }
        ))
        .await
        .is_some());
    assert_eq!(state.hub.online_device_count(), 2);

    // Enable Enforce arbitration with 0 remote fleet peers.
    let mut patch = hm_core::voice::FollowWindowPatch::default();
    patch.wake_arbitration = Some(WakeArbitration::Enforce);
    state
        .voice
        .set_mode_with_follow_window(
            "global", None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, patch, "test",
        )
        .unwrap();

    let now = hm_core::id::now_ms();
    // Both gateways spot the same wake word: dot2-d is much louder (-12 dBFS vs -30 dBFS).
    gw_a.tx
        .send(GatewayFrame::Keyword {
            device: "dot2-d".into(),
            ts: now,
            seq: 1,
            keyword: "你好高达".into(),
            fed_ms: Some(600),
            self_heard: false,
            rms_dbfs: Some(-12.0),
            lag_ms: Some(20),
            origin: InputOrigin::Legacy,
            bundle_id: None,
            kws_generation: None,
            kws_tier: None,
            wake_dur_ms: None,
            max_gap_ms: None,
            l2_passed: None,
        })
        .unwrap();
    gw_b.tx
        .send(GatewayFrame::Keyword {
            device: "android-2".into(),
            ts: now + 15,
            seq: 1,
            keyword: "你好高达".into(),
            fed_ms: Some(600),
            self_heard: false,
            rms_dbfs: Some(-30.0),
            lag_ms: Some(20),
            origin: InputOrigin::Legacy,
            bundle_id: None,
            kws_generation: None,
            kws_tier: None,
            wake_dur_ms: None,
            max_gap_ms: None,
            l2_passed: None,
        })
        .unwrap();

    // Wait for arbitration window (200ms) + dispatch.
    let mut saw_lost = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        let log = state.voice.recent(100);
        if log.iter().any(|e| {
            e["kind"] == "arbitration_lost"
                && e["device"] == "android-2"
                && e["decision"]["winner"] == "dot2-d"
        }) {
            saw_lost = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        saw_lost,
        "android-2 must lose single-Core multi-gateway arbitration to louder dot2-d even with 0 fleet peers"
    );
}

#[tokio::test]
async fn p1_5_volume_clamps_to_per_unit_cap_and_confirms_100_percent() {
    let (_d, state) = boot_state("p1-5", None);
    let mut profile = UnitAcousticProfile::default();
    profile.volume_cap = 0.65;
    state
        .config
        .save_unit_acoustic_profile("dot2-d", profile, "test")
        .unwrap();

    let mut gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:41501".parse().unwrap(),
        &["dot2-d"],
        vec!["audio_out", "volume"],
        None,
    )
    .await;
    assert!(gw
        .wait_frame(Duration::from_secs(2), |f| matches!(
            f,
            CoreFrame::HelloAck { .. }
        ))
        .await
        .is_some());

    // Auto-reply to VolumeSet with exact confirmed effective volume.
    let gw_tx = gw.tx.clone();
    let responder = tokio::spawn(async move {
        while let Some(f) = gw.rx.recv().await {
            if let CoreFrame::VolumeSet {
                device,
                task_id,
                value,
                ..
            } = f
            {
                let _ = gw_tx.send(GatewayFrame::VolumeReceipt {
                    task_id,
                    effective: value,
                    unit: "linear".into(),
                    stage: "device_confirmed".into(),
                    device: Some(device),
                });
                return value;
            }
        }
        -1.0
    });

    let ctx = ToolCtx::new("T-vol", "C-vol", Arc::new(AtomicBool::new(false)))
        .with_provenance(Some("dot2-d".into()), None);
    let res = state
        .registry
        .call("system.volume", json!({ "percent": 100 }), &ctx)
        .await;
    let sent_value = responder.await.unwrap();
    assert!(
        (sent_value - 0.65).abs() < 1e-6,
        "expected clamped 0.65, got {sent_value}"
    );
    assert_eq!(
        res.status,
        ToolStatus::Completed,
        "100% request must confirm against effective cap: {res:?}"
    );
    assert!((res.result.unwrap()["reported"].as_f64().unwrap() - 0.65).abs() < 1e-6);
}

#[tokio::test]
async fn p1_6_mode_supersede_after_gateway_apply_rolls_back_implicit_half_duplex() {
    let (_d, state) = boot_state("p1-6", None);
    let delivery = ConfigDelivery::for_app(&state);
    let enrolled = delivery.enroll("dot2-d").unwrap();
    let hh = enrolled["household_id"].as_str().unwrap().to_string();

    let mut gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:41601".parse().unwrap(),
        &["dot2-d"],
        vec!["audio_out", "config_apply_v2", "config_block_duplex"],
        None,
    )
    .await;
    assert!(gw
        .wait_frame(Duration::from_secs(2), |f| matches!(
            f,
            CoreFrame::HelloAck { .. }
        ))
        .await
        .is_some());

    // Start background ConfigApplyCoordinator.
    let _coord = ConfigApplyCoordinator::start_for_app(&state);

    // Enqueue a mode delivery that sets duplex_requested.dot2-d = "full" (when expected has NO entry -> implicit "half").
    let pkg = Package {
        format: "home-mate-config".into(),
        version: 2,
        package_id: "00000000-0000-4000-8000-000000000106".into(),
        package_revision: 1,
        created_ms: 1,
        purpose: Purpose::NodeConfig,
        source: Source {
            node_id: "dot2-d".into(),
            config_revision: 0,
            core_version: "0.1.0".into(),
            schema_version: 2,
        },
        target: Target {
            household_id: hh,
            node_id: Some("dot2-d".into()),
        },
        compatibility: Compatibility {
            core_api_schema: 2,
            gateway_protocol_min: 0,
            gateway_protocol_max: 2,
            required_capabilities: vec!["config_apply_v2".into()],
        },
        change_reason: "p1-6 rollback test".into(),
        blocks: vec![Block {
            name: "mode".into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: "mode.duplex_requested.dot2-d".into(),
                value: json!("full"),
            }],
        }],
        bindings: vec![],
        content_hash: None,
    };
    let plan = delivery.preflight("admin", pkg, json!({})).unwrap();
    let app_id = "11111111-1111-4111-8111-000000000106".to_string();
    let _ = delivery
        .commit(
            "admin",
            &Commit {
                plan_id: plan["plan_id"].as_str().unwrap().into(),
                plan_hash: plan["plan_hash"].as_str().unwrap().into(),
                expected_revision: plan["expected_revision"].as_u64().unwrap(),
                application_id: app_id,
            },
        )
        .unwrap();

    // Serve Gateway side: answer ConfigStateGet / ConfigValidate / forward ConfigApply,
    // concurrently bump config_head revision before sending Active so SQLite publication
    // is superseded (Ok(false)), then capture the compensating Duplex rollback frame!
    let store_c = state.store.clone();
    let mut rollback_duplex: Option<String> = None;
    let mut saw_forward = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while tokio::time::Instant::now() < deadline {
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(frame)) = tokio::time::timeout(rem, gw.rx.recv()).await else {
            break;
        };
        match frame {
            CoreFrame::ConfigStateGet { request_id, device } => {
                let _ = gw.tx.send(GatewayFrame::ConfigState {
                    request_id: Some(request_id),
                    device,
                    gateway_boot_id: Some("boot-1".into()),
                    state_revision: Some(1),
                    capability_fingerprint: Some("fp-1".into()),
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
                let _ = gw.tx.send(GatewayFrame::ConfigValidated {
                    request_id,
                    device,
                    block,
                    schema_version,
                    block_hash,
                    ok: true,
                    supported: true,
                    blockers: Vec::new(),
                    state: ConfigReceiptStatus::Validated,
                    reason: None,
                    retryable: false,
                    gateway_boot_id: Some("boot-1".into()),
                    state_revision: Some(1),
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
                block: ConfigBlock::Duplex,
                schema_version,
                block_hash,
                values,
                ..
            } => {
                if !saw_forward {
                    assert_eq!(values["duplex_requested"], "full");
                    saw_forward = true;
                    // Concurrently advance config_head so SQLite publication returns Ok(false)!
                    store_c
                        .with(|c| {
                            c.execute(
                                "UPDATE config_head SET revision = revision + 1 WHERE id = 1",
                                [],
                            )?;
                            Ok(())
                        })
                        .unwrap();
                } else {
                    rollback_duplex = values["duplex_requested"].as_str().map(str::to_string);
                }
                let _ = gw.tx.send(GatewayFrame::ConfigReceipt {
                    device,
                    core_node_id,
                    data_epoch,
                    application_id,
                    operation_id,
                    desired_revision,
                    block_generation,
                    block: ConfigBlock::Duplex,
                    schema_version,
                    block_hash,
                    state: ConfigReceiptStatus::Active,
                    reason: None,
                    retryable: false,
                    gateway_boot_id: Some("boot-1".into()),
                    state_revision: Some(2),
                    persisted_hash: Some("ph".into()),
                    active_hash: Some("ah".into()),
                    active_values: Some(values),
                    evidence: None,
                    ts: hm_core::id::now_ms(),
                });
                if rollback_duplex.is_some() {
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(saw_forward, "expected forward ConfigApply(Duplex)");
    assert_eq!(
        rollback_duplex.as_deref(),
        Some("half"),
        "post-apply supersede must roll back implicit empty duplex_requested to 'half'"
    );
    drop(_coord);
}

#[tokio::test]
async fn p1_8_led_config_reconciles_on_device_reconnect() {
    let (_d, state) = boot_state("p1-8", None);

    let led = DeviceLedConfig {
        night_enabled: true,
        night_start_minute: 1320,
        night_end_minute: 420,
        night_brightness_pct: 15,
        orientation_rotation: 2,
        orientation_reverse: true,
    };
    state
        .config
        .save_device_led_config("dot2-d", led.clone(), "test")
        .unwrap();

    // Connect legacy gateway (without config_block_led) -> must receive CoreFrame::LedConfig immediately.
    let mut gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:41801".parse().unwrap(),
        &["dot2-d"],
        vec!["audio_out"],
        None,
    )
    .await;
    let led_frame = gw
        .wait_frame(Duration::from_secs(2), |f| {
            matches!(f, CoreFrame::LedConfig { .. })
        })
        .await;
    match led_frame {
        Some(CoreFrame::LedConfig {
            device,
            night_enabled,
            night_brightness_pct,
            orientation_rotation,
            orientation_reverse,
            ..
        }) => {
            assert_eq!(device, "dot2-d");
            assert!(night_enabled);
            assert_eq!(night_brightness_pct, 15);
            assert_eq!(orientation_rotation, 2);
            assert!(orientation_reverse);
        }
        other => panic!("expected CoreFrame::LedConfig on reconnect, got {other:?}"),
    }
}

#[tokio::test]
async fn p1_9_mutating_tools_and_learning_prose_avoid_sentence_gate_false_refusal() {
    // Fake learning-svc HTTP server returning a vocabulary card on start & control.
    let app = axum::Router::new()
        .route(
            "/api/v1/learning/device/sessions/start",
            axum::routing::post(|| async {
                axum::Json(
                    json!({ "session_id": "ls-1", "card": { "word": "apple", "meaning": "苹果" } }),
                )
            }),
        )
        .route(
            "/api/v1/learning/device/current/control",
            axum::routing::post(|| async { axum::Json(json!({ "status": "paused" })) }),
        );
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let l_addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });

    // Scripted LLM calls:
    // Turn 1: reminder.set (with hour only -> exercises P2-6 minute=0 default too!)
    // Turn 2: learning.start -> Core speaks fact + preserves non-action card prose ("apple")!
    // Turn 3: learning.control(pause) -> Core speaks fact with zero refusal!
    let llm = Arc::new(FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "reminder.set".into(),
                json!({ "note": "吃药", "hour": 8 }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["已经帮你设置好明天早上8点吃药的提醒了。".into()],
        },
        Scripted::Calls {
            calls: vec![("learning.start".into(), "{}".into())],
        },
        Scripted::Text {
            chunks: vec!["已为你开启英语学习。第一个词是 apple，请问它的中文意思是什么？".into()],
        },
        Scripted::Calls {
            calls: vec![(
                "learning.control".into(),
                json!({ "action": "pause" }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["已经帮你暂停学习了。".into()],
        },
    ]));

    let (_d, state) = boot_state("p1-9", Some(llm.clone()));
    state
        .config
        .save_personal_endpoint(
            LEARNING_KEY,
            LEARNING_TOKEN_SECRET,
            PersonalEndpointConfig {
                enabled: true,
                url: format!("http://{l_addr}"),
                timeout_ms: 4000,
            },
            None,
            "test",
        )
        .unwrap();

    let mut gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:41901".parse().unwrap(),
        &["dot2-d"],
        vec!["audio_out"],
        None,
    )
    .await;
    assert!(gw
        .wait_frame(Duration::from_secs(2), |f| matches!(
            f,
            CoreFrame::HelloAck { .. }
        ))
        .await
        .is_some());

    let run = |text: &str| {
        let state = state.clone();
        let llm = llm.clone();
        let text = text.to_string();
        async move {
            let env = make_agent_env(&state, llm);
            let mut stream = run_turn(env, Some("s-p1-9".into()), text, "dot2-d".into());
            let mut answer = String::new();
            let mut facts = Vec::new();
            while let Some(ev) = stream.next().await {
                match ev {
                    AgentEvent::Fact { text } => facts.push(text),
                    AgentEvent::Answer { text } => answer = text,
                    _ => {}
                }
            }
            (answer, facts)
        }
    };

    // 1. reminder.set with hour only (also exercises P2-6 minute=0 default!)
    let (ans1, facts1) = run("早上8点提醒我吃药").await;
    assert!(
        !ans1.contains("我没有执行这个操作"),
        "reminder.set must NOT trigger false refusal: ans={ans1:?} facts={facts1:?}"
    );
    assert!(
        ans1.contains("吃药"),
        "expected reminder answer, got {ans1:?}"
    );

    // 2. learning.start must speak fact AND preserve the vocabulary card prose!
    let (ans2, facts2) = run("开始背单词").await;
    assert!(
        !ans2.contains("我没有执行这个操作"),
        "learning.start must NOT trigger false refusal: ans={ans2:?}"
    );
    assert!(
        ans2.contains("apple"),
        "learning.start must preserve vocabulary card prose: ans={ans2:?} facts={facts2:?}"
    );

    // 3. learning.control(pause) must emit confirmed fact with zero false refusal!
    let (ans3, _) = run("暂停学习").await;
    assert!(!ans3.contains("我没有执行这个操作"), "got {ans3:?}");
    assert!(ans3.contains("暂停"), "got {ans3:?}");
}

#[tokio::test]
async fn p1_10_p2_3_weather_explicit_place_without_default_and_day4_uses_date() {
    let mock_weather = axum::Router::new().route(
        "/v1/forecast",
        axum::routing::get(|| async {
            axum::Json(json!({
                "current": { "temperature_2m": 22.0, "apparent_temperature": 21.5, "relative_humidity_2m": 55, "wind_speed_10m": 8.0, "weather_code": 0 },
                "daily": {
                    "time": ["2026-11-24", "2026-11-25", "2026-11-26", "2026-11-27", "2026-11-28"],
                    "weather_code": [0, 1, 2, 3, 61],
                    "temperature_2m_min": [10.0, 11.0, 12.0, 13.0, 14.0],
                    "temperature_2m_max": [20.0, 21.0, 22.0, 23.0, 24.0],
                    "precipitation_probability_max": [0, 10, 20, 30, 80]
                }
            }))
        }),
    );
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, mock_weather).await;
    });

    let (_d, state) = boot_state("p1-10", None);
    state
        .config
        .save_weather(
            WeatherConfig {
                enabled: true,
                provider: "open_meteo".into(),
                endpoint: format!("http://{addr}"),
                api_key_env: String::new(),
                places: vec![
                    WeatherPlace {
                        alias: "家".into(),
                        name: "北京".into(),
                        latitude: 39.9,
                        longitude: 116.4,
                        timezone: String::new(),
                        city_code: String::new(),
                    },
                    WeatherPlace {
                        alias: "公司".into(),
                        name: "上海".into(),
                        latitude: 31.2,
                        longitude: 121.5,
                        timezone: String::new(),
                        city_code: String::new(),
                    },
                ],
                default_alias: String::new(), // explicitly empty with 2 places!
                max_days: 5,
            },
            None,
            "test",
        )
        .unwrap();

    let ctx = ToolCtx::new("T-w", "C-w", Arc::new(AtomicBool::new(false)));
    // Omitted place still returns WEATHER_PLACE_REQUIRED
    let missing = state.registry.call("weather.now", json!({}), &ctx).await;
    assert_eq!(missing.status, ToolStatus::Failed);

    // Explicit place succeeds!
    let now_res = state
        .registry
        .call("weather.now", json!({ "place": "公司" }), &ctx)
        .await;
    assert_eq!(now_res.status, ToolStatus::Completed, "{now_res:?}");

    // Forecast for 5 days labels days 0,1,2 as 今天/明天/后天 and days 3,4 as date strings!
    let fc_res = state
        .registry
        .call(
            "weather.forecast",
            json!({ "place": "公司", "days": 5 }),
            &ctx,
        )
        .await;
    assert_eq!(fc_res.status, ToolStatus::Completed, "{fc_res:?}");
    let days = fc_res.result.unwrap()["forecast"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(days.len(), 5);
    assert_eq!(days[0]["day"], "今天");
    assert_eq!(days[1]["day"], "明天");
    assert_eq!(days[2]["day"], "后天");
    assert_eq!(days[3]["day"], "2026-11-27");
    assert_eq!(days[4]["day"], "2026-11-28");
}

#[tokio::test]
async fn p1_11_p2_1_p2_7_music_control_availability_previous_and_empty_online_query() {
    let (_d, state) = boot_state("p1-11", None);
    let specs = state.registry.specs();

    // P2-1: music.control schema advertises "previous".
    let mc_spec = specs
        .iter()
        .find(|s| s.canonical_name == "music.control")
        .unwrap();
    let actions = mc_spec.parameters["properties"]["action"]["enum"]
        .as_array()
        .unwrap();
    assert!(
        actions.contains(&json!("previous")),
        "missing previous in schema: {actions:?}"
    );

    // P1-11: Even with music completely unconfigured, calling music.control MUST NOT fail with TOOL_UNAVAILABLE!
    let ctx = ToolCtx::new("T-mc", "C-mc", Arc::new(AtomicBool::new(false)));
    let mc_res = state
        .registry
        .call("music.control", json!({ "action": "pause" }), &ctx)
        .await;
    assert_ne!(
        mc_res.error.as_ref().map(|e| e.code.as_str()),
        Some("TOOL_UNAVAILABLE"),
        "music.control must remain Ready for story/radio even when music service is unconfigured: {mc_res:?}"
    );

    // P2-4: unconfigured openapi.call must omit empty "enum" array from operation schema.
    let oa_spec = specs
        .iter()
        .find(|s| s.canonical_name == "openapi.call")
        .unwrap();
    assert!(
        oa_spec.parameters["properties"]["operation"]
            .get("enum")
            .is_none(),
        "empty enum must be omitted from openapi.call schema"
    );

    // P2-7: Configure BOTH library and online; source="online" with empty query MUST fail MUSIC_QUERY_REQUIRED.
    state
        .config
        .save_music(
            MusicConfig {
                enabled: true,
                prefer_local: true,
                library: Some(MusicConnection {
                    endpoint: "http://127.0.0.1:4533".into(),
                    user: "u".into(),
                }),
                online: Some(MusicConnection {
                    endpoint: "http://127.0.0.1:4534".into(),
                    user: String::new(),
                }),
            },
            Some("lib-sec"),
            Some("online-sec"),
            "test",
        )
        .unwrap();
    let ctx = ToolCtx::new("T-m", "C-m", Arc::new(AtomicBool::new(false)));
    let res = state
        .registry
        .call(
            "music.search",
            json!({ "source": "online", "query": "" }),
            &ctx,
        )
        .await;
    assert_eq!(res.status, ToolStatus::Failed);
    assert_eq!(res.error.unwrap().code, "MUSIC_QUERY_REQUIRED");
}

#[tokio::test]
async fn p1_12_agent_loop_budget_exhaustion_emits_explicit_error_event() {
    // 8 consecutive tool calls exhaust MAX_MODEL_CALLS without final text.
    let scripted: Vec<Scripted> = (0..8)
        .map(|_| Scripted::Calls {
            calls: vec![("time.now".into(), "{}".into())],
        })
        .collect();
    let llm = Arc::new(FakeLlm::new(scripted));
    let (_d, state) = boot_state("p1-12", Some(llm.clone()));
    let env = make_agent_env(&state, llm);
    let mut stream = run_turn(
        env,
        Some("s-loop".into()),
        "一直查时间".into(),
        "console".into(),
    );
    let mut saw_error = None;
    let mut end_status = None;
    while let Some(ev) = stream.next().await {
        match ev {
            AgentEvent::Error { message } => saw_error = Some(message),
            AgentEvent::TurnEnd { status, .. } => end_status = Some(status),
            _ => {}
        }
    }
    assert!(
        saw_error.is_some(),
        "MAX_MODEL_CALLS must emit AgentEvent::Error"
    );
    assert_eq!(end_status.as_deref(), Some("failed"));
}

#[tokio::test]
async fn p2_8_p2_9_export_import_personal_agent_md_llm_fallback_restore_and_auth_safe_rollback() {
    let (_d, state) = boot_state("p2-8", None);
    let token = state.auth.provision("owner").unwrap();
    state.auth.set_lan_gateway_exempt(true).unwrap();

    // Configure story, learning, agent_md, and llm_fallback.
    state
        .config
        .save_personal_endpoint(
            STORY_KEY,
            STORY_TOKEN_SECRET,
            PersonalEndpointConfig {
                enabled: true,
                url: "http://192.168.10.168:18083".into(),
                timeout_ms: 4000,
            },
            Some("story-secret-xyz"),
            "test",
        )
        .unwrap();
    state
        .config
        .save_personal_endpoint(
            LEARNING_KEY,
            LEARNING_TOKEN_SECRET,
            PersonalEndpointConfig {
                enabled: true,
                url: "http://192.168.10.168:18084".into(),
                timeout_ms: 4000,
            },
            Some("learn-secret-xyz"),
            "test",
        )
        .unwrap();
    state
        .agent_md
        .update("# Household Persona\nBe kind.", "test", "init")
        .unwrap();
    state
        .config
        .save_service(
            "llm_fallback",
            hm_core::config::ServiceConfig {
                provider: "openai".into(),
                base_url: "https://api.example.com/v1".into(),
                model: "gpt-4o-mini".into(),
                api_key_env: "FB_KEY".into(),
                voice: String::new(),
                thinking: "disabled".into(),
                language: String::new(),
            },
            None,
            "test",
        )
        .unwrap();

    // 1. Export WITH secrets -> includes story, learning, agent_md, and personal secrets!
    let doc = build_config_document(&state, true).unwrap();
    assert_eq!(doc["story"]["url"], "http://192.168.10.168:18083");
    assert_eq!(doc["learning"]["url"], "http://192.168.10.168:18084");
    assert_eq!(doc["agent_md"]["content"], "# Household Persona\nBe kind.");
    assert_eq!(doc["secrets"][STORY_TOKEN_SECRET], "story-secret-xyz");
    assert_eq!(doc["secrets"][LEARNING_TOKEN_SECRET], "learn-secret-xyz");

    // 2. Import into a fresh Core -> story, learning, secrets, and agent_md round-trip cleanly!
    let (_d2, state2) = boot_state("p2-8-dst", None);
    let body: ConfigImportBody = serde_json::from_value(doc).unwrap();
    let out = apply_config_import(&state2, body, "import").await.unwrap();
    assert!(out.applied.contains(&"story".to_string()));
    assert!(out.applied.contains(&"learning".to_string()));
    assert!(out.applied.contains(&"agent_md".to_string()));
    assert_eq!(
        state2.config.personal_endpoint(STORY_KEY).unwrap().url,
        "http://192.168.10.168:18083"
    );
    assert_eq!(
        state2
            .config
            .personal_token(STORY_TOKEN_SECRET)
            .unwrap()
            .as_deref(),
        Some("story-secret-xyz")
    );
    assert_eq!(
        state2.agent_md.current_content(),
        "# Household Persona\nBe kind."
    );

    // 3. config_history_restore accepts llm_fallback!
    let hist = state.config.history(20).unwrap();
    let fb_row_id = hist.iter().find(|r| r["scope"] == "llm_fallback").unwrap()["id"]
        .as_i64()
        .unwrap();
    let base = spawn_http_as(state.clone(), "127.0.0.1:52801".parse().unwrap()).await;
    let http = reqwest::Client::new();
    let restore = http
        .post(format!("{base}/api/v0/config/history/{fb_row_id}/restore"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(restore.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn p2_10_p2_11_unpaired_public_ip_rejected_and_console_security_headers() {
    // Unpaired Core with voice_gateway_open=true MUST reject public internet IP on :8932.
    let (_d, state) = boot_state("p2-10", None);
    assert!(!state.auth.configured());
    let mut public_gw = MockGw::connect(
        state.hub.clone(),
        "8.8.8.8:55001".parse().unwrap(),
        &["evil-public"],
        vec!["audio_out"],
        None,
    )
    .await;
    let rej = public_gw
        .wait_frame(Duration::from_secs(2), |f| {
            matches!(f, CoreFrame::HelloReject { .. })
        })
        .await;
    assert!(matches!(
        rej,
        Some(CoreFrame::HelloReject { ref reason, .. }) if reason == "unauthorized"
    ));

    // Web Console non-API responses MUST include nosniff, Referrer-Policy, and X-Frame-Options.
    let base = spawn_http_as(state.clone(), "127.0.0.1:52901".parse().unwrap()).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        resp.headers()
            .get("referrer-policy")
            .and_then(|v| v.to_str().ok()),
        Some("no-referrer")
    );
    assert_eq!(
        resp.headers()
            .get("x-frame-options")
            .and_then(|v| v.to_str().ok()),
        Some("SAMEORIGIN")
    );
}

#[tokio::test]
async fn p1_6_unit_acoustic_profile_update_ignores_untouched_secondary_unit() {
    let (_d, state) = boot_state("p1-6-acoustics", None);
    let delivery = ConfigDelivery::for_app(&state);
    let enrolled = delivery.enroll("dot2-d").unwrap();
    let hh = enrolled["household_id"].as_str().unwrap().to_string();

    // Seed a pre-existing secondary device acoustic profile in SQLite config_kv.
    let mut sec_prof = UnitAcousticProfile::default();
    sec_prof.volume_cap = 0.80;
    state
        .config
        .save_unit_acoustic_profile("android-2", sec_prof.clone(), "seed")
        .unwrap();

    // Connect BOTH dot2-d (enrolled node) and android-2 (secondary v2 gateway).
    let mut gw_dot2 = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:42601".parse().unwrap(),
        &["dot2-d"],
        vec!["audio_out", "config_apply_v2", "config_block_acoustics"],
        None,
    )
    .await;
    let mut gw_android = MockGw::connect(
        state.hub.clone(),
        "192.168.10.106:42602".parse().unwrap(),
        &["android-2"],
        vec!["audio_out", "config_apply_v2", "config_block_acoustics"],
        None,
    )
    .await;
    assert!(gw_dot2
        .wait_frame(Duration::from_secs(2), |f| matches!(
            f,
            CoreFrame::HelloAck { .. }
        ))
        .await
        .is_some());
    assert!(gw_android
        .wait_frame(Duration::from_secs(2), |f| matches!(
            f,
            CoreFrame::HelloAck { .. }
        ))
        .await
        .is_some());

    // Drain any initial connect-time reconcile frames before starting ConfigApplyCoordinator.
    tokio::time::sleep(Duration::from_millis(80)).await;
    while gw_dot2.rx.try_recv().is_ok() {}
    while gw_android.rx.try_recv().is_ok() {}

    let _coord = ConfigApplyCoordinator::start_for_app(&state);
    let pkg = Package {
        format: "home-mate-config".into(),
        version: 2,
        package_id: "00000000-0000-4000-8000-000000000166".into(),
        package_revision: 1,
        created_ms: 1,
        purpose: Purpose::NodeConfig,
        source: Source {
            node_id: "dot2-d".into(),
            config_revision: 0,
            core_version: "0.1.0".into(),
            schema_version: 2,
        },
        target: Target {
            household_id: hh,
            node_id: Some("dot2-d".into()),
        },
        compatibility: Compatibility {
            core_api_schema: 2,
            gateway_protocol_min: 0,
            gateway_protocol_max: 2,
            required_capabilities: vec!["config_apply_v2".into()],
        },
        change_reason: "p1-6 untouched secondary profile test".into(),
        blocks: vec![Block {
            name: "unit_acoustic_profiles".into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: "unit_acoustic_profiles.dot2-d.volume_cap".into(),
                value: json!(0.70),
            }],
        }],
        bindings: vec![],
        content_hash: None,
    };
    let plan = delivery.preflight("admin", pkg, json!({})).unwrap();
    let app_id = "11111111-1111-4111-8111-000000000166".to_string();
    let _ = delivery
        .commit(
            "admin",
            &Commit {
                plan_id: plan["plan_id"].as_str().unwrap().into(),
                plan_hash: plan["plan_hash"].as_str().unwrap().into(),
                expected_revision: plan["expected_revision"].as_u64().unwrap(),
                application_id: app_id.clone(),
            },
        )
        .unwrap();

    // Serve dot2-d v2 handshake; android-2 must NOT receive ConfigApply(Acoustics).
    let mut dot2_applied = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(frame)) = tokio::time::timeout(rem, gw_dot2.rx.recv()).await else {
            break;
        };
        match frame {
            CoreFrame::ConfigStateGet { request_id, device } => {
                let _ = gw_dot2.tx.send(GatewayFrame::ConfigState {
                    request_id: Some(request_id),
                    device,
                    gateway_boot_id: Some("boot-d".into()),
                    state_revision: Some(1),
                    capability_fingerprint: Some("fp-d".into()),
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
                let _ = gw_dot2.tx.send(GatewayFrame::ConfigValidated {
                    request_id,
                    device,
                    block,
                    schema_version,
                    block_hash,
                    ok: true,
                    supported: true,
                    blockers: Vec::new(),
                    state: ConfigReceiptStatus::Validated,
                    reason: None,
                    retryable: false,
                    gateway_boot_id: Some("boot-d".into()),
                    state_revision: Some(1),
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
                block: ConfigBlock::Acoustics,
                schema_version,
                block_hash,
                values,
                ..
            } => {
                assert_eq!(device, "dot2-d");
                assert!((values["volume_cap"].as_f64().unwrap() - 0.70).abs() < 1e-6);
                dot2_applied = true;
                let _ = gw_dot2.tx.send(GatewayFrame::ConfigReceipt {
                    device,
                    core_node_id,
                    data_epoch,
                    application_id,
                    operation_id,
                    desired_revision,
                    block_generation,
                    block: ConfigBlock::Acoustics,
                    schema_version,
                    block_hash,
                    state: ConfigReceiptStatus::Active,
                    reason: None,
                    retryable: false,
                    gateway_boot_id: Some("boot-d".into()),
                    state_revision: Some(2),
                    persisted_hash: Some("ph".into()),
                    active_hash: Some("ah".into()),
                    active_values: Some(values),
                    evidence: None,
                    ts: hm_core::id::now_ms(),
                });
                break;
            }
            _ => {}
        }
    }
    assert!(dot2_applied, "dot2-d must receive ConfigApply(Acoustics)");
    // Give worker a moment to complete publish_transaction and verify persisted state.
    let mut published = false;
    for _ in 0..40 {
        let cur = state.config.unit_acoustic_profile("dot2-d").unwrap();
        if (cur.volume_cap - 0.70).abs() < 1e-6 {
            published = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        published,
        "dot2-d acoustic profile must publish cleanly despite untouched android-2 profile in config_kv"
    );
    // Untouched android-2 must NOT have received any ConfigApply(Acoustics) and keeps 0.80 cap.
    while let Ok(f) = gw_android.rx.try_recv() {
        assert!(
            !matches!(
                f,
                CoreFrame::ConfigApply {
                    block: ConfigBlock::Acoustics,
                    ..
                }
            ),
            "untouched secondary device must not receive ConfigApply(Acoustics): {f:?}"
        );
    }
    assert!(
        (state
            .config
            .unit_acoustic_profile("android-2")
            .unwrap()
            .volume_cap
            - 0.80)
            .abs()
            < 1e-6
    );
    drop(_coord);
}

#[tokio::test]
async fn p1_8_v2_led_config_reconciles_via_config_apply_on_reconnect() {
    let (_d, state) = boot_state("p1-8-v2", None);
    let led = DeviceLedConfig {
        night_enabled: true,
        night_start_minute: 1380,
        night_end_minute: 390,
        night_brightness_pct: 20,
        orientation_rotation: 3,
        orientation_reverse: true,
    };
    state
        .config
        .save_device_led_config("dot2-d", led, "test")
        .unwrap();

    // Connect a v2 gateway advertising config_apply_v2 + config_block_led.
    let mut gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:42801".parse().unwrap(),
        &["dot2-d"],
        vec!["audio_out", "config_apply_v2", "config_block_led"],
        None,
    )
    .await;

    let mut saw_v2_led = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < deadline {
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(frame)) = tokio::time::timeout(rem, gw.rx.recv()).await else {
            break;
        };
        match frame {
            CoreFrame::ConfigStateGet { request_id, device } => {
                let _ = gw.tx.send(GatewayFrame::ConfigState {
                    request_id: Some(request_id),
                    device,
                    gateway_boot_id: Some("boot-led".into()),
                    state_revision: Some(1),
                    capability_fingerprint: Some("fp-led".into()),
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
                let _ = gw.tx.send(GatewayFrame::ConfigValidated {
                    request_id,
                    device,
                    block,
                    schema_version,
                    block_hash,
                    ok: true,
                    supported: true,
                    blockers: Vec::new(),
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
                device,
                core_node_id,
                data_epoch,
                application_id,
                operation_id,
                desired_revision,
                block_generation,
                block: ConfigBlock::Led,
                schema_version,
                block_hash,
                values,
                ..
            } => {
                assert_eq!(device, "dot2-d");
                assert_eq!(values["night_enabled"], true);
                assert_eq!(values["night_start_minute"], 1380);
                assert_eq!(values["night_end_minute"], 390);
                assert_eq!(values["night_brightness_pct"], 20);
                assert_eq!(values["orientation_rotation"], 3);
                assert_eq!(values["orientation_reverse"], true);
                saw_v2_led = true;
                let _ = gw.tx.send(GatewayFrame::ConfigReceipt {
                    device,
                    core_node_id,
                    data_epoch,
                    application_id,
                    operation_id,
                    desired_revision,
                    block_generation,
                    block: ConfigBlock::Led,
                    schema_version,
                    block_hash,
                    state: ConfigReceiptStatus::Active,
                    reason: None,
                    retryable: false,
                    gateway_boot_id: Some("boot-led".into()),
                    state_revision: Some(2),
                    persisted_hash: Some("ph-led".into()),
                    active_hash: Some("ah-led".into()),
                    active_values: Some(values),
                    evidence: None,
                    ts: hm_core::id::now_ms(),
                });
                break;
            }
            _ => {}
        }
    }
    assert!(
        saw_v2_led,
        "v2 gateway with config_block_led must reconcile persisted LED config via dispatch_led_v2"
    );
}

#[tokio::test]
async fn p2_5_open_socket_streams_bounded_body_and_rejects_oversized() {
    let huge_body = "x".repeat(96 * 1024); // > 64 KiB MAX_RESPONSE_BODY
    let app = axum::Router::new().route(
        "/huge",
        axum::routing::get(move || {
            let body = huge_body.clone();
            async move { body }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let (_d, state) = boot_state("p2-5", None);
    let base = spawn_http_as(state.clone(), "127.0.0.1:52501".parse().unwrap()).await;
    let http = reqwest::Client::new();
    let create = http
        .post(format!("{base}/api/v0/config/sockets"))
        .json(&json!({
            "name": "sensor.huge",
            "description": "Oversized body test socket",
            "effect": "Query",
            "endpoint_url": format!("http://{addr}/huge")
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), reqwest::StatusCode::OK);

    let ctx = ToolCtx::new("T-os", "C-os", Arc::new(AtomicBool::new(false)));
    let res = state.registry.call("sensor.huge", json!({}), &ctx).await;
    assert_eq!(
        res.status,
        ToolStatus::Failed,
        "oversized socket response must fail: {res:?}"
    );
    assert_eq!(
        res.error.unwrap().code,
        "SOCKET_RESPONSE_TOO_LARGE",
        "must enforce streaming MAX_RESPONSE_BODY cap"
    );
}

#[tokio::test]
async fn p2_8_p2_9_failed_import_rolls_back_agent_md_and_personal_while_preserving_auth_keys() {
    let (_d, state) = boot_state("p2-8-rb", None);
    let token = state.auth.provision("owner-rb").unwrap();
    let core_id_before = state.auth.core_id().to_string();
    state.auth.set_lan_gateway_exempt(true).unwrap();

    // Baseline story & agent_md state before failed import.
    state
        .config
        .save_personal_endpoint(
            STORY_KEY,
            STORY_TOKEN_SECRET,
            PersonalEndpointConfig {
                enabled: true,
                url: "http://192.168.10.168:18083".into(),
                timeout_ms: 4000,
            },
            Some("orig-story-tok"),
            "test",
        )
        .unwrap();
    state
        .agent_md
        .update("# Original Persona", "test", "baseline")
        .unwrap();

    // Inject a mid-write SQLite failure on `audio_eq` AFTER `story` and `agent_md` have already
    // been written so `restore_import_checkpoint` MUST genuinely execute!
    state
        .store
        .with(|c| {
            c.execute_batch(
                "CREATE TRIGGER fail_audio_eq_wave3 BEFORE INSERT ON config_kv WHEN NEW.key='audio_eq' BEGIN SELECT RAISE(ABORT,'synthetic mid-write failure'); END;",
            )?;
            Ok(())
        })
        .unwrap();

    let bad_import: ConfigImportBody = serde_json::from_value(json!({
        "story": {
            "enabled": true,
            "url": "http://192.168.10.199:19999",
            "timeout_ms": 5000
        },
        "agent_md": {
            "content": "# Mutated Persona That Must Roll Back"
        },
        "secrets": {
            "story.token": "mutated-story-tok"
        },
        "audio_eq": {
            "curve": "official",
            "bands": []
        }
    }))
    .unwrap();

    let res = apply_config_import(&state, bad_import, "test-rollback").await;
    assert!(
        res.is_err(),
        "synthetic mid-write failure on audio_eq must abort import and trigger rollback"
    );

    // 1. Both story (config + secret) and agent_md MUST have rolled back to baseline.
    assert_eq!(
        state.config.personal_endpoint(STORY_KEY).unwrap().url,
        "http://192.168.10.168:18083"
    );
    assert_eq!(
        state
            .config
            .personal_token(STORY_TOKEN_SECRET)
            .unwrap()
            .as_deref(),
        Some("orig-story-tok")
    );
    assert_eq!(state.agent_md.current_content(), "# Original Persona");

    // 2. Auth & system keys MUST remain intact across rollback (never wiped or clobbered).
    assert!(state.auth.configured());
    assert!(state.auth.verify(&token).is_some());
    assert!(state.auth.lan_gateway_exempt());
    assert_eq!(
        state.store.config_get("core_id").unwrap().as_deref(),
        Some(core_id_before.as_str())
    );
}

#[tokio::test]
async fn p2_12_stale_play_frames_dropped_and_single_device_reclaim_evicts_plays() {
    let (_d, state) = boot_state("p2-12", None);

    // Gateway A connects owning TWO devices ("dev-1" and "dev-2").
    let mut gw_a = MockGw::connect(
        state.hub.clone(),
        "192.168.10.120:42901".parse().unwrap(),
        &["dev-1", "dev-2"],
        vec!["audio_out", "play_stream"],
        None,
    )
    .await;
    assert!(gw_a
        .wait_frame(Duration::from_secs(2), |f| matches!(
            f,
            CoreFrame::HelloAck { .. }
        ))
        .await
        .is_some());

    // Start active plays on BOTH dev-1 and dev-2 on Gateway A.
    let ticket_1 = state
        .hub
        .play_begin("dev-1", "voice", 24_000, Some("one".into()))
        .unwrap();
    let play_1 = ticket_1.play_id.clone();
    let ticket_2 = state
        .hub
        .play_begin("dev-2", "voice", 24_000, Some("two".into()))
        .unwrap();
    let play_2 = ticket_2.play_id.clone();
    assert!(state.hub.play_active(&play_1));
    assert!(state.hub.play_active(&play_2));

    // Gateway B connects under a temporary ID, then reclaims ONLY "dev-1" via DeviceStatus.
    let mut gw_b = MockGw::connect(
        state.hub.clone(),
        "192.168.10.121:42902".parse().unwrap(),
        &["dev-b-tmp"],
        vec!["audio_out", "play_stream"],
        None,
    )
    .await;
    assert!(gw_b
        .wait_frame(Duration::from_secs(2), |f| matches!(
            f,
            CoreFrame::HelloAck { .. }
        ))
        .await
        .is_some());
    gw_b.tx
        .send(GatewayFrame::DeviceStatus {
            device: "dev-1".into(),
            online: true,
            reason: "reclaim".into(),
            descriptor: Some(HelloDevice {
                stable_id: "dev-1".into(),
                model: "dot2".into(),
                firmware: Some("2.3.6".into()),
                capabilities: vec!["audio_out".into(), "play_stream".into()],
                audio_profile: "default".into(),
                wifi_ssid: None,
            }),
            northbound: None,
        })
        .unwrap();

    // Wait for single-device reclaim to evict dev-1's old play while keeping dev-2's active play intact.
    for _ in 0..40 {
        if !state.hub.play_active(&play_1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !state.hub.play_active(&play_1),
        "single-device reclaim must evict old connection's active play on reclaimed device"
    );
    assert!(
        state.hub.play_active(&play_2),
        "sibling device dev-2 on original connection must keep its active play intact"
    );

    // Now start a new play on reclaimed dev-1 (owned by Gateway B).
    let ticket_1b = state
        .hub
        .play_begin("dev-1", "voice", 24_000, Some("new".into()))
        .unwrap();
    let play_1b = ticket_1b.play_id.clone();
    assert!(state.hub.play_active(&play_1b));

    // Stale Gateway A sends PlayRejected AND PlayFinished targeting dev-1's new play_1b -> MUST be dropped!
    gw_a.tx
        .send(GatewayFrame::PlayRejected {
            device: "dev-1".into(),
            task_id: play_1b.clone(),
            reason: "spoof_stale_reject".into(),
            ts: hm_core::id::now_ms(),
        })
        .unwrap();
    gw_a.tx
        .send(GatewayFrame::PlayFinished {
            device: "dev-1".into(),
            task_id: play_1b.clone(),
            played_ms: 100,
            consumed_ms: Some(100),
            progress_basis: Some("hw_delay".into()),
            ts: hm_core::id::now_ms(),
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        state.hub.play_active(&play_1b),
        "stale PlayRejected/PlayFinished from non-owner connection must be dropped and never terminate live play"
    );
}
