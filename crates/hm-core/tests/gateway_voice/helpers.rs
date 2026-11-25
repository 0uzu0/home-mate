use super::*;

pub struct FakeGateway {
    pub generation: std::sync::atomic::AtomicU64,
    pub capture_epoch: String,
    pub tx: mpsc::Sender<CoreFrame>,
    pub rx: Mutex<mpsc::Receiver<CoreFrame>>,
    pub write: Mutex<Option<tokio::net::tcp::OwnedWriteHalf>>,
}

/// Media app + a scripted ASR: the transcript arrives through the REAL mic
/// path (uplink audio → VAD → ASR), because the KWS/ASR race is about their
/// timing, not about a shortcut.
/// These suites exercise the follow window itself (answer tails, credits,
/// follow-up questions, barge-in resumption) — the participation MECHANISM.
///
/// Since 2026-11-15 a fresh node CLOSES after each answer (the user's "我不该
/// 为了让它闭嘴去说退下吧"), so a suite that means to test the window has to
/// ask for it, exactly as a household would. The product default is covered by
/// `an_answer_ends_the_conversation_by_default`.
pub fn enable_follow_window(state: &Arc<AppState>) {
    let _ = state.voice.set_mode_with_follow_window(
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
            follow_window_enabled: Some(true),
            ..Default::default()
        },
        "test",
    );
}

pub async fn spawn_app_with_gateway_media_asr(
    llm: FakeLlm,
    asr: Arc<dyn hm_core::speech::AsrClient>,
) -> (
    String,
    Arc<AppState>,
    Arc<FakeGateway>,
    Arc<common::FakeMusic>,
) {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm)),
        Some(Arc::new(MockTts::default())),
        Some(asr),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    let fake = common::spawn_fake_music().await;
    common::configure_music(&state.store, &fake, true, false, true, true);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let gw = connect_gateway_audio_in_caps(gw_port, vec!["play_stream"]).await;
    let _ = state.voice.set_mode(
        "dot2-d",
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
        Some(no_filler()),
        None,
        None,
        "test",
    );
    enable_follow_window(&state);
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state, gw, Arc::new(fake))
}

impl FakeGateway {
    /// Drop the socket from the gateway side: the core must see EOF, mark
    /// devices offline and release waiters (the real disconnect path).
    pub async fn close(&self) {
        self.write.lock().await.take();
    }
}

impl FakeGateway {
    pub async fn send(&self, f: &GatewayFrame) {
        let line = serde_json::to_string(f).unwrap();
        let mut guard = self.write.lock().await;
        let w = guard.as_mut().expect("connection closed");
        w.write_all(line.as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
    }

    pub async fn recv(&self) -> CoreFrame {
        self.rx
            .lock()
            .await
            .recv()
            .await
            .expect("core frame expected")
    }

    /// Collect until a Play on `channel` arrives (answering nothing yet).
    pub async fn until_play(&self, channel: &str) -> (String, Value) {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), self.recv()).await {
                Ok(CoreFrame::Play {
                    task_id,
                    channel: ch,
                    text,
                    simulated_ms,
                    audio_b64,
                    ..
                }) => {
                    if ch.as_deref() == Some(channel) {
                        return (
                            task_id,
                            json!({ "text": text, "simulated_ms": simulated_ms, "has_audio": audio_b64.is_some() }),
                        );
                    }
                }
                Ok(CoreFrame::PlayBegin {
                    task_id,
                    channel: ch,
                    text,
                    ..
                }) => {
                    if ch.as_deref() == Some(channel) {
                        return (
                            task_id,
                            json!({ "text": text, "simulated_ms": Value::Null, "has_audio": true }),
                        );
                    }
                }
                Ok(_) => continue,
                Err(_) => panic!("timeout waiting for {channel} play"),
            }
        }
    }

    /// Collect until a frame matching pred arrives (skip-latest pattern).
    pub async fn until(&self, pred: impl Fn(&CoreFrame) -> bool) -> CoreFrame {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let f = tokio::time::timeout_at(deadline, self.rx.lock().await.recv())
                .await
                .expect("timeout waiting for expected frame")
                .expect("connection died");
            if pred(&f) {
                return f;
            }
        }
    }

    /// Wait for the next voice play of EITHER shape: a whole `Play` (speak) or a
    /// streamed `PlayBegin` (play_stream). Returns (task/play id, text).
    #[allow(clippy::type_complexity)]
    pub async fn until_any_voice_play(&self) -> (String, String) {
        let f = self
            .until(|f| {
                matches!(f, CoreFrame::Play { channel: Some(c), .. } if c == "voice")
                    || matches!(f, CoreFrame::PlayBegin { .. })
            })
            .await;
        match f {
            CoreFrame::Play { task_id, text, .. } => (task_id, text.unwrap_or_default()),
            CoreFrame::PlayBegin { task_id, text, .. } => (task_id, text.unwrap_or_default()),
            other => panic!("not a play: {other:?}"),
        }
    }

    /// Wait for the media play in EITHER shape — a whole `Play` or a streamed
    /// `PlayBegin` (music). Returns (id, text) and confirms it like a device.
    pub async fn until_media_play(&self) -> (String, String) {
        self.until_media_play_opts(true).await
    }

    pub async fn until_media_play_opts(&self, confirm: bool) -> (String, String) {
        let f = self
            .until(|f| {
                matches!(f, CoreFrame::Play { channel: Some(c), .. } if c == "media")
                    || matches!(f, CoreFrame::PlayBegin { channel: Some(c), .. } if c == "media")
            })
            .await;
        let (id, text) = match f {
            CoreFrame::Play { task_id, text, .. } => (task_id, text.unwrap_or_default()),
            CoreFrame::PlayBegin { task_id, text, .. } => (task_id, text.unwrap_or_default()),
            other => panic!("not a play: {other:?}"),
        };
        if confirm {
            self.confirm_play(&id).await;
        }
        (id, text)
    }

    /// Receipt pair the virtual device sends for an accepted play.
    pub async fn confirm_play(&self, play_id: &str) {
        let now = hm_core::id::now_ms();
        self.send(&GatewayFrame::PlayReceipt {
            task_id: play_id.into(),
            stage: "accepted".into(),
            detail: None,
            ts: now,
        })
        .await;
        self.send(&GatewayFrame::PlayReceipt {
            task_id: play_id.into(),
            stage: "device_confirmed".into(),
            detail: None,
            ts: now,
        })
        .await;
    }

    /// The stop receipt a real device sends only AFTER its flush (R0).
    pub async fn confirm_stop(&self, play_id: &str) {
        self.send(&GatewayFrame::StopReceipt {
            task_id: play_id.into(),
            stage: "device_confirmed".into(),
            played_ms: Some(120),

            consumed_ms: None,
            progress_basis: None,
            detail: None,
        })
        .await;
    }
}

