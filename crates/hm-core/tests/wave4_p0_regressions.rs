//! Wave 4 Core P0 regression suite (`#282`):
//! - Field Bug 1: Wake-slice cursor race when `Keyword(tier="real")` arrives
//!   slightly before flushed preroll PCM under 200 ms arbitration (`speech_start_cursor > cut_cursor + 1`).
//! - Field Bug 2: Leading-edge `tail_blind_only` preroll grazing admits follow-up
//!   turns while pure tail echo and live playback overlap remain `control_only`.
//! - Field Bug 3 / `MG-P1-04` / Field Bug 5 partial: Adaptive `PLAY_AHEAD_SECS`
//!   (20 s loopback vs 3 s non-loopback), `wake_ack` `PlayFinished` refreshes
//!   wake listening window, and `asr_wake` interrupting media pauses cleanly
//!   without `media_takeover_pause_failed`.
//! - `MG-P0-01`: Unbound HTTP `/api/v0/chat`, `/api/v0/voice/announce`, audio/media/
//!   reminder/management tools, and `MasterBrainRuntime` fail closed / skip blast
//!   when multiple devices are online, while scope-bound HTTP `/api/v0/chat`
//!   routes tool execution to the bound device without spoofing `physical_device_id`.
//! - `MG-P0-03`: Under `lan_gateway_exempt`, unauthenticated LAN peers cannot
//!   access/mutate loopback-protected devices (`dot2-d`) or global mode fields
//!   (`barge_in`, `wake_arbitration`, `wake_ack.enabled`), while non-loopback LAN
//!   devices and discovery reads stay usable.
//! - `MG-P0-04`: Non-loopback mobile (`android-v2`) gateways are isolated from
//!   household wake arbitration, passive presence, `arbiter.inbound()`,
//!   sovereignty playback notices, `local_music`, and foreign cue echoes.

mod common;

use axum::extract::ConnectInfo;
use base64::Engine;
use hm_core::api::{router, AppState};
use hm_core::gateway::{CoreFrame, DeviceHub, GatewayFrame, HelloDevice, InputOrigin};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::speech::{AsrClient, SpeechError, TtsAudio, TtsClient};
use hm_core::store::Store;
use hm_core::tool::mocks::MockInject;
use hm_core::tool::music::MediaState;
use hm_core::tool::{ToolCtx, ToolStatus};
use hm_core::voice::{FollowWindowPatch, WakeArbitration};
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
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
            "hm-wave4-{tag}-{}-{}",
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

struct QueueAsr {
    replies: Mutex<VecDeque<String>>,
}
impl QueueAsr {
    fn new(items: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(items.iter().map(|s| (*s).to_string()).collect()),
        })
    }
}
#[async_trait::async_trait]
impl AsrClient for QueueAsr {
    async fn transcribe(&self, _wav: &[u8]) -> Result<String, SpeechError> {
        let next = self.replies.lock().unwrap().pop_front().unwrap_or_default();
        Ok(next)
    }
    fn provider(&self) -> &str {
        "fake-asr"
    }
}

struct DelayedAsr {
    delay: Duration,
    replies: Mutex<VecDeque<String>>,
}
impl DelayedAsr {
    fn new(delay: Duration, items: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            delay,
            replies: Mutex::new(items.iter().map(|s| (*s).to_string()).collect()),
        })
    }
}
#[async_trait::async_trait]
impl AsrClient for DelayedAsr {
    async fn transcribe(&self, _wav: &[u8]) -> Result<String, SpeechError> {
        tokio::time::sleep(self.delay).await;
        let next = self.replies.lock().unwrap().pop_front().unwrap_or_default();
        Ok(next)
    }
    fn provider(&self) -> &str {
        "delayed-fake-asr"
    }
}

struct InstantTts;
#[async_trait::async_trait]
impl TtsClient for InstantTts {
    async fn synthesize(&self, _text: &str) -> Result<TtsAudio, SpeechError> {
        let pcm = vec![0u8; 3200]; // 100ms 16kHz mono silence
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
    async fn connect(
        hub: Arc<DeviceHub>,
        peer: SocketAddr,
        device_id: &str,
        profile_version: Option<&str>,
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
                capabilities: vec![
                    "audio_in".into(),
                    "audio_out".into(),
                    "play_stream".into(),
                    "volume".into(),
                    "remote_mute".into(),
                    "kws".into(),
                    "duplex_full".into(),
                ],
                audio_profile: "default".into(),
                wifi_ssid: None,
            }],
            aec_tail_ms: Some(160),
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
            let mut seq = 10u64;
            let mut media_plays = HashSet::<String>::new();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(cf) = serde_json::from_str::<CoreFrame>(&line) {
                    if auto_confirm_play {
                        let now = hm_core::id::now_ms();
                        match &cf {
                            CoreFrame::PlayBegin {
                                task_id, channel, ..
                            } => {
                                if channel.as_deref() == Some("media") {
                                    media_plays.insert(task_id.clone());
                                }
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
                            }
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
                            CoreFrame::PlayEnd { task_id } => {
                                // Keep media plays live until explicitly stopped, like real audio.
                                if !media_plays.contains(task_id) {
                                    let _ = reply_tx.send(GatewayFrame::PlayFinished {
                                        task_id: task_id.clone(),
                                        device: dev.clone(),
                                        played_ms: 100,
                                        consumed_ms: Some(100),
                                        progress_basis: Some("mock".into()),
                                        ts: now,
                                    });
                                }
                            }
                            CoreFrame::Stop { task_id, .. } => {
                                media_plays.remove(task_id);
                                let _ = reply_tx.send(GatewayFrame::StopReceipt {
                                    task_id: task_id.clone(),
                                    stage: "device_confirmed".into(),
                                    played_ms: Some(80),
                                    consumed_ms: Some(80),
                                    progress_basis: Some("hw_delay".into()),
                                    detail: None,
                                });
                            }
                            CoreFrame::VolumeSet { task_id, value, .. } => {
                                let _ = reply_tx.send(GatewayFrame::VolumeReceipt {
                                    task_id: task_id.clone(),
                                    device: Some(dev.clone()),
                                    effective: *value,
                                    unit: "linear".into(),
                                    stage: "device_confirmed".into(),
                                });
                            }
                            CoreFrame::MuteSet { task_id, muted, .. } => {
                                seq += 1;
                                let _ = reply_tx.send(GatewayFrame::MuteState {
                                    device: dev.clone(),
                                    ts: now,
                                    seq,
                                    muted: *muted,
                                    source: "remote".into(),
                                    task_id: Some(task_id.clone()),
                                });
                            }
                            _ => {}
                        }
                    }
                    let _ = in_tx.send(cf);
                }
            }
        });
        let ack = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(f) = rx.recv().await {
                if matches!(f, CoreFrame::HelloAck { .. }) {
                    return Some(f);
                }
            }
            None
        })
        .await
        .expect("HelloAck timeout");
        assert!(ack.is_some(), "expected HelloAck for {device_id}");
        Self {
            tx,
            rx,
            _srv: srv,
            _r: r,
            _w: w,
        }
    }

    fn send_pcm_blocks(&self, device: &str, seq: &mut u64, count: usize, sample: i16) {
        let pcm: Vec<u8> = std::iter::repeat_n(sample, 1280)
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let pcm_b64 = base64::engine::general_purpose::STANDARD.encode(&pcm);
        for _ in 0..count {
            *seq += 1;
            let _ = self.tx.send(GatewayFrame::AudioChunk {
                device: device.into(),
                origin: InputOrigin::Legacy,
                ts: hm_core::id::now_ms(),
                seq: *seq,
                pcm_b64: pcm_b64.clone(),
                format: "s16le".into(),
                sample_rate: 16_000,
                channels: 1,
                layout: "mono".into(),
                vad_speech: None,
                vad_prob: None,
            });
        }
    }

    fn drain_frames(&mut self) -> Vec<CoreFrame> {
        let mut out = Vec::new();
        while let Ok(f) = self.rx.try_recv() {
            out.push(f);
        }
        out
    }
}