pub async fn connect_fake_gateway(port: u16) -> Arc<FakeGateway> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (rd, wr) = stream.into_split();
    let (tx, rx) = mpsc::channel(64);
    let gw = Arc::new(FakeGateway {
        generation: std::sync::atomic::AtomicU64::new(0),
        capture_epoch: hm_core::id::next("capture"),
        tx,
        rx: Mutex::new(rx),
        write: Mutex::new(Some(wr)),
    });
    // hello
    let hello = GatewayFrame::Hello {
        protocol: 0,
        supported_versions: vec![],
        min_protocol: 0,
        gateway: "fake-gateway 0.0.1".into(),
        gateway_version: "0.0.1".into(),
        devices: vec![HelloDevice {
            stable_id: "dot2-d".into(),
            model: "AEOBC".into(),
            firmware: None,
            capabilities: vec![
                "wake_event".into(),
                "text_in".into(),
                "audio_out".into(),
                "volume".into(),
                "stop".into(),
            ],
            audio_profile: "fireos_builtin_speaker".into(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
        auth_token: None,
    };
    let _line = serde_json::to_string(&hello).unwrap();
    gw.send(&hello).await;
    // reader → channel
    let tx2 = gw.tx.clone();
    let gw2 = gw.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(rd).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if let Ok(f) = serde_json::from_str::<CoreFrame>(&l) {
                if let CoreFrame::HelloAck { generation: _, .. } = &f {
                    // remember via channel too
                }
                let _ = tx2.send(f).await;
            }
        }
    });
    // hello_ack must be the first frame
    let first = gw.recv().await;
    match first {
        CoreFrame::HelloAck { generation, .. } => {
            gw.generation
                .store(generation, std::sync::atomic::Ordering::SeqCst);
        }
        f => panic!("expected hello_ack, got {f:?}"),
    }
    let _ = &gw2;
    gw
}

/// B-M0: tests that are not ABOUT waiting cues disable them explicitly, so
/// a cue WAV cached by another test can never inject an extra play into
/// their timeline (the cue directory is shared per crate run).
pub fn no_filler() -> hm_core::voice::FillerSettings {
    hm_core::voice::FillerSettings {
        thinking: vec![],
        search: vec![],
        device: vec![],
        story: vec![],
        ..Default::default()
    }
}

/// R1g: make the device report a link whose EFFECTIVE duplex is what the
/// caller needs. Production firmware reports `half` (the near-end local stop
/// is gated on `full`); the full-duplex rows of the matrix are reached
/// through the same `ModeReport` event a future device would send.
pub async fn report_duplex(gw: &FakeGateway, effective: &str) {
    gw.send(&GatewayFrame::ModeReport {
        device: "dot2-d".into(),
        duplex: hm_core::gateway::DuplexState {
            capability: if effective == "full" {
                "full".into()
            } else {
                "half".into()
            },
            requested: effective.into(),
            negotiated: effective.into(),
            effective: effective.into(),
        },
        aec: hm_core::gateway::AecState {
            owner: "core".into(),
            reference: "available".into(),
        },
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// App + fake gateway wired end to end. Returns (base_url, state, gateway).
pub async fn spawn_app_with_gateway(llm: FakeLlm) -> (String, Arc<AppState>, Arc<FakeGateway>) {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm)),
        Some(Arc::new(MockTts::default())),
        None,
        None,
    );
    // gateway listener on an ephemeral port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    tokio::time::sleep(Duration::from_millis(50)).await;

    let gw = connect_fake_gateway(gw_port).await;
    let _ = state.voice.set_mode(
        "dot2-d",
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
        Some(no_filler()),
        None,
        None,
        "test",
    );

    // management API on another ephemeral port
    enable_follow_window(&state);
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state, gw)
}

/// App + gateway advertising `play_stream` + a configured fake music service:
/// `music.play` takes the real streamed path through this socket (R1e).
pub async fn spawn_app_with_gateway_media(
    llm: FakeLlm,
) -> (
    String,
    Arc<AppState>,
    Arc<FakeGateway>,
    Arc<common::FakeMusic>,
) {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm)),
        Some(Arc::new(MockTts::default())),
        None,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    let fake = common::spawn_fake_music().await;
    common::configure_music(&state.store, &fake, true, false, true, true);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let gw = connect_gateway_audio_in_caps(gw_port, vec!["play_stream"]).await;
    let _ = state.voice.set_mode(
        "dot2-d",
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
        Some(no_filler()),
        None,
        None,
        "test",
    );

    enable_follow_window(&state);
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state, gw, Arc::new(fake))
}

impl FakeGateway {
    /// Send one raw JSON line (malformed-frame tests need shapes the
    /// typed enum refuses to express).
    pub async fn send_raw_json(&self, v: Value) {
        let line = v.to_string();
        let mut guard = self.write.lock().await;
        let w = guard.as_mut().expect("connection closed");
        w.write_all(line.as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
    }
}

pub async fn say(gw: &FakeGateway, seq: u64, text: &str) {
    gw.send(&GatewayFrame::Utterance {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq,
        source: "text".into(),
        text: text.into(),
    })
    .await;
}

// --- v0.2 uplink audio (G3) ------------------------------------------------------

pub struct FakeAsr {
    pub reply: String,
}

/// Fake ASR whose transcript can be changed between segments — the wake and
/// the command are two different utterances in one conversation.
pub struct SwitchableAsr {
    pub reply: std::sync::Arc<std::sync::Mutex<String>>,
}

impl SwitchableAsr {
    pub fn new(reply: &str) -> (Self, std::sync::Arc<std::sync::Mutex<String>>) {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(reply.to_string()));
        (
            Self {
                reply: shared.clone(),
            },
            shared,
        )
    }
}

#[async_trait::async_trait]
impl hm_core::speech::AsrClient for SwitchableAsr {
    async fn transcribe(&self, _wav: &[u8]) -> Result<String, hm_core::speech::SpeechError> {
        Ok(self.reply.lock().unwrap().clone())
    }
    fn provider(&self) -> &str {
        "switchable"
    }
}

#[async_trait::async_trait]
impl hm_core::speech::AsrClient for FakeAsr {
    async fn transcribe(&self, _wav: &[u8]) -> Result<String, hm_core::speech::SpeechError> {
        Ok(self.reply.clone())
    }
    fn provider(&self) -> &str {
        "fake"
    }
}

/// Counts every transcription request. The ASR-fallback tests are about
/// WHETHER a segment is sent at all, so the count is the assertion.
pub struct CountingAsr {
    pub reply: String,
    pub calls: std::sync::atomic::AtomicUsize,
}

impl CountingAsr {
    pub fn new(reply: &str) -> Arc<Self> {
        Arc::new(Self {
            reply: reply.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl hm_core::speech::AsrClient for CountingAsr {
    async fn transcribe(&self, _wav: &[u8]) -> Result<String, hm_core::speech::SpeechError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.reply.clone())
    }
    fn provider(&self) -> &str {
        "counting"
    }
}

/// A loud 80 ms tone block (VAD start) at 16 kHz mono.
pub fn loud_block() -> Vec<i16> {
    (0..1280)
        .map(|i| ((440.0 * i as f32 * 2.0 * std::f32::consts::PI / 16000.0).sin() * 12000.0) as i16)
        .collect()
}

pub fn quiet_block() -> Vec<i16> {
    vec![0; 1280]
}

/// A 440 Hz 80 ms block at the requested dBFS RMS (s16 full scale). A sine's
/// RMS is amplitude/√2, so the amplitude carries that factor — the VAD sees
/// the dB number asked for.
pub fn tone_block(db: f32) -> Vec<i16> {
    let amp = 32768.0 * 10f32.powf(db / 20.0) * std::f32::consts::SQRT_2;
    (0..1280)
        .map(|i| {
            ((440.0 * i as f32 * 2.0 * std::f32::consts::PI / 16000.0).sin() * amp)
                .clamp(-32768.0, 32767.0) as i16
        })
        .collect()
}

pub fn audio_chunk(seq: u64, samples: &[i16]) -> GatewayFrame {
    use base64::Engine;
    let bytes: Vec<u8> = samples.iter().flat_map(|&s| s.to_le_bytes()).collect();
    GatewayFrame::AudioChunk {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq,
        pcm_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        format: "s16le".into(),
        sample_rate: 16000,
        channels: 1,
        layout: "mono".into(),
        vad_speech: None,
        vad_prob: None,
    }
}

pub async fn connect_gateway_audio_in(port: u16) -> Arc<FakeGateway> {
    connect_gateway_audio_in_caps(port, vec![]).await
}

/// Caps-aware variant: `extra` joins the base audio_in device capability
/// set (v0.3 streaming tests add `play_stream`).
pub async fn connect_gateway_audio_in_caps(port: u16, extra: Vec<&str>) -> Arc<FakeGateway> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (rd, wr) = stream.into_split();
    let (tx, rx) = mpsc::channel(64);
    let gw = Arc::new(FakeGateway {
        generation: std::sync::atomic::AtomicU64::new(0),
        capture_epoch: hm_core::id::next("capture"),
        tx,
        rx: Mutex::new(rx),
        write: Mutex::new(Some(wr)),
    });
    let hello = GatewayFrame::Hello {
        protocol: 0,
        supported_versions: vec![],
        min_protocol: 0,
        gateway: "fake-gateway 0.0.1".into(),
        gateway_version: "0.0.1".into(),
        devices: vec![HelloDevice {
            stable_id: "dot2-d".into(),
            model: "AEOBC".into(),
            firmware: None,
            capabilities: {
                let mut c = vec![
                    "audio_in".to_string(),
                    "audio_out".to_string(),
                    "volume".to_string(),
                    "stop".to_string(),
                ];
                c.extend(extra.into_iter().map(str::to_string));
                c
            },
            audio_profile: "fireos_builtin_speaker".into(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
        auth_token: None,
    };
    gw.send(&hello).await;
    let tx2 = gw.tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(rd).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if let Ok(f) = serde_json::from_str::<CoreFrame>(&l) {
                let _ = tx2.send(f).await;
            }
        }
    });
    match gw.recv().await {
        CoreFrame::HelloAck { generation, .. } => gw
            .generation
            .store(generation, std::sync::atomic::Ordering::SeqCst),
        other => panic!("expected hello_ack, got {other:?}"),
    }
    gw
}

/// Like [`spawn_mic_app`] but the fake device ALSO advertises
/// `play_stream` (v0.3) — the streamed answer path is taken.
pub async fn spawn_mic_app_stream(
    llm: FakeLlm,
    asr_reply: &str,
) -> (String, Arc<AppState>, Arc<FakeGateway>) {
    spawn_mic_app_stream_tts(llm, asr_reply, Arc::new(MockTts::default())).await
}

/// Streamed path with the test's own ASR client.
pub async fn spawn_mic_app_stream_asr(
    llm: FakeLlm,
    asr: Arc<dyn hm_core::speech::AsrClient>,
) -> (String, Arc<AppState>, Arc<FakeGateway>) {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm)),
        Some(Arc::new(MockTts::default())),
        Some(asr),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    let _ = state.voice.set_mode(
        "dot2-d",
        None,
        None,
        None,
        None,
        None,
        Some(hm_core::voice::WakeAckSettings {
            enabled: false,
            ..Default::default()
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
        "test",
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let gw = connect_gateway_audio_in_caps(gw_port, vec!["play_stream"]).await;
    enable_follow_window(&state);
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state, gw)
}

/// Streaming app with an INJECTED TTS — the slow-segment tests need the
/// answer to still be streaming when the interruption lands (MockTts
/// finishes every sentence instantly, which cannot model that).
/// Same as `spawn_mic_app_stream_tts`, plus a REAL fake music service — the
/// no-match case needs the tool to answer `observed_ok{no_matches}`, not
/// `MUSIC_UNCONFIGURED`.
pub async fn spawn_mic_app_stream_tts_music(
    llm: FakeLlm,
    asr_reply: &str,
    tts: Arc<dyn hm_core::speech::TtsClient>,
) -> (
    String,
    Arc<AppState>,
    Arc<FakeGateway>,
    Arc<common::FakeMusic>,
) {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm)),
        Some(tts),
        Some(Arc::new(FakeAsr {
            reply: asr_reply.into(),
        })),
        None,
    );
    let fake = common::spawn_fake_music().await;
    common::configure_music(&state.store, &fake, true, false, true, true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    let _ = state.voice.set_mode(
        "dot2-d",
        None,
        None,
        None,
        None,
        None,
        Some(hm_core::voice::WakeAckSettings {
            enabled: false,
            ..Default::default()
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
        "test",
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let gw = connect_gateway_audio_in_caps(gw_port, vec!["play_stream"]).await;
    enable_follow_window(&state);
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state, gw, Arc::new(fake))
}

pub async fn spawn_mic_app_stream_tts(
    llm: FakeLlm,
    asr_reply: &str,
    tts: Arc<dyn hm_core::speech::TtsClient>,
) -> (String, Arc<AppState>, Arc<FakeGateway>) {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm)),
        Some(tts),
        Some(Arc::new(FakeAsr {
            reply: asr_reply.into(),
        })),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    let _ = state.voice.set_mode(
        "dot2-d",
        None,
        None,
        None,
        None,
        None,
        Some(hm_core::voice::WakeAckSettings {
            enabled: false,
            ..Default::default()
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
        "test",
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let gw = connect_gateway_audio_in_caps(gw_port, vec!["play_stream"]).await;
    enable_follow_window(&state);
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state, gw)
}

pub async fn spawn_mic_app(
    llm: FakeLlm,
    asr_reply: &str,
) -> (String, Arc<AppState>, Arc<FakeGateway>) {
    spawn_mic_app_asr(
        llm,
        Arc::new(FakeAsr {
            reply: asr_reply.into(),
        }),
    )
    .await
}

/// Same app, but with the test's own ASR client (which can record the audio
/// it was asked to transcribe — the only way to prove what reached ASR).
pub async fn spawn_mic_app_asr(
    llm: FakeLlm,
    asr: Arc<dyn hm_core::speech::AsrClient>,
) -> (String, Arc<AppState>, Arc<FakeGateway>) {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm)),
        Some(Arc::new(MockTts::default())),
        Some(asr),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    // Classic semantics for the state-frame tests: bare wake stays silent
    // (the local-ack behavior has its own dedicated tests below).
    let _ = state.voice.set_mode(
        "dot2-d",
        None,
        None,
        None,
        None,
        None,
        Some(hm_core::voice::WakeAckSettings {
            enabled: false,
            ..Default::default()
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
        "test",
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let gw = connect_gateway_audio_in(gw_port).await;
    enable_follow_window(&state);
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state, gw)
}

/// The first event of `kind` from THIS core epoch in the in-memory ring.
pub fn ring_event(state: &Arc<hm_core::api::AppState>, epoch: u64, kind: &str) -> Option<Value> {
    state
        .voice
        .recent(200)
        .into_iter()
        .find(|e| e["kind"] == kind && e["core_epoch"].as_u64() == Some(epoch))
}

// --- regression: stop during pending synthesis (user-reported race) ----------

/// A controllable TTS that parks specific texts on gates, so tests decide
/// exactly when each synthesis returns.
pub struct GatedTts {
    /// text-fragment -> gate (released when the bool flips to true).
    pub gates: Vec<(&'static str, tokio::sync::watch::Receiver<bool>)>,
    /// texts in call order (shared with the test for polling).
    pub calls: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl hm_core::speech::TtsClient for GatedTts {
    async fn synthesize(
        &self,
        text: &str,
    ) -> Result<hm_core::speech::TtsAudio, hm_core::speech::SpeechError> {
        self.calls.lock().unwrap().push(text.to_string());
        for (frag, rx) in &self.gates {
            if text.contains(frag) {
                let mut rx = rx.clone();
                while !*rx.borrow() {
                    rx.changed().await.expect("gate sender dropped");
                }
            }
        }
        // Valid WAV of a fixed short length once released.
        let pcm = vec![0u8; 3200]; // 100 ms at 16 kHz mono
        Ok(hm_core::speech::TtsAudio {
            duration_ms: 100,
            wav: hm_core::speech::pcm_to_wav(&pcm, 16000, 1),
            sample_rate: 16000,
            channels: 1,
            provider: "gated".into(),
            voice: "gated".into(),
        })
    }
    fn provider(&self) -> &str {
        "gated"
    }
}

/// A TTS that parks every synthesis on a gate and then FAILS, so the test
/// can cancel the turn before the error arrives (R1f).
pub struct FailingGatedTts {
    pub gate: tokio::sync::watch::Receiver<bool>,
    pub calls: Arc<std::sync::Mutex<usize>>,
}

#[async_trait::async_trait]
impl hm_core::speech::TtsClient for FailingGatedTts {
    async fn synthesize(
        &self,
        _text: &str,
    ) -> Result<hm_core::speech::TtsAudio, hm_core::speech::SpeechError> {
        *self.calls.lock().unwrap() += 1;
        let mut rx = self.gate.clone();
        while !*rx.borrow() {
            rx.changed().await.expect("gate sender dropped");
        }
        Err(hm_core::speech::SpeechError::Protocol(
            "synthesis cancelled".into(),
        ))
    }
    fn provider(&self) -> &str {
        "failing-gated"
    }
}

/// Collect ALL core frames (plays, stops, …) until the async predicate
/// holds, plus a short quiet drain; bounded by a hard deadline.
pub async fn collect_frames_until<F, Fut>(gw: &FakeGateway, mut done: F) -> Vec<CoreFrame>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut out = Vec::new();
    let mut quiet_after_done = 0u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout(Duration::from_millis(100), gw.recv()).await {
            Ok(f) => {
                quiet_after_done = 0;
                out.push(f);
            }
            Err(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        if done().await {
            quiet_after_done += 1;
            if quiet_after_done >= 5 {
                break;
            }
        }
    }
    out
}

// --- helpers for the regression tests -----------------------------------------

/// App + gateway + API with an injectable TTS (the gated-race tests need it;
/// everything else keeps `spawn_app_with_gateway`'s MockTts).
pub async fn spawn_app_with_tts(
    llm: FakeLlm,
    tts: Arc<dyn hm_core::speech::TtsClient>,
) -> (String, Arc<AppState>, Arc<FakeGateway>) {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm)),
        Some(tts),
        None,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let gw = connect_fake_gateway(gw_port).await;

    enable_follow_window(&state);
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state, gw)
}

/// Poll a sync condition until true (bounded).
pub async fn wait_until_sync(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timeout waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll an async condition until it yields Some (bounded).
pub async fn wait_until_some<T>(
    what: &str,
    mut cond: impl FnMut() -> futures::future::BoxFuture<'static, Option<T>>,
) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(v) = cond().await {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timeout waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Collect plays seen within `budget` or until `at_least` arrive; returns
/// (play_id, meta) pairs — the gateway's own view, nothing inferred.
pub async fn collect_plays(
    gw: &FakeGateway,
    budget: Duration,
    at_least: usize,
) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
        if wait.is_zero() {
            break;
        }
        match tokio::time::timeout(wait, gw.recv()).await {
            Ok(CoreFrame::Play {
                task_id,
                channel: Some(ch),
                text,
                simulated_ms,
                audio_b64,
                ..
            }) => {
                out.push((
                    task_id,
                    json!({ "channel": ch, "text": text, "simulated_ms": simulated_ms, "has_audio": audio_b64.is_some() }),
                ));
            }
            Ok(CoreFrame::PlayBegin {
                task_id,
                channel: Some(ch),
                text,
                ..
            }) => {
                out.push((
                    task_id,
                    json!({ "channel": ch, "text": text, "simulated_ms": Value::Null, "has_audio": true }),
                ));
            }
            Ok(_) => continue,
            Err(_) => {}
        }
        if out.len() >= at_least && at_least > 0 {
            break;
        }
    }
    out
}

/// "Collect to turn end" semantics (§0.3 item 3): gather EVERY play frame
/// from the wire until the async `done` predicate holds, then keep listening
/// through a ~500 ms quiet drain so a duplicate play arriving after the
/// first frame is still counted. A hard 10 s ceiling bounds test failures.
/// M5: drive a program the way a device does — confirm each play's terminal
/// (hardware-measured) so the NEXT sentence is allowed to start. Returns every
/// play in order. Without this a per-sentence-waiting program yields exactly
/// one play and the test mistakes it for the whole reading.
pub async fn play_program_through(gw: &FakeGateway, quiesce: Duration) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout(quiesce, gw.recv()).await {
            Ok(CoreFrame::Play {
                task_id,
                channel: Some(ch),
                text,
                simulated_ms,
                audio_b64,
                ..
            }) => {
                let meta = json!({ "channel": ch, "text": text, "simulated_ms": simulated_ms, "has_audio": audio_b64.is_some() });
                gw.send(&GatewayFrame::PlayFinished {
                    device: "dot2-d".into(),
                    task_id: task_id.clone(),
                    played_ms: 900,
                    consumed_ms: Some(850),
                    progress_basis: Some("hw_delay".into()),
                    ts: hm_core::id::now_ms(),
                })
                .await;
                out.push((task_id, meta));
            }
            Ok(CoreFrame::PlayBegin {
                task_id,
                channel: Some(ch),
                text,
                ..
            }) => {
                let meta = json!({ "channel": ch, "text": text, "simulated_ms": Value::Null, "has_audio": true });
                gw.send(&GatewayFrame::PlayFinished {
                    device: "dot2-d".into(),
                    task_id: task_id.clone(),
                    played_ms: 900,
                    consumed_ms: Some(850),
                    progress_basis: Some("hw_delay".into()),
                    ts: hm_core::id::now_ms(),
                })
                .await;
                out.push((task_id, meta));
            }
            Ok(_) => {}
            Err(_) => break,
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    out
}

pub async fn collect_plays_until<F, Fut>(gw: &FakeGateway, mut done: F) -> Vec<(String, Value)>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut out = Vec::new();
    let mut quiet_after_done = 0u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout(Duration::from_millis(100), gw.recv()).await {
            Ok(CoreFrame::Play {
                task_id,
                channel: Some(ch),
                text,
                simulated_ms,
                audio_b64,
                ..
            }) => {
                quiet_after_done = 0;
                out.push((
                    task_id,
                    json!({ "channel": ch, "text": text, "simulated_ms": simulated_ms, "has_audio": audio_b64.is_some() }),
                ));
            }
            Ok(CoreFrame::PlayBegin {
                task_id,
                channel: Some(ch),
                text,
                ..
            }) => {
                quiet_after_done = 0;
                out.push((
                    task_id,
                    json!({ "channel": ch, "text": text, "simulated_ms": Value::Null, "has_audio": true }),
                ));
            }
            Ok(_) => {
                quiet_after_done = 0;
            }
            Err(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        if done().await {
            quiet_after_done += 1;
            if quiet_after_done >= 5 {
                break;
            }
        }
    }
    out
}

// --- G4: assistant_state presentation (LED mapping input) + console debt ----------

/// Helper: a block at an approximate dBFS level (sine, RMS ≈ target).
pub fn block_at_db(db: f32) -> Vec<i16> {
    let amp = 32768.0 * 10f32.powf(db / 20.0);
    (0..1280)
        .map(|i| ((440.0 * i as f32 * 2.0 * std::f32::consts::PI / 16000.0).sin() * amp) as i16)
        .collect()
}

// --- D/T4: keyword interruption (device-side reflex → core decision) ---------------

/// Start a long voice play and return its play id once the device confirmed it.
pub async fn start_long_voice_play(
    state: &AppState,
    gw: &FakeGateway,
    seq: u64,
    text: &str,
) -> String {
    explicit_request(state, gw, seq, text).await;
    let (play_id, _) = gw.until_play("voice").await;
    gw.confirm_play(&play_id).await;
    play_id
}

/// Collect every frame that arrives within `window`, then report the Stops.
pub async fn stops_within(gw: &FakeGateway, window: Duration) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + window;
    let mut stopped = Vec::new();
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        match tokio::time::timeout_at(deadline, gw.recv()).await {
            Ok(CoreFrame::Stop { task_id, .. }) => stopped.push(task_id),
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    stopped
}

/// The music counterpart of `spawn_mic_app`: a fake music service AND a
/// deterministic ASR, so "music playing + a spoken command without the wake
/// word" can be driven end to end (R1g's half-duplex rule).
pub async fn spawn_media_mic_app(
    llm: FakeLlm,
    asr_reply: &str,
) -> (
    String,
    Arc<AppState>,
    Arc<FakeGateway>,
    Arc<common::FakeMusic>,
) {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm)),
        Some(Arc::new(MockTts::default())),
        Some(Arc::new(FakeAsr {
            reply: asr_reply.into(),
        })),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    let fake = common::spawn_fake_music().await;
    common::configure_music(&state.store, &fake, true, false, true, true);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let gw = connect_gateway_audio_in_caps(gw_port, vec!["play_stream"]).await;
    let _ = state.voice.set_mode(
        "dot2-d",
        None,
        None,
        None,
        None,
        None,
        Some(hm_core::voice::WakeAckSettings {
            enabled: false,
            ..Default::default()
        }),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(no_filler()),
        None,
        None,
        "test",
    );
    enable_follow_window(&state);
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state, gw, Arc::new(fake))
}