fn boot_state(
    tag: &str,
    llm: Option<Arc<dyn hm_core::llm::LlmClient>>,
    tts: Option<Arc<dyn TtsClient>>,
    asr: Option<Arc<dyn AsrClient>>,
) -> (TempDir, Arc<AppState>, String) {
    let dir = TempDir::new(tag);
    let store = Arc::new(Store::open(&dir.0.join("hm.db")).unwrap());
    store.allow_loopback_public_for_test();
    let state = AppState::with_overrides(
        store,
        dir.0.clone(),
        MockInject::default(),
        llm,
        tts,
        asr,
        None,
    );
    let token = state.auth.provision("wave4-admin").unwrap();
    state.hub.set_voice_gateway_open(true);
    (dir, state, token)
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

/// Bug 1: When a gateway sends `Keyword(tier="real")` before flushed preroll
/// PCM arrives during 200 ms arbitration, `speech_start_cursor > cut_cursor + 1`
/// must classify the segment as `CommandSliced` so ASR admits the post-wake
/// command instead of rejecting it with `kws_wake_rejected_by_asr`.
#[tokio::test]
async fn bug1_wake_slice_cursor_race_under_arbitration_admits_command() {
    let llm = Arc::new(FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["今天晴转多云，二十三度。".into()],
    }]));
    let asr = QueueAsr::new(&["今天天气怎么样"]);
    let (_d, state, token) = boot_state("bug1", Some(llm), Some(Arc::new(InstantTts)), Some(asr));

    // Two household speakers (`v1`) with `WakeArbitration::Enforce` trigger a
    // real 200 ms arbitration wait.
    let dot2_a = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:42101".parse().unwrap(),
        DOT2_ID,
        Some("v1"),
        Some(token.clone()),
        true,
    )
    .await;
    let _dot2_b = MockGw::connect(
        state.hub.clone(),
        "192.168.10.77:42102".parse().unwrap(),
        "dot2-b",
        Some("v1"),
        Some(token),
        true,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    let patch = FollowWindowPatch {
        wake_arbitration: Some(WakeArbitration::Enforce),
        ..Default::default()
    };
    state
        .voice
        .set_mode_with_follow_window(
            "global", None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, patch, "test",
        )
        .unwrap();

    // Gateway sends `Keyword(tier="real")` BEFORE flushing PCM (blocks_seen == 0).
    let mut seq = 1u64;
    dot2_a
        .tx
        .send(GatewayFrame::Keyword {
            device: DOT2_ID.into(),
            ts: hm_core::id::now_ms(),
            seq,
            keyword: "你好高达".into(),
            fed_ms: Some(640),
            self_heard: false,
            rms_dbfs: Some(-14.0),
            lag_ms: Some(94),
            origin: InputOrigin::Legacy,
            bundle_id: None,
            kws_generation: None,
            kws_tier: Some("real".into()),
            wake_dur_ms: Some(600),
            max_gap_ms: Some(120),
            l2_passed: Some(true),
        })
        .unwrap();
    // During the 200 ms arbitration wait, gateway flushes 3 quiet preroll blocks
    // followed by 6 loud command blocks -> `speech_start_cursor = 4 > cut_cursor + 1`.
    tokio::time::sleep(Duration::from_millis(40)).await;
    dot2_a.send_pcm_blocks(DOT2_ID, &mut seq, 3, 0);
    dot2_a.send_pcm_blocks(DOT2_ID, &mut seq, 6, 18_000);
    // Wait for arbitration (200 ms) to finish and call `arm_wake_slice`.
    tokio::time::sleep(Duration::from_millis(260)).await;
    // Now close the segment with quiet hangover blocks.
    dot2_a.send_pcm_blocks(DOT2_ID, &mut seq, 10, 0);

    let mut saw_turn = false;
    let mut saw_rejected = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let log = state.voice.recent(120);
        if log.iter().any(|e| e["kind"] == "kws_wake_rejected_by_asr") {
            saw_rejected = true;
            break;
        }
        if log
            .iter()
            .any(|e| e["kind"] == "turn_end" && e["device"] == DOT2_ID)
        {
            saw_turn = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        !saw_rejected,
        "valid L1+L2 command after early Keyword must NOT be rejected with kws_wake_rejected_by_asr"
    );
    assert!(
        saw_turn,
        "post-wake command must run and complete a turn cleanly"
    );
}