/// Feed one VAD segment: loud blocks open it, quiet blocks close it.
pub async fn feed_one_segment(gw: &FakeGateway, first_seq: u64) {
    let loud = loud_block();
    let quiet = quiet_block();
    for i in 0..10 {
        gw.send(&audio_chunk(first_seq + i, &loud)).await;
    }
    for i in 10..20 {
        gw.send(&audio_chunk(first_seq + i, &quiet)).await;
    }
}

// --- D/T5: semantic barge-in (near-end speech fact → core decision) ------------------

/// Build a barge_in frame: `loud_blocks` of loud audio as the pre-roll
/// (the device's onset ring), the guard's own numbers attached.
pub fn barge_in_frame(seq: u64, preroll_blocks: usize) -> GatewayFrame {
    // A person interrupting is LOUDER than what we are playing: the measured
    // real interruptions were +11.8/+20.2/+21.8/+23.1 dB over the reference
    // (-0.5 in this fixture was never a real number, and the core now refuses
    // a residual that is not positive).
    barge_in_frame_evidence(seq, preroll_blocks, -19.0, 12.0, true)
}

/// R1: the same fact with explicit device evidence. A certified fact has
/// `ref_locked=true` and `residual_db > -15` (the device's near-end
/// margin); anything else must be refused by the core.
pub fn barge_in_frame_evidence(
    seq: u64,
    preroll_blocks: usize,
    ref_db: f32,
    residual_db: f32,
    ref_locked: bool,
) -> GatewayFrame {
    barge_in_frame_energy(seq, preroll_blocks, -29.5, ref_db, residual_db, ref_locked)
}

/// R1f: the same fact with the microphone level spelled out, so the
/// absolute-floor rule can be exercised (`energy_db` is its own gate).
pub fn barge_in_frame_energy(
    seq: u64,
    preroll_blocks: usize,
    energy_db: f32,
    ref_db: f32,
    residual_db: f32,
    ref_locked: bool,
) -> GatewayFrame {
    use base64::Engine;
    let mut preroll: Vec<i16> = Vec::new();
    for _ in 0..preroll_blocks {
        preroll.extend(loud_block());
    }
    let bytes: Vec<u8> = preroll.iter().flat_map(|&s| s.to_le_bytes()).collect();
    GatewayFrame::BargeIn {
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq,
        pcm_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        sample_rate: 16000,
        channels: 1,
        corr: 0.21,
        energy_db,
        ref_db,
        residual_db,
        ref_locked,
    }
}

// --- D/T5 R5: a barge-in ends the WHOLE turn, not one sentence -------------

/// Streams one piece per sentence after a fixed synthesis delay, so the
/// answer is still being spoken when the interruption arrives.
pub struct SlowStreamTts {
    pub synth_ms: u64,
    pub piece_ms: u64,
}

#[async_trait::async_trait]
impl hm_core::speech::TtsClient for SlowStreamTts {
    async fn synthesize(
        &self,
        _text: &str,
    ) -> Result<hm_core::speech::TtsAudio, hm_core::speech::SpeechError> {
        let pcm = vec![0u8; (self.piece_ms * 32) as usize]; // 16 kHz mono s16le
        Ok(hm_core::speech::TtsAudio {
            wav: hm_core::speech::pcm_to_wav(&pcm, 16_000, 1),
            sample_rate: 16_000,
            channels: 1,
            duration_ms: self.piece_ms,
            provider: "slow-mock".into(),
            voice: "slow-mock".into(),
        })
    }