/// Bug 2: Leading-edge `tail_blind_only` preroll/onset blocks after `PlayFinished`
/// must NOT taint a clean follow-up speech segment, whereas pure tail echo inside
/// `tail_blind_ms` remains rejected.
#[tokio::test]
async fn bug2_tail_blind_leading_preroll_admits_followup_and_rejects_pure_echo() {
    let llm = Arc::new(FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["现在是下午三点。".into()],
        },
        Scripted::Text {
            chunks: vec!["好的，明天上午九点提醒你。".into()],
        },
    ]));
    let asr = QueueAsr::new(&["明天九点提醒我", "残余回声"]);
    let (_d, state, token) = boot_state("bug2", Some(llm), Some(Arc::new(InstantTts)), Some(asr));

    let gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:42201".parse().unwrap(),
        DOT2_ID,
        Some("v1"),
        Some(token),
        true,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Enable follow-up window and configure a 1000 ms tail_blind_ms guard so we
    // have generous timing margin to feed leading blocks inside `tail_blind` and
    // subsequent clean speech blocks well after it.
    let fw_patch = FollowWindowPatch {
        follow_window_enabled: Some(true),
        ..Default::default()
    };
    state
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
            Some(1_000),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            fw_patch,
            "test",
        )
        .unwrap();

    // Open session & complete first answer so `play_finished` arms `tail_blind_ms = 1000`.
    gw.tx
        .send(GatewayFrame::Wake {
            device: DOT2_ID.into(),
            ts: hm_core::id::now_ms(),
            seq: 1,
            origin: InputOrigin::Legacy,
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    gw.tx
        .send(GatewayFrame::Utterance {
            device: DOT2_ID.into(),
            ts: hm_core::id::now_ms(),
            seq: 2,
            source: "asr".into(),
            text: "今天天气如何".into(),
            origin: InputOrigin::Legacy,
        })
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if state
            .voice
            .recent(60)
            .iter()
            .any(|e| e["kind"] == "play_finished" && e["device"] == DOT2_ID)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Feed 2 leading blocks comfortably inside the 1000 ms tail_blind window
    // (at ~350 ms), then wait past 1000 ms (+750 ms) and feed 8 clean loud
    // blocks + 10 quiet hangover blocks.
    tokio::time::sleep(Duration::from_millis(350)).await;
    let mut seq = 10u64;
    gw.send_pcm_blocks(DOT2_ID, &mut seq, 1, 0);
    gw.send_pcm_blocks(DOT2_ID, &mut seq, 1, 18_000);
    tokio::time::sleep(Duration::from_millis(750)).await;
    gw.send_pcm_blocks(DOT2_ID, &mut seq, 8, 18_000);
    gw.send_pcm_blocks(DOT2_ID, &mut seq, 10, 0);

    // Second turn MUST be admitted and completed!
    let mut turns = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        turns = state
            .voice
            .recent(120)
            .iter()
            .filter(|e| e["kind"] == "turn_end" && e["device"] == DOT2_ID)
            .count();
        if turns >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        turns >= 2,
        "follow-up that grazed tail_blind_ms on leading preroll must be admitted (turns={turns}, log={:?})",
        state.voice.recent(40)
    );

    // Now test Part B: pure echo captured ENTIRELY inside `tail_blind_ms`
    // right after turn 2's `play_finished` must still be rejected (`control_only = true`).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        let finished = state
            .voice
            .recent(80)
            .iter()
            .filter(|e| e["kind"] == "play_finished" && e["device"] == DOT2_ID)
            .count();
        if finished >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    gw.send_pcm_blocks(DOT2_ID, &mut seq, 3, 18_000); // all 3 loud blocks inside tail_blind
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    gw.send_pcm_blocks(DOT2_ID, &mut seq, 10, 0); // only silence outside tail_blind
    tokio::time::sleep(Duration::from_millis(250)).await;

    let turns_after = state
        .voice
        .recent(120)
        .iter()
        .filter(|e| e["kind"] == "turn_end" && e["device"] == DOT2_ID)
        .count();
    assert_eq!(
        turns_after, turns,
        "pure tail_blind echo with zero clean post-tail speech must NOT open a third turn"
    );
}

/// Bug 3 + MG-P1-04 + Bug 5 partial:
/// 1) Non-loopback gateways cap music play-ahead to ~3 s (`REMOTE_PLAY_AHEAD_SECS`)
///    whereas loopback gateways push up to 20 s (`PLAY_AHEAD_SECS`).
/// 2) `asr_wake` interrupting actively playing music pauses media cleanly
///    (`MediaState::Paused`) without logging `media_takeover_pause_failed`.
#[tokio::test]
async fn bug3_adaptive_play_ahead_and_asr_wake_media_pause() {
    let fake_music = common::spawn_fake_music().await;
    fake_music.duration_secs.store(12, Ordering::SeqCst);

    let llm = Arc::new(FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["好的。".into()],
    }]));
    let asr = QueueAsr::new(&["你好高达"]);
    let (_d, state, token) = boot_state("bug3", Some(llm), Some(Arc::new(InstantTts)), Some(asr));
    common::configure_music(&state.store, &fake_music, true, false, true, true);

    let mut dot2 = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:42301".parse().unwrap(),
        DOT2_ID,
        Some("v1"),
        Some(token.clone()),
        true,
    )
    .await;
    let mut android = MockGw::connect(
        state.hub.clone(),
        "192.168.10.96:42302".parse().unwrap(),
        ANDROID_ID,
        Some("android-v2"),
        Some(token),
        true,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    assert!(state.hub.device_is_loopback(DOT2_ID));
    assert!(!state.hub.device_is_loopback(ANDROID_ID));

    // 1. Adaptive PLAY_AHEAD_SECS: stream a 12 s track to non-loopback Android
    //    vs loopback dot2-d and compare chunks pushed in the first 350 ms.
    let cancel_a = Arc::new(AtomicBool::new(false));
    let ctx_android =
        ToolCtx::new("T-and", "C-and", cancel_a).with_provenance(Some(ANDROID_ID.into()), None);
    let _ = state
        .registry
        .call("music.search", json!({"query":"晚风信笺"}), &ctx_android)
        .await;
    let res_a = state
        .registry
        .call(
            "music.play",
            json!({"source":"library","id":"t01"}),
            &ctx_android,
        )
        .await;
    assert_eq!(res_a.status, ToolStatus::Completed, "{res_a:?}");

    let cancel_d = Arc::new(AtomicBool::new(false));
    let ctx_dot2 =
        ToolCtx::new("T-dot", "C-dot", cancel_d).with_provenance(Some(DOT2_ID.into()), None);
    let _ = state
        .registry
        .call("music.search", json!({"query":"晚风信笺"}), &ctx_dot2)
        .await;
    let res_d = state
        .registry
        .call(
            "music.play",
            json!({"source":"library","id":"t01"}),
            &ctx_dot2,
        )
        .await;
    assert_eq!(res_d.status, ToolStatus::Completed, "{res_d:?}");

    tokio::time::sleep(Duration::from_millis(350)).await;
    let android_chunks = android
        .drain_frames()
        .iter()
        .filter(|f| matches!(f, CoreFrame::PlayChunk { .. }))
        .count();
    let dot2_chunks = dot2
        .drain_frames()
        .iter()
        .filter(|f| matches!(f, CoreFrame::PlayChunk { .. }))
        .count();
    // 40 ms chunks: 3.0 s ≈ 75–90 chunks; 12.0 s ≈ 300 chunks.
    assert!(
        android_chunks <= 120,
        "non-loopback Android play-ahead must stay bounded near 3 s (got {android_chunks} chunks)"
    );
    assert!(
        dot2_chunks > android_chunks + 80,
        "loopback dot2-d must keep larger play-ahead (dot2={dot2_chunks}, android={android_chunks})"
    );

    // 2. Bug 5 partial: `asr_wake` interrupting actively playing music on dot2-d
    //    must pause media cleanly (`MediaState::Paused`) with ZERO `media_takeover_pause_failed`.
    assert_eq!(state.music.media_state(DOT2_ID), Some(MediaState::Playing));
    let mut seq = 100u64;
    dot2.send_pcm_blocks(DOT2_ID, &mut seq, 5, 18_000);
    dot2.send_pcm_blocks(DOT2_ID, &mut seq, 10, 0);

    let mut paused = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if state.music.media_state(DOT2_ID) == Some(MediaState::Paused) {
            paused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        paused,
        "asr_wake interrupting playing music must transition MediaSession to Paused, state={:?}",
        state.music.media_state(DOT2_ID)
    );
    let log = state.voice.recent(120);
    assert!(
        !log.iter()
            .any(|e| e["kind"] == "media_takeover_pause_failed" && e["device"] == DOT2_ID),
        "asr_wake media takeover must NOT emit media_takeover_pause_failed"
    );

    let _ = state.music.control(ANDROID_ID, "stop", Some("media")).await;
}

/// MG-P0-01: With BOTH `dot2-d` and `android` online:
/// - Unbound `/api/v0/voice/announce` (no `device`) fails closed (`400 ANNOUNCE_DEVICE_REQUIRED`).
/// - Unbound audio/media/reminder/management tools fail closed (`AMBIGUOUS_DEVICE`).
/// - Scope-bound HTTP `POST /api/v0/chat` routes `ToolCtx.device_id` to `android`
///   while leaving `dot2-d` untouched and keeping `authority.physical_device_id() == None`.
#[tokio::test]
async fn mg_p0_01_unbound_callers_fail_closed_and_scope_bound_chat_targets_bound_device() {
    let fake_music = common::spawn_fake_music().await;
    let llm = Arc::new(FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "reminder.set".into(),
                json!({ "delay_minutes": 10, "note": "喝水" }).to_string(),
            )],
        },
        Scripted::Text {
            chunks: vec!["已设好10分钟后的提醒。".into()],
        },
    ]));
    let (_d, state, token) = boot_state("mg-p0-01", Some(llm), Some(Arc::new(InstantTts)), None);
    common::configure_music(&state.store, &fake_music, true, false, true, true);

    let mut dot2 = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:42401".parse().unwrap(),
        DOT2_ID,
        Some("v1"),
        Some(token.clone()),
        true,
    )
    .await;
    let mut android = MockGw::connect(
        state.hub.clone(),
        "192.168.10.96:42402".parse().unwrap(),
        ANDROID_ID,
        Some("android-v2"),
        Some(token.clone()),
        true,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    dot2.drain_frames();
    android.drain_frames();

    // Enable proactive speech so announce fails specifically on missing device.
    state
        .voice
        .set_mode_with_follow_window(
            "global",
            None,
            Some(true),
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
            FollowWindowPatch::default(),
            "test",
        )
        .unwrap();

    let base = spawn_http_as(state.clone(), "127.0.0.1:50001".parse().unwrap()).await;
    let http = reqwest::Client::new();

    // 1. Unbound `POST /api/v0/voice/announce` without `device` MUST fail 400.
    let ann_err = http
        .post(format!("{base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(&json!({ "text": "测试播报" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ann_err.status(), reqwest::StatusCode::BAD_REQUEST);
    let ann_body: Value = ann_err.json().await.unwrap();
    assert_eq!(ann_body["code"], json!("ANNOUNCE_DEVICE_REQUIRED"));
    assert!(
        !dot2
            .drain_frames()
            .iter()
            .any(|f| matches!(f, CoreFrame::Play { .. } | CoreFrame::PlayBegin { .. })),
        "unbound announce must NEVER send audio to dot2-d"
    );

    // Explicit `device: ANDROID_ID` succeeds and targets Android only.
    let ann_ok = http
        .post(format!("{base}/api/v0/voice/announce"))
        .bearer_auth(&token)
        .json(&json!({ "text": "只给手机播报", "device": ANDROID_ID }))
        .send()
        .await
        .unwrap();
    assert_eq!(ann_ok.status(), reqwest::StatusCode::ACCEPTED);

    // 2. Unbound audio/media/reminder/management tools (`ctx.device_id == None`)
    //    MUST fail closed when >1 devices are online.
    let unbound_ctx = ToolCtx::new("T-unbound", "C-unbound", Arc::new(AtomicBool::new(false)));
    let _ = state
        .registry
        .call("music.search", json!({"query":"晚风信笺"}), &unbound_ctx)
        .await;
    let r_play = state
        .registry
        .call(
            "music.play",
            json!({"source":"library","id":"t01"}),
            &unbound_ctx,
        )
        .await;
    assert_eq!(r_play.status, ToolStatus::Failed);
    assert_eq!(
        r_play.error.as_ref().map(|e| e.code.as_str()),
        Some("AMBIGUOUS_DEVICE")
    );

    let r_ctrl = state
        .registry
        .call("music.control", json!({"action":"pause"}), &unbound_ctx)
        .await;
    assert_eq!(r_ctrl.status, ToolStatus::Failed);
    assert_eq!(
        r_ctrl.error.as_ref().map(|e| e.code.as_str()),
        Some("AMBIGUOUS_DEVICE")
    );

    let r_rem = state
        .registry
        .call(
            "reminder.set",
            json!({"delay_minutes": 5, "note": "未绑定"}),
            &unbound_ctx,
        )
        .await;
    assert_eq!(r_rem.status, ToolStatus::Failed);
    assert!(
        r_rem
            .error
            .as_ref()
            .is_some_and(|e| e.detail.contains("multiple devices are online")),
        "reminder.set must fail closed on unbound multi-device ctx: {r_rem:?}"
    );

    let r_vol = state
        .registry
        .call("system.volume", json!({"percent": 50}), &unbound_ctx)
        .await;
    assert_eq!(r_vol.status, ToolStatus::Failed);
    assert!(
        r_vol
            .error
            .as_ref()
            .is_some_and(|e| e.detail.contains("AMBIGUOUS_DEVICE")),
        "system.volume must fail closed on unbound multi-device ctx: {r_vol:?}"
    );

    // 3. Scope-bound HTTP `POST /api/v0/chat` (`scope_binding_id` -> ANDROID_ID)
    //    routes `reminder.set` to `ANDROID_ID` while preserving `physical_device_id == None`.
    let principal = state.execution.principal(&token).unwrap();
    let issued = state
        .execution
        .issue(&principal, None, Some(ANDROID_ID.into()), false)
        .unwrap();
    let binding_id = issued["binding_id"].as_str().unwrap().to_string();
    let (resolved_auth, _) = state
        .execution
        .resolve(&binding_id, &principal, None)
        .unwrap();
    assert_eq!(
        resolved_auth.physical_device_id(),
        None,
        "HTTP scope binding must keep physical_device_id == None for memory provenance"
    );

    let chat_resp = http
        .post(format!("{base}/api/v0/chat"))
        .bearer_auth(&token)
        .json(&json!({
            "scope_binding_id": binding_id,
            "text": "十分钟后提醒我喝水"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(chat_resp.status(), reqwest::StatusCode::OK);
    let _sse = chat_resp.text().await.unwrap();

    let reminders_resp: Value = http
        .get(format!("{base}/api/v0/reminders"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let reminders = reminders_resp["reminders"].as_array().unwrap();
    assert_eq!(
        reminders.len(),
        1,
        "scope-bound chat must create 1 reminder"
    );
    assert_eq!(
        reminders[0]["device"],
        json!(ANDROID_ID),
        "scope-bound chat tool must target the bound Android device, NEVER dot2-d"
    );
}

/// MG-P0-03: Under `lan_gateway_exempt = true`, unauthenticated private-LAN
/// peers MUST be rejected with 401 on EVERY loopback speaker (`dot2-d`) route
/// AND on global mode fields (`barge_in`, `wake_arbitration`, `wake_ack.enabled`),
/// while non-loopback LAN devices and read-only discovery remain accessible.
#[tokio::test]
async fn mg_p0_03_lan_gateway_exempt_protects_loopback_speaker_and_global_mode_fields() {
    let (_d, state, token) = boot_state("mg-p0-03", None, None, None);
    state.auth.set_lan_gateway_exempt(true).unwrap();

    let _dot2 = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:42501".parse().unwrap(),
        DOT2_ID,
        Some("v1"),
        Some(token.clone()),
        true,
    )
    .await;
    let _android = MockGw::connect(
        state.hub.clone(),
        "192.168.10.96:42502".parse().unwrap(),
        ANDROID_ID,
        Some("android-v2"),
        Some(token.clone()),
        true,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Seed a real device context session on dot2-d AND a prior-epoch opaque
    // `device_contexts` row plus `dev:scope11:` / `dev:context12:` sessions so
    // we prove ALL physical session namespaces and prior epochs are blocked.
    let dot2_ctx = state
        .sessions
        .admit_device_context(DOT2_ID, hm_core::id::now_ms(), 300_000, false)
        .unwrap();
    let scope11_sid = format!("dev:scope11:{DOT2_ID}:1");
    let ctx12_old_sid = format!("dev:context12:{DOT2_ID}:ctx-old");
    let opaque_old_sid = "opaque-old-dot2-ctx".to_string();
    state.sessions.ensure(&scope11_sid).unwrap();
    state.sessions.ensure(&ctx12_old_sid).unwrap();
    state.sessions.ensure(&opaque_old_sid).unwrap();
    state
        .store
        .with(|c| {
            // Use `data_epoch = -1` (strictly prior to current epoch 0) to prove
            // prior-epoch `device_contexts` rows are still blocked.
            c.execute(
                "INSERT INTO device_contexts(session_id,device,data_epoch,created_ms,last_activity_ms) VALUES (?1,?2,-1,1000,1000)",
                rusqlite::params![&opaque_old_sid, DOT2_ID],
            )?;
            Ok(())
        })
        .unwrap();

    // Seed running tasks for dot2-d (both `dev:` and `dev:scope11:`) and Android
    // to prove `POST /api/v0/session/cancel` blocks loopback tasks under `lan_gateway_exempt`.
    let dot2_task = state.tasks.create(&format!("dev:{DOT2_ID}")).unwrap();
    let dot2_scope11_task = state.tasks.create(&scope11_sid).unwrap();
    let android_task = state.tasks.create(&format!("dev:{ANDROID_ID}")).unwrap();

    let base = spawn_http_as(state.clone(), "192.168.10.88:55555".parse().unwrap()).await;
    let http = reqwest::Client::new();

    // Every per-device & session route targeting loopback-protected `dot2-d` MUST return 401.
    assert_eq!(
        http.get(format!("{base}/api/v0/devices/{DOT2_ID}/conversation"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    for protected_sid in [
        format!("dev:{DOT2_ID}"),
        dot2_ctx.session_id.clone(),
        scope11_sid.clone(),
        ctx12_old_sid.clone(),
        opaque_old_sid.clone(),
    ] {
        assert_eq!(
            http.get(format!("{base}/api/v0/session/{protected_sid}/messages"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "unauthenticated LAN GET /session/{protected_sid}/messages must return 401"
        );
    }

    // Blocker #2: `POST /api/v0/session/cancel` must return 401 and leave
    // loopback speaker tasks uncancelled under `lan_gateway_exempt`.
    for task in [&dot2_task, &dot2_scope11_task] {
        let resp = http
            .post(format!("{base}/api/v0/session/cancel"))
            .json(&json!({"task_id": task.id}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "unauthenticated LAN cancel of dot2-d task {} must return 401",
            task.id
        );
        assert!(
            !task.is_cancelled(),
            "rejected cancel must NOT flip cancellation flag on dot2-d task {}",
            task.id
        );
    }
    // Cancelling a non-loopback Android task under `lan_gateway_exempt` succeeds.
    let android_cancel = http
        .post(format!("{base}/api/v0/session/cancel"))
        .json(&json!({"task_id": android_task.id}))
        .send()
        .await
        .unwrap();
    assert_eq!(android_cancel.status(), reqwest::StatusCode::OK);
    assert!(android_task.is_cancelled());
    for (method, path, body) in [
        ("POST", format!("/api/v0/devices/{DOT2_ID}/stop"), json!({})),
        (
            "POST",
            format!("/api/v0/devices/{DOT2_ID}/volume"),
            json!({"value": 0.4}),
        ),
        (
            "POST",
            format!("/api/v0/devices/{DOT2_ID}/program/pause"),
            json!({}),
        ),
        (
            "POST",
            format!("/api/v0/devices/{DOT2_ID}/program/resume"),
            json!({}),
        ),
        (
            "POST",
            format!("/api/v0/devices/{DOT2_ID}/media/control"),
            json!({"action": "pause"}),
        ),
        (
            "PUT",
            format!("/api/v0/devices/{DOT2_ID}/mute"),
            json!({"muted": true}),
        ),
        (
            "PUT",
            format!("/api/v0/mode/{DOT2_ID}"),
            json!({"duplex_requested": "full"}),
        ),
        (
            "POST",
            "/api/v0/session/close".to_string(),
            json!({"device": DOT2_ID}),
        ),
    ] {
        let req = match method {
            "POST" => http.post(format!("{base}{path}")).json(&body),
            "PUT" => http.put(format!("{base}{path}")).json(&body),
            _ => unreachable!(),
        };
        let resp = req.send().await.unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "unauthenticated LAN {method} {path} on loopback dot2-d must return 401"
        );
    }

    // Global mode fields (`barge_in`, `wake_arbitration`, `wake_ack.enabled`)
    // MUST be rejected 401 even when targeting non-loopback `android`.
    for payload in [
        json!({"barge_in": false}),
        json!({"wake_arbitration": "off"}),
        json!({"wake_ack": {"enabled": false}}),
    ] {
        let resp = http
            .put(format!("{base}/api/v0/mode/{ANDROID_ID}"))
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "privileged global mode payload on android must return 401: {payload}"
        );
    }

    // Non-loopback Android everyday routes AND read-only discovery stay 200.
    for get_path in [
        "/api/v0/status",
        "/api/v0/mode",
        "/api/v0/devices",
        "/api/v0/device-meta",
        &format!("/api/v0/devices/{ANDROID_ID}/conversation"),
    ] {
        assert_eq!(
            http.get(format!("{base}{get_path}"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK,
            "GET {get_path} must stay 200 under lan_gateway_exempt"
        );
    }
    assert_eq!(
        http.put(format!("{base}/api/v0/mode/{ANDROID_ID}"))
            .json(&json!({"duplex_requested": "full"}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        http.post(format!("{base}/api/v0/devices/{ANDROID_ID}/stop"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );

    // Admin Bearer token retains full access to loopback `dot2-d` (conversations,
    // all session namespaces, prior-epoch contexts, and task cancellation).
    assert_eq!(
        http.get(format!("{base}/api/v0/devices/{DOT2_ID}/conversation"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    for protected_sid in [&scope11_sid, &ctx12_old_sid, &opaque_old_sid] {
        assert_eq!(
            http.get(format!("{base}/api/v0/session/{protected_sid}/messages"))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
    }
    let admin_cancel = http
        .post(format!("{base}/api/v0/session/cancel"))
        .bearer_auth(&token)
        .json(&json!({"task_id": dot2_task.id}))
        .send()
        .await
        .unwrap();
    assert_eq!(admin_cancel.status(), reqwest::StatusCode::OK);
    assert!(dot2_task.is_cancelled());
}

/// MG-P0-04: Non-loopback mobile (`android-v2`) gateways sit outside the fixed
/// household speaker acoustic domain:
/// - No wake arbitration delay or cross-room contention with `dot2-d`.
/// - `NearEndPresence` from `android-v2` never enters `arbiter.passive_presence`
///   and is never returned by `arbiter.inbound()`.
/// - Android playback/cues never populate `sovereignty` notices or trigger bleed
///   suppression against `dot2-d`, and `dot2-d` playback never suppresses Android.
#[tokio::test]
async fn mg_p0_04_mobile_android_v2_isolated_from_household_acoustic_domain() {
    let (_d, state, token) = boot_state("mg-p0-04", None, Some(Arc::new(InstantTts)), None);

    let dot2 = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:42601".parse().unwrap(),
        DOT2_ID,
        Some("v1"),
        Some(token.clone()),
        true,
    )
    .await;
    let android = MockGw::connect(
        state.hub.clone(),
        "192.168.10.96:42602".parse().unwrap(),
        ANDROID_ID,
        Some("android-v2"),
        Some(token),
        true,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    assert!(state.voice.device_in_household_acoustic_domain(DOT2_ID));
    assert!(!state.voice.device_in_household_acoustic_domain(ANDROID_ID));
    assert_eq!(state.hub.online_device_count(), 2);
    assert_eq!(state.hub.online_household_acoustic_device_count(), 1);

    // Enable Enforce arbitration.
    let patch = FollowWindowPatch {
        wake_arbitration: Some(WakeArbitration::Enforce),
        ..Default::default()
    };
    state
        .voice
        .set_mode_with_follow_window(
            "global", None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, patch, "test",
        )
        .unwrap();

    // 1. Loud `NearEndPresence` from `android-v2` must NOT enter `arbiter.passive_presence`
    //    and `arbiter.inbound()` from a remote Dot2 peer must NOT return Android.
    let now = hm_core::id::now_ms();
    android
        .tx
        .send(GatewayFrame::NearEndPresence {
            device: ANDROID_ID.into(),
            ts_ms: now,
            duration_ms: 480,
            rms_dbfs: Some(-10.0),
            noise_floor_dbfs: Some(-55.0),
            snr_db: Some(45.0),
            clipped: false,
            echo_health: Some("healthy".into()),
            profile_version: Some("android-v2".into()),
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        state
            .voice
            .arbiter
            .passive_presence_for(ANDROID_ID)
            .is_none(),
        "mobile android-v2 must NEVER inject passive_presence into household arbiter"
    );
    let inbound = state
        .voice
        .arbiter
        .inbound(hm_core::arbitration::Claim::new(
            "dot2-remote",
            now as i64,
            Some(-30.0),
        ));
    assert!(
        inbound["claim"].is_null(),
        "arbiter.inbound() must NEVER return mobile android-v2 claim: {inbound}"
    );

    // 2. Simultaneous `Keyword` on BOTH `dot2-d` (-28 dBFS) and `android-v2` (-10 dBFS):
    //    neither device may veto the other, and Android wakes immediately (< 180 ms,
    //    proving it skipped the 200 ms arbitration window).
    let t0 = std::time::Instant::now();
    android
        .tx
        .send(GatewayFrame::Keyword {
            device: ANDROID_ID.into(),
            ts: hm_core::id::now_ms(),
            seq: 1,
            keyword: "你好高达".into(),
            fed_ms: Some(500),
            self_heard: false,
            rms_dbfs: Some(-10.0),
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
    dot2.tx
        .send(GatewayFrame::Keyword {
            device: DOT2_ID.into(),
            ts: hm_core::id::now_ms(),
            seq: 1,
            keyword: "你好高达".into(),
            fed_ms: Some(500),
            self_heard: false,
            rms_dbfs: Some(-28.0),
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

    let mut android_woke_ms = None;
    let mut dot2_woke = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        let log = state.voice.recent(80);
        if android_woke_ms.is_none()
            && log
                .iter()
                .any(|e| e["kind"] == "keyword_wake" && e["device"] == ANDROID_ID)
        {
            android_woke_ms = Some(t0.elapsed().as_millis());
        }
        if log
            .iter()
            .any(|e| e["kind"] == "keyword_wake" && e["device"] == DOT2_ID)
        {
            dot2_woke = true;
        }
        if android_woke_ms.is_some() && dot2_woke {
            break;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert!(
        android_woke_ms.is_some_and(|ms| ms < 180),
        "mobile android-v2 must wake immediately without 200 ms arbitration wait, got {android_woke_ms:?} ms"
    );
    assert!(
        dot2_woke,
        "dot2-d must NOT be vetoed by louder mobile android-v2"
    );
    let log = state.voice.recent(80);
    assert!(
        !log.iter().any(|e| e["kind"] == "arbitration_lost"),
        "neither dot2-d nor mobile android-v2 should lose arbitration to each other: {log:?}"
    );

    // 3. Sovereignty & fleet bleed isolation:
    //    `dot2-d` active playback notice must NEVER bleed-suppress `android-v2`.
    state
        .voice
        .sovereignty
        .record_playback_notice(hm_core::sovereignty::FleetPlaybackNotice {
            device: DOT2_ID.into(),
            room: "living".into(),
            channel: "voice".into(),
            active: true,
            text_preview: "你好高达".into(),
            at_ms: hm_core::id::now_ms(),
            ttl_ms: 5_000,
        });
    android
        .tx
        .send(GatewayFrame::Keyword {
            device: ANDROID_ID.into(),
            ts: hm_core::id::now_ms(),
            seq: 2,
            keyword: "你好高达".into(),
            fed_ms: Some(500),
            self_heard: false,
            rms_dbfs: Some(-35.0),
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
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        !state.voice.recent(80).iter().any(|e| {
            e["kind"] == "bleed_suppressed_by_fleet_playback" && e["device"] == ANDROID_ID
        }),
        "dot2-d playback notice must NEVER bleed-suppress mobile android-v2"
    );
}

/// Blocker #1 regression: a `tail_blind_only`-grazed follow-up whose onset was
/// captured while the follow-up window was open (`frozen_context.is_some()`)
/// must STILL be admitted even when the follow-up window expires before ASR
/// finishes (neither line 881 nor line 960 may drop it as `followup_credits_exhausted`).
#[tokio::test]
async fn bug2_tail_blind_followup_survives_window_expiry_during_asr() {
    let llm = Arc::new(FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["现在外面是晴天。".into()],
        },
        Scripted::Text {
            chunks: vec!["好的，明天七点叫你。".into()],
        },
    ]));
    // Delay ASR by 4.3 s so the 5.0 s follow-up window expires strictly BEFORE
    // ASR returns (t = ~1.1 s speech close + 4.3 s ASR = 5.4 s > 5.0 s).
    let asr = DelayedAsr::new(Duration::from_millis(4_300), &["明天七点叫我起床"]);
    let (_d, state, token) = boot_state(
        "bug2-expiry",
        Some(llm),
        Some(Arc::new(InstantTts)),
        Some(asr),
    );

    let gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:42701".parse().unwrap(),
        DOT2_ID,
        Some("v1"),
        Some(token),
        true,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    let fw_patch = FollowWindowPatch {
        follow_window_enabled: Some(true),
        follow_window_ms: Some(5_000),
        ..Default::default()
    };
    state
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
            Some(1_000),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            fw_patch,
            "test",
        )
        .unwrap();

    // Complete turn 1 so `play_finished` arms `tail_blind_ms = 1000` and opens
    // a 5000 ms follow-up window.
    gw.tx
        .send(GatewayFrame::Wake {
            device: DOT2_ID.into(),
            ts: hm_core::id::now_ms(),
            seq: 1,
            origin: InputOrigin::Legacy,
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    gw.tx
        .send(GatewayFrame::Utterance {
            device: DOT2_ID.into(),
            ts: hm_core::id::now_ms(),
            seq: 2,
            source: "asr".into(),
            text: "今天天气怎么样".into(),
            origin: InputOrigin::Legacy,
        })
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if state
            .voice
            .recent(60)
            .iter()
            .any(|e| e["kind"] == "play_finished" && e["device"] == DOT2_ID)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Feed 2 leading blocks inside `tail_blind_ms` (~300 ms after play_finished),
    // then wait past `tail_blind_ms` (+850 ms) and close with 8 clean speech + 10 quiet blocks.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut seq = 10u64;
    gw.send_pcm_blocks(DOT2_ID, &mut seq, 1, 0);
    gw.send_pcm_blocks(DOT2_ID, &mut seq, 1, 18_000);
    tokio::time::sleep(Duration::from_millis(850)).await;
    gw.send_pcm_blocks(DOT2_ID, &mut seq, 8, 18_000);
    gw.send_pcm_blocks(DOT2_ID, &mut seq, 10, 0);

    // Wait for delayed ASR (4.3 s) + turn 2 completion.
    let mut turns = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        turns = state
            .voice
            .recent(120)
            .iter()
            .filter(|e| e["kind"] == "turn_end" && e["device"] == DOT2_ID)
            .count();
        if turns >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let log = state.voice.recent(80);
    assert!(
        turns >= 2,
        "tail_blind-grazed follow-up whose window expired during ASR must still complete (turns={turns}, log={log:?})"
    );
    assert!(
        !log.iter().any(|e| e["kind"] == "mic_ignored_unwoken"),
        "admitted tail_blind follow-up must NOT be dropped as mic_ignored_unwoken: {log:?}"
    );
}

/// Item 7 E2E: `wake_ack` `PlayFinished` flowing through `pump.rs` refreshes
/// the wake listening window and renews sovereignty, and NEVER reopens a closed
/// session.
#[tokio::test]
async fn bug3_wake_ack_play_finished_refreshes_listening_window_and_renews_sovereignty() {
    let (_d, state, token) = boot_state("bug3-ack", None, Some(Arc::new(InstantTts)), None);

    // Connect with `auto_confirm_play = false` so we control exact `PlayFinished` timing.
    let mut gw = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:42801".parse().unwrap(),
        DOT2_ID,
        Some("v1"),
        Some(token),
        false,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Synthesize a local wake_ack phrase with `InstantTts` and enable a 5 s follow window.
    let fw_patch = FollowWindowPatch {
        follow_window_enabled: Some(true),
        follow_window_ms: Some(5_000),
        ..Default::default()
    };
    state
        .voice
        .set_mode_with_follow_window(
            "global",
            None,
            None,
            None,
            None,
            None,
            Some(hm_core::voice::WakeAckPatch {
                enabled: Some(true),
                phrases: Some(vec!["我在".into()]),
                resynth: Some(true),
            }),
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
            fw_patch,
            "test",
        )
        .unwrap();
    let synth_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < synth_deadline {
        if state
            .voice
            .recent(40)
            .iter()
            .any(|e| e["kind"] == "wake_ack_synth")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Close a short bare-wake speech segment so `Keyword` sees `bare_wake_closed = true`
    // and speaks the `wake_ack` immediately.
    let mut seq = 1u64;
    gw.send_pcm_blocks(DOT2_ID, &mut seq, 4, 18_000);
    gw.send_pcm_blocks(DOT2_ID, &mut seq, 10, 0);
    tokio::time::sleep(Duration::from_millis(50)).await;
    gw.drain_frames();

    seq += 1;
    gw.tx
        .send(GatewayFrame::Keyword {
            device: DOT2_ID.into(),
            ts: hm_core::id::now_ms(),
            seq,
            keyword: "你好高达".into(),
            fed_ms: Some(400),
            self_heard: false,
            rms_dbfs: Some(-18.0),
            lag_ms: Some(40),
            origin: InputOrigin::Legacy,
            bundle_id: None,
            kws_generation: None,
            kws_tier: None,
            wake_dur_ms: None,
            max_gap_ms: None,
            l2_passed: None,
        })
        .unwrap();

    // Wait for Core to send the `wake_ack` play frame and confirm receipt (without finishing yet).
    let ack_play_id = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(f) = gw.rx.recv().await {
            match f {
                CoreFrame::PlayBegin { task_id, .. } | CoreFrame::Play { task_id, .. } => {
                    return Some(task_id);
                }
                _ => {}
            }
        }
        None
    })
    .await
    .expect("wake_ack play timeout")
    .expect("expected wake_ack play frame");

    let now0 = hm_core::id::now_ms();
    let _ = gw.tx.send(GatewayFrame::PlayReceipt {
        task_id: ack_play_id.clone(),
        stage: "accepted".into(),
        detail: None,
        ts: now0,
    });
    let _ = gw.tx.send(GatewayFrame::PlayReceipt {
        task_id: ack_play_id.clone(),
        stage: "device_confirmed".into(),
        detail: None,
        ts: now0,
    });
    tokio::time::sleep(Duration::from_millis(40)).await;

    let before_owner = state
        .voice
        .sovereignty
        .owner_for_room("", DOT2_ID, hm_core::id::now_ms())
        .expect("sovereignty owner must exist after wake");
    let before_until = before_owner.active_voice_until_ms;

    // Simulate slow LAN/Tailscale playback completion 500 ms later.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = gw.tx.send(GatewayFrame::PlayFinished {
        device: DOT2_ID.into(),
        task_id: ack_play_id.clone(),
        played_ms: 500,
        consumed_ms: Some(500),
        progress_basis: Some("hw_delay".into()),
        ts: hm_core::id::now_ms(),
    });
    tokio::time::sleep(Duration::from_millis(120)).await;

    let after_owner = state
        .voice
        .sovereignty
        .owner_for_room("", DOT2_ID, hm_core::id::now_ms())
        .expect("sovereignty owner must stay active after wake_ack PlayFinished");
    assert!(
        after_owner.active_voice_until_ms >= before_until + 400,
        "wake_ack PlayFinished must refresh listening window and renew sovereignty (before={before_until}, after={})",
        after_owner.active_voice_until_ms
    );

    // Explicitly close session, then inject a late PlayFinished: session MUST stay idle.
    state.voice.close_session(DOT2_ID, None).unwrap();
    let _ = gw.tx.send(GatewayFrame::PlayFinished {
        device: DOT2_ID.into(),
        task_id: ack_play_id,
        played_ms: 500,
        consumed_ms: Some(500),
        progress_basis: Some("hw_delay".into()),
        ts: hm_core::id::now_ms(),
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    let snap = state.voice.mode_snapshot();
    let dev_snap = snap["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == json!(DOT2_ID))
        .unwrap();
    assert_eq!(
        dev_snap["session"]["state"],
        json!("idle"),
        "late wake_ack PlayFinished must NEVER reopen an explicitly closed session"
    );
}

/// Item 7 E2E: `asr_wake` combined with a non-wake reflex command (e.g.
/// `"你好高达，下一首"`) while music is playing must NOT pre-pause media via
/// `pause_media_before_wake_activate`, and instead executes the track advance cleanly.
#[tokio::test]
async fn bug5_asr_wake_non_wake_reflex_does_not_pre_pause_media() {
    let fake_music = common::spawn_fake_music().await;
    fake_music.duration_secs.store(12, Ordering::SeqCst);

    let asr = QueueAsr::new(&["你好高达，下一首"]);
    let (_d, state, token) = boot_state("bug5-reflex", None, Some(Arc::new(InstantTts)), Some(asr));
    common::configure_music(&state.store, &fake_music, true, false, true, true);

    let dot2 = MockGw::connect(
        state.hub.clone(),
        "127.0.0.1:42901".parse().unwrap(),
        DOT2_ID,
        Some("v1"),
        Some(token),
        true,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Start a 2-track queue on dot2-d (`t01`, `t02`).
    let ctx = ToolCtx::new("T-q", "C-q", Arc::new(AtomicBool::new(false)))
        .with_provenance(Some(DOT2_ID.into()), None);
    let _ = state
        .registry
        .call("music.search", json!({"query": "*"}), &ctx)
        .await;
    let play_res = state
        .registry
        .call(
            "music.play",
            json!({"source": "library", "id": "t01", "queue_ids": ["t01", "t02"]}),
            &ctx,
        )
        .await;
    assert_eq!(play_res.status, ToolStatus::Completed, "{play_res:?}");
    assert_eq!(state.music.media_state(DOT2_ID), Some(MediaState::Playing));

    // Speak `"你好高达，下一首"` via ASR while music is playing.
    let mut seq = 50u64;
    dot2.send_pcm_blocks(DOT2_ID, &mut seq, 5, 18_000);
    dot2.send_pcm_blocks(DOT2_ID, &mut seq, 10, 0);

    // Wait for `reflex_text` (`next`) and next track `t02` to start playing.
    let mut saw_next = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let log = state.voice.recent(120);
        if log
            .iter()
            .any(|e| e["kind"] == "music_play_start" && e["device"] == DOT2_ID && e["id"] == "t02")
        {
            saw_next = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let log = state.voice.recent(120);
    assert!(
        saw_next,
        "asr_wake + non-wake reflex '下一首' must advance cleanly to track t02: {log:?}"
    );
    assert!(
        !log.iter()
            .any(|e| e["kind"] == "media_paused" && e["why"] == "wake_takeover"),
        "non-wake reflex command must NOT trigger pre-activate wake_takeover pause: {log:?}"
    );
    assert_eq!(
        state.music.media_state(DOT2_ID),
        Some(MediaState::Playing),
        "music must remain Playing on the next track rather than Paused"
    );
}