    async fn synthesize_streaming(
        &self,
        _text: &str,
        on_pcm: &tokio::sync::mpsc::UnboundedSender<hm_core::speech::TtsPiece>,
    ) -> Result<hm_core::speech::TtsAudio, hm_core::speech::SpeechError> {
        tokio::time::sleep(Duration::from_millis(self.synth_ms)).await;
        let pcm = vec![0u8; (self.piece_ms * 32) as usize];
        let _ = on_pcm.send(hm_core::speech::TtsPiece {
            sample_rate: 16_000,
            pcm: pcm.clone(),
        });
        Ok(hm_core::speech::TtsAudio {
            wav: hm_core::speech::pcm_to_wav(&pcm, 16_000, 1),
            sample_rate: 16_000,
            channels: 1,
            duration_ms: self.piece_ms,
            provider: "slow-mock".into(),
            voice: "slow-mock".into(),
        })
    }

    fn provider(&self) -> &str {
        "slow-mock"
    }
}

// --- B-M0 waiting cues ("垫话") ---------------------------------------------------

pub fn one_cue(delay_ms: u64, phrase: &str) -> hm_core::voice::FillerSettings {
    cue_with_mode(delay_ms, phrase, hm_core::voice::FillerMode::Radio)
}

/// R1g/P4: the same cue under an explicit mode.
pub fn cue_with_mode(
    delay_ms: u64,
    phrase: &str,
    mode: hm_core::voice::FillerMode,
) -> hm_core::voice::FillerSettings {
    hm_core::voice::FillerSettings {
        delay_ms,
        tool_delay_ms: 600,
        mode,
        thinking: vec![phrase.into()],
        search: vec![],
        device: vec![],
        story: vec![],
        resynth: None,
    }
}

pub async fn set_filler(state: &Arc<AppState>, filler: hm_core::voice::FillerSettings) {
    let _ = state.voice.set_mode(
        "dot2-d",
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
        Some(filler),
        None,
        None,
        "test",
    );
    state.voice.synthesize_wake_acks(false).await;
}

pub fn voice_log_has(state: &Arc<AppState>, kind: &str) -> Vec<Value> {
    state
        .voice
        .recent(400)
        .into_iter()
        .filter(|e| e["kind"] == kind)
        .collect()
}

// --- R1g/P4: the cue's mode (radio default / interrupt) -----------------------

/// A turn that is slow enough for the cue, then asks for music. With
/// `think_ms` longer than the cue's delay + length, the media play starts
/// while the cue is still talking — the case the user saw as
/// "「我查一下。」与音乐起播相差 153 ms".
pub fn slow_turn_then_music(think_ms: u64) -> FakeLlm {
    FakeLlm::new(vec![
        Scripted::DelayedThenCalls {
            delay_ms: think_ms,
            calls: vec![("music.search".into(), r#"{"query":"晚风"}"#.into())],
        },
        Scripted::Calls {
            calls: vec![(
                "music.play".into(),
                r#"{"source":"library","id":"t01"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["放上了。".into()],
        },
    ])
}

// --- R1e: pause reachability over the barge-in guard --------------------------------

/// Records the audio it is asked to transcribe, so a test can prove WHAT the
/// ASR heard (the segment alone, or the segment with the guarded head).
pub struct RecordingAsr {
    pub reply: String,
    pub lens: Arc<std::sync::Mutex<Vec<usize>>>,
}

#[async_trait::async_trait]
impl hm_core::speech::AsrClient for RecordingAsr {
    async fn transcribe(&self, wav: &[u8]) -> Result<String, hm_core::speech::SpeechError> {
        self.lens.lock().unwrap().push(wav.len());
        Ok(self.reply.clone())
    }
    fn provider(&self) -> &str {
        "fake"
    }
}

// --- R1h: the truncation label (RH-1) ----------------------------------------------

/// Set the truncation notice through the same mode API the console uses.
pub async fn set_truncation_notice(state: &Arc<AppState>, phrase: Option<&str>) {
    let _ = state
        .voice
        .set_mode(
            "dot2-d",
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
            phrase.map(str::to_string),
            "test",
        )
        .unwrap();
}

// --- R1j/P1: the reflex vocabulary (per-word policy, preconditions, dedupe) ---

pub async fn send_keyword(gw: &FakeGateway, seq: u64, word: &str, self_heard: bool) {
    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq,
        keyword: word.into(),
        fed_ms: Some(1000),
        self_heard,
        rms_dbfs: None,
        lag_ms: None,
        bundle_id: None,
        kws_generation: None,
        kws_tier: None,
        wake_dur_ms: None,
        max_gap_ms: None,
        l2_passed: None,
    })
    .await;
}

pub fn reflex_word_hits(state: &Arc<AppState>, word: &str) -> (u64, u64) {
    let mode = state.voice.mode_snapshot();
    let words = mode["devices"][0]["reflex"]["words"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let w = words
        .iter()
        .find(|w| w["text"] == word)
        .cloned()
        .unwrap_or_default();
    (
        w["hits"].as_u64().unwrap_or(0),
        w["ignored"].as_u64().unwrap_or(0),
    )
}

/// TTS that records every sentence it is asked to say. A streamed play
/// carries text only in its `PlayBegin` (later sentences ride as PCM chunks),
/// so this is the only place a test can see WHICH sentences were spoken.
#[derive(Default)]
pub struct RecordingTts {
    pub said: std::sync::Mutex<Vec<String>>,
}

impl RecordingTts {
    pub fn said(&self) -> Vec<String> {
        self.said.lock().unwrap().clone()
    }
}

/// Recording TTS that also takes its time, so a resume can be interrupted
/// deterministically while a sentence is still being synthesized.
#[derive(Default)]
pub struct SlowTts {
    pub delay_ms: u64,
    pub said: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl hm_core::speech::TtsClient for SlowTts {
    async fn synthesize(
        &self,
        text: &str,
    ) -> Result<hm_core::speech::TtsAudio, hm_core::speech::SpeechError> {
        self.said.lock().unwrap().push(text.to_string());
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        let pcm = vec![0u8; 3200];
        Ok(hm_core::speech::TtsAudio {
            wav: hm_core::speech::pcm_to_wav(&pcm, 16_000, 1),
            sample_rate: 16_000,
            channels: 1,
            duration_ms: 100,
            provider: "slow".into(),
            voice: "slow".into(),
        })
    }

    fn provider(&self) -> &str {
        "slow"
    }
}

#[async_trait::async_trait]
impl hm_core::speech::TtsClient for RecordingTts {
    async fn synthesize(
        &self,
        text: &str,
    ) -> Result<hm_core::speech::TtsAudio, hm_core::speech::SpeechError> {
        self.said.lock().unwrap().push(text.to_string());
        let chars = text.chars().count().max(1) as u64;
        let ms = (chars * 70).min(3000);
        let samples = ms * 16_000 / 1000;
        let pcm = vec![0u8; (samples * 2) as usize];
        Ok(hm_core::speech::TtsAudio {
            wav: hm_core::speech::pcm_to_wav(&pcm, 16_000, 1),
            sample_rate: 16_000,
            channels: 1,
            duration_ms: ms,
            provider: "recording".into(),
            voice: "recording".into(),
        })
    }

    fn provider(&self) -> &str {
        "recording"
    }
}

/// Every sentence ONE streamed answer delivered, in order: the `PlayBegin`
/// text plus every following sentence the TTS was asked to say.
pub async fn streamed_answer(gw: &FakeGateway, tts: &RecordingTts) -> String {
    let mut out = String::new();
    let mut stream: Option<String> = None;
    loop {
        let f = gw
            .until(|f| matches!(f, CoreFrame::PlayBegin { .. } | CoreFrame::PlayEnd { .. }))
            .await;
        match f {
            CoreFrame::PlayBegin {
                task_id,
                channel,
                text,
                ..
            } => {
                if channel.as_deref() != Some("voice") {
                    continue;
                }
                if stream.is_none() {
                    stream = Some(task_id.clone());
                }
                if stream.as_deref() == Some(task_id.as_str()) {
                    out.push_str(&text.unwrap_or_default());
                }
            }
            CoreFrame::PlayEnd { task_id } => {
                if stream.as_deref() == Some(task_id.as_str()) {
                    break;
                }
            }
            _ => {}
        }
    }
    // Later sentences of the same stream are only visible at the TTS.
    for s in tts.said() {
        if !out.contains(&s) {
            out.push_str(&s);
        }
    }
    out
}

/// The runaway backstop set LOW on purpose. Since 2026-11-14 the MODEL picks
/// the delivery shape and the runtime only catches a runaway, so at the
/// default (2000 chars) no fixture is ever cut; a test that is ABOUT the cut
/// has to ask for it.
pub fn low_backstop(state: &Arc<AppState>, sentences: usize) {
    let policy = hm_core::voice::SpokenDeliverySettings {
        max_sentences: 1,
        max_chars: 100,
        detail_max_sentences: 1,
        detail_max_chars: 100,
        backstop_max_sentences: sentences,
        backstop_max_chars: 200,
        ..Default::default()
    };
    state
        .voice
        .set_spoken_delivery("dot2-d", policy, "test")
        .expect("a low backstop is still a valid policy");
}

pub fn e_hint(log: &[Value]) -> bool {
    log.iter().any(|e| e["kind"] == "spoken_continue_hint")
}

// --- reminders (2026-11-14) ------------------------------------------------

pub struct BrokenTts;

#[async_trait::async_trait]
impl hm_core::speech::TtsClient for BrokenTts {
    async fn synthesize(
        &self,
        _text: &str,
    ) -> Result<hm_core::speech::TtsAudio, hm_core::speech::SpeechError> {
        // The 2026-11-14 outage, replayed: synthesis is refused.
        Err(hm_core::speech::SpeechError::Http {
            status: 403,
            detail: "requested resource not granted".into(),
        })
    }
    fn provider(&self) -> &str {
        "broken"
    }
}

// D0 helpers are explicit: old say/audio_chunk helpers retain their original
// wire behavior and seq values. Capturing and sending are separate operations.
#[derive(Clone)]
pub struct CapturedInput {
    pub epoch: String,
    pub ticket: String,
    pub input_id: String,
}

impl FakeGateway {
    /// Start a source capture context. A coordinate reset issues a new
    /// source epoch here, never when an asynchronous event is sent.
    pub fn capture_input(&self) -> CapturedInput {
        CapturedInput {
            epoch: format!("{}:{}", self.capture_epoch, hm_core::id::next("epoch")),
            ticket: hm_core::id::next("ticket"),
            input_id: hm_core::id::next("input"),
        }
    }

    pub async fn send_captured(&self, mut frame: Value, ts: u64, seq: u64) {
        frame["ts"] = json!(ts);
        frame["seq"] = json!(seq);
        self.send_raw_json(frame).await;
    }
}

impl CapturedInput {
    pub fn origin(&self, start: u64, end: u64) -> Value {
        json!({ "kind":"audio", "capture_epoch":self.epoch, "ticket":self.ticket,
            "input_id":self.input_id, "start_sample":start, "end_sample":end })
    }

    pub fn keyword(&self, start: u64, end: u64) -> Value {
        json!({ "type":"keyword", "device":"dot2-d", "keyword":"未配置测试词", "ts":1, "seq":1,
            "origin": self.origin(start, end) })
    }

    pub fn pcm(&self, seq: u64, start: u64, samples: &[i16]) -> Value {
        let mut frame = serde_json::to_value(audio_chunk(seq, samples)).unwrap();
        frame["origin"] = self.origin(start, start + samples.len() as u64);
        frame
    }
}

pub type AsrRelease = tokio::sync::oneshot::Sender<Result<String, hm_core::speech::SpeechError>>;
pub struct D0GatedAsr {
    pub started: mpsc::UnboundedSender<AsrRelease>,
}
#[async_trait::async_trait]
impl hm_core::speech::AsrClient for D0GatedAsr {
    async fn transcribe(&self, _wav: &[u8]) -> Result<String, hm_core::speech::SpeechError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.started.send(tx).unwrap();
        rx.await.unwrap()
    }
    fn provider(&self) -> &str {
        "d0-gated"
    }
}

pub async fn d0_wait_note(state: &AppState, kind: &str, pred: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(note) = state
                .voice
                .recent(100)
                .into_iter()
                .find(|v| v["kind"] == kind && pred(v))
            {
                return note;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing {kind}: {:?}", state.voice.recent(100)))
}

pub async fn d0_send_segment(gw: &FakeGateway, input: &CapturedInput, first_seq: u64) {
    // Exactly 18 frames: close at the end, with no spare preroll for the next input.
    for block in 0..18 {
        let samples = if block < 10 {
            loud_block()
        } else {
            quiet_block()
        };
        gw.send_raw_json(input.pcm(first_seq + block, block * 1280, &samples))
            .await;
    }
}

// D1 helpers deliberately name the new scenarios. The original say/seq
// behavior and all 157 pre-D0 tests above remain unchanged.
pub async fn d1_finish_voice(gw: &FakeGateway, play: &str) {
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: play.into(),
        played_ms: 100,
        consumed_ms: Some(100),
        progress_basis: Some("hw_delay".into()),
        ts: hm_core::id::now_ms(),
    })
    .await;
}

pub async fn d1_wait_idle(state: &AppState) {
    for _ in 0..100 {
        if state.voice.mode_snapshot()["devices"][0]["participation"] == "idle" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("request did not settle: {}", state.voice.mode_snapshot());
}

pub struct D1PartialFailTts {
    pub release_failure: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl hm_core::speech::TtsClient for D1PartialFailTts {
    async fn synthesize(
        &self,
        _text: &str,
    ) -> Result<hm_core::speech::TtsAudio, hm_core::speech::SpeechError> {
        Err(hm_core::speech::SpeechError::Http {
            status: 503,
            detail: "D1 isolated failure".into(),
        })
    }

    async fn synthesize_streaming(
        &self,
        _text: &str,
        on_pcm: &mpsc::UnboundedSender<hm_core::speech::TtsPiece>,
    ) -> Result<hm_core::speech::TtsAudio, hm_core::speech::SpeechError> {
        on_pcm
            .send(hm_core::speech::TtsPiece {
                sample_rate: 16_000,
                pcm: vec![0; 3_200],
            })
            .unwrap();
        self.release_failure.notified().await;
        Err(hm_core::speech::SpeechError::Http {
            status: 503,
            detail: "D1 failure after partial PCM".into(),
        })
    }

    fn provider(&self) -> &str {
        "d1-partial-fail"
    }
}

/// D2 setup: the pre-D2 text helper self-opened implicitly. The request
/// scenarios now explicitly activate before sending the SAME seq/text.
/// Control exits are submitted without activation, on the current lease.
pub async fn explicit_request(state: &AppState, gw: &FakeGateway, seq: u64, text: &str) {
    if !hm_core::voice::is_retreat(text) {
        state.voice.test_activate_session("dot2-d");
    }
    let origin = json!({"kind":"leased", "lease":state.voice.capture_lease("dot2-d"),
        "origin":{"kind":"text", "input_id":hm_core::id::next("test-text")}});
    gw.send_raw_json(
        json!({"type":"utterance","device":"dot2-d","ts":hm_core::id::now_ms(),
        "seq":seq,"source":"text","text":text,"origin":origin}),
    )
    .await;
}
