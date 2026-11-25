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

#[test]
fn select_media_decimation_keeps_lan_and_loopback_native_and_halves_compact_links() {
    use super::decode::{select_media_decimation, PcmDecimator};

    // Loopback speakers and low-RTT home LAN Wi-Fi (`compact_link == false`) keep native Hi-Fi.
    assert_eq!(select_media_decimation(44_100, false), (1, 44_100));
    assert_eq!(select_media_decimation(48_000, false), (1, 48_000));
    assert_eq!(select_media_decimation(96_000, false), (1, 96_000));

    // Constrained WAN / VPN / cellular links (`compact_link == true`) step >24kHz down cleanly.
    assert_eq!(select_media_decimation(44_100, true), (2, 22_050));
    assert_eq!(select_media_decimation(48_000, true), (2, 24_000));
    assert_eq!(select_media_decimation(96_000, true), (4, 24_000));
    assert_eq!(select_media_decimation(88_200, true), (4, 22_050));
    assert_eq!(select_media_decimation(24_000, true), (1, 24_000));
    assert_eq!(select_media_decimation(16_000, true), (1, 16_000));

    // Verify stateful carry across odd block boundaries produces identical output
    // to single-slice decimation without dropping samples or clicking.
    let mut dec = PcmDecimator::new(2);
    let mut out = Vec::new();
    dec.push_into(&[100, 200, 300], &mut out); // odd length: [150], carry=[300]
    assert_eq!(out, vec![150]);
    dec.push_into(&[500, 1000, 2000, 4000], &mut out); // pairs (300,500)->400, (1000,2000)->1500, carry=[4000]
    assert_eq!(out, vec![150, 400, 1500]);
    dec.flush_into(&mut out); // trailing carry [4000] -> 4000
    assert_eq!(out, vec![150, 400, 1500, 4000]);

    // 4:1 decimation with split carries across 3 blocks
    let mut dec4 = PcmDecimator::new(4);
    let mut out4 = Vec::new();
    dec4.push_into(&[100, 200], &mut out4);
    assert!(out4.is_empty());
    dec4.push_into(&[300], &mut out4);
    assert!(out4.is_empty());
    dec4.push_into(&[600, 40, 80, 120, 160], &mut out4);
    assert_eq!(out4, vec![300, 100]);
    dec4.flush_into(&mut out4);
    assert_eq!(out4, vec![300, 100]);
}

#[tokio::test]
async fn lan_and_loopback_keep_native_44100hz_while_tailscale_and_high_rtt_wan_decimate_to_22050hz()
{
    use axum::{routing::get, Router};
    use base64::Engine;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    // Build a 1-second 44,100 Hz mono PCM WAV fixture.
    let wav_44k = {
        let rate: u32 = 44_100;
        let samples = rate as usize;
        let data_len = (samples * 2) as u32;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..samples {
            let v: i16 = if (i / 2) % 2 == 0 { 1000 } else { -1000 };
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    };
    let wav_shared = Arc::new(wav_44k);
    let app = Router::new().route(
        "/stream.wav",
        get({
            let w = wav_shared.clone();
            move || {
                let bytes = (*w).clone();
                async move { ([("content-type", "audio/wav")], bytes) }
            }
        }),
    );
    let http_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_l.local_addr().unwrap();
    let _http = tokio::spawn(async move {
        let _ = axum::serve(http_l, app).await;
    });

    let hub = crate::gateway::DeviceHub::new();
    hub.set_voice_gateway_open(true);
    let rt = MusicRuntime::new(
        Arc::new(crate::store::Store::open_in_memory().unwrap()),
        hub.clone(),
    );

    async fn connect_mock(
        hub: Arc<crate::gateway::DeviceHub>,
        peer: std::net::SocketAddr,
        dev_id: &'static str,
    ) -> tokio::task::JoinHandle<(u32, usize)> {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let h = hub.clone();
        tokio::spawn(async move {
            let (sock, _) = l.accept().await.unwrap();
            let _ = h.serve_connection_from(sock, Some(peer)).await;
        });
        let sock = TcpStream::connect(addr).await.unwrap();
        let (rd, mut wr) = sock.into_split();
        let hello = json!({
            "type": "hello",
            "protocol": 2,
            "min_protocol": 0,
            "gateway": format!("mock-{dev_id}"),
            "gateway_version": "2.3.13",
            "devices": [{
                "stable_id": dev_id,
                "model": "mock",
                "capabilities": ["audio_out", "stop", "play_stream"],
                "audio_profile": "mock"
            }]
        });
        wr.write_all(format!("{hello}\n").as_bytes()).await.unwrap();
        let mut lines = BufReader::new(rd).lines();
        // Drain hello_ack + time_set
        for _ in 0..2 {
            let _ = lines.next_line().await.unwrap().unwrap();
        }
        tokio::spawn(async move {
            let mut began_rate = 0u32;
            let mut total_pcm_bytes = 0usize;
            let mut sent_streaming = false;
            while let Ok(Some(line)) =
                tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                    .await
                    .unwrap_or(Ok(None))
            {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                match v["type"].as_str() {
                    Some("play_begin") => {
                        began_rate = v["audio"]["sample_rate"].as_u64().unwrap() as u32;
                        let tid = v["task_id"].as_str().unwrap();
                        let ack =
                            json!({"type":"play_receipt","task_id":tid,"stage":"accepted","ts":1});
                        let _ = wr.write_all(format!("{ack}\n").as_bytes()).await;
                    }
                    Some("play_chunk") => {
                        let b64 = v["audio_b64"].as_str().unwrap();
                        let raw = base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .unwrap();
                        total_pcm_bytes += raw.len();
                        if !sent_streaming {
                            sent_streaming = true;
                            let tid = v["task_id"].as_str().unwrap();
                            let st = json!({"type":"play_receipt","task_id":tid,"stage":"streaming","ts":2});
                            let _ = wr.write_all(format!("{st}\n").as_bytes()).await;
                        }
                    }
                    Some("play_end") => break,
                    _ => {}
                }
            }
            (began_rate, total_pcm_bytes)
        })
    }

    let local_rx = connect_mock(
        hub.clone(),
        "127.0.0.1:41001".parse().unwrap(),
        "lx06-local",
    )
    .await;
    let lan_rx = connect_mock(
        hub.clone(),
        "192.168.100.20:41002".parse().unwrap(),
        "android-lan",
    )
    .await;
    let tailscale_rx = connect_mock(
        hub.clone(),
        "100.64.0.42:41003".parse().unwrap(),
        "android-tailscale",
    )
    .await;
    let subnet_wan_rx = connect_mock(
        hub.clone(),
        "192.168.100.1:41004".parse().unwrap(),
        "android-subnet-wan",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Simulate a healthy 4 ms LAN Wi-Fi link vs an 85 ms cellular link SNATed
    // through a home subnet router.
    hub.set_device_link_rtt_us_for_test("android-lan", 4_000);
    hub.set_device_link_rtt_us_for_test("android-subnet-wan", 85_000);

    let track = Track {
        source: Source::Library,
        id: "t44k".into(),
        title: "HiFi44k".into(),
        artist: "Test".into(),
        binding: String::new(),
    };

    let client = reqwest::Client::new();
    for (idx, dev_id) in [
        "lx06-local",
        "android-lan",
        "android-tailscale",
        "android-subnet-wan",
    ]
    .into_iter()
    .enumerate()
    {
        let resp = client
            .get(format!("http://{http_addr}/stream.wav"))
            .send()
            .await
            .unwrap();
        let ctx = crate::tool::ToolCtx::new(
            format!("T{idx}"),
            format!("C{idx}"),
            Arc::new(AtomicBool::new(false)),
        );
        rt.start(dev_id.into(), vec![track.clone()], 0, 0, resp, &ctx)
            .await
            .unwrap();
    }

    let (local_rate, local_bytes) = local_rx.await.unwrap();
    let (lan_rate, lan_bytes) = lan_rx.await.unwrap();
    let (ts_rate, ts_bytes) = tailscale_rx.await.unwrap();
    let (wan_rate, wan_bytes) = subnet_wan_rx.await.unwrap();

    assert_eq!(
        local_rate, 44_100,
        "loopback speaker must keep native 44.1 kHz"
    );
    assert_eq!(local_bytes, 44_100 * 2);

    assert_eq!(
        lan_rate, 44_100,
        "home LAN Wi-Fi gateway must keep untouched native 44.1 kHz Hi-Fi"
    );
    assert_eq!(lan_bytes, 44_100 * 2);

    assert_eq!(
        ts_rate, 22_050,
        "direct Tailscale CGNAT gateway must decimate 44.1 kHz -> 22.05 kHz"
    );
    assert_eq!(ts_bytes, 22_050 * 2);

    assert_eq!(
        wan_rate, 22_050,
        "high-RTT subnet-routed WAN gateway must decimate 44.1 kHz -> 22.05 kHz"
    );
    assert_eq!(wan_bytes, 22_050 * 2);

    // Verify per-connection media queue congestion latch (`>= 96` chunks) flips
    // ONLY the stalled LAN connection to compact mode while leaving a healthy
    // sibling LAN connection untouched.
    let stalled_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stalled_addr = stalled_listener.local_addr().unwrap();
    let hub_stalled = hub.clone();
    tokio::spawn(async move {
        let (sock, _) = stalled_listener.accept().await.unwrap();
        let _ = hub_stalled
            .serve_connection_from(sock, Some("192.168.100.30:41005".parse().unwrap()))
            .await;
    });
    let stalled_sock = TcpStream::connect(stalled_addr).await.unwrap();
    let (stalled_rd, mut stalled_wr) = stalled_sock.into_split();
    let stalled_hello = json!({
        "type": "hello",
        "protocol": 2,
        "min_protocol": 0,
        "gateway": "mock-stalled-lan",
        "gateway_version": "2.3.13",
        "devices": [{
            "stable_id": "android-lan-stalled",
            "model": "mock",
            "capabilities": ["audio_out", "stop", "play_stream"],
            "audio_profile": "mock"
        }]
    });
    stalled_wr
        .write_all(format!("{stalled_hello}\n").as_bytes())
        .await
        .unwrap();
    let mut stalled_lines = BufReader::new(stalled_rd).lines();
    for _ in 0..2 {
        let _ = stalled_lines.next_line().await.unwrap().unwrap();
    }
    hub.set_device_link_rtt_us_for_test("android-lan-stalled", 5_000);
    assert!(
        !hub.device_prefers_compact_media("android-lan-stalled"),
        "fresh low-RTT LAN peer must start in native Hi-Fi mode"
    );
    assert!(
        !hub.device_prefers_compact_media("android-lan"),
        "healthy sibling LAN peer must stay in native Hi-Fi mode"
    );

    // Enqueue > 96 media chunks faster than the socket drains by NOT reading
    // from `stalled_lines` and shrinking SO_RCVBUF / saturating `media_tx` via
    // a large burst of chunks on `android-lan-stalled`.
    let stalled_ticket = hub
        .play_begin_ref("android-lan-stalled", "media", 44_100, None, None)
        .unwrap();
    let big_pcm = vec![0u8; 3_528]; // 40ms @ 44.1kHz s16le = 3528B raw (~4.8KB JSON)
    for seq in 0..220u64 {
        let _ = hub.play_chunk(&stalled_ticket.play_id, seq, &big_pcm);
    }
    assert!(
        hub.device_prefers_compact_media("android-lan-stalled"),
        "sustained >=96-chunk media queue backlog must latch compact media on stalled peer"
    );
    assert!(
        !hub.device_prefers_compact_media("android-lan"),
        "stalled peer congestion must NEVER degrade a healthy sibling LAN peer"
    );
}

#[tokio::test]
async fn remote_music_start_ramps_smoothly_and_yields_to_concurrent_voice_tts() {
    use axum::{routing::get, Router};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    // 1. Verify smooth play-ahead ramp math: >= 480 ms initial prebuffer at t=0,
    //    never blasting 3.0 s upfront, smoothly reaching 3.0 s by ~5.3 s.
    assert!(
        remote_play_ahead_budget_secs(0.0) >= 0.55 && remote_play_ahead_budget_secs(0.0) <= 0.80,
        "initial remote burst must fill Android 480ms prebuffer without blasting 3s"
    );
    assert!(remote_play_ahead_budget_secs(1.0) < 1.3);
    assert_eq!(remote_play_ahead_budget_secs(10.0), REMOTE_PLAY_AHEAD_SECS);

    // 2. Build a 3.5-second 48 kHz mono WAV fixture (compact link -> 24 kHz = 87 chunks).
    let wav_48k = {
        let rate: u32 = 48_000;
        let samples = (rate as usize) * 7 / 2;
        let data_len = (samples * 2) as u32;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(44 + data_len as usize, 0u8);
        out
    };
    let wav_shared = Arc::new(wav_48k);
    let app = Router::new().route(
        "/track.wav",
        get({
            let w = wav_shared.clone();
            move || {
                let bytes = (*w).clone();
                async move { ([("content-type", "audio/wav")], bytes) }
            }
        }),
    );
    let http_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_l.local_addr().unwrap();
    let _http = tokio::spawn(async move {
        let _ = axum::serve(http_l, app).await;
    });

    let hub = crate::gateway::DeviceHub::new();
    hub.set_voice_gateway_open(true);
    let rt = MusicRuntime::new(
        Arc::new(crate::store::Store::open_in_memory().unwrap()),
        hub.clone(),
    );

    let gw_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = gw_l.local_addr().unwrap();
    let hub_srv = hub.clone();
    tokio::spawn(async move {
        let (sock, _) = gw_l.accept().await.unwrap();
        let _ = hub_srv
            .serve_connection_from(sock, Some("100.64.0.77:41020".parse().unwrap()))
            .await;
    });

    let sock = TcpStream::connect(gw_addr).await.unwrap();
    sock.set_nodelay(true).unwrap();
    let (rd, mut wr) = sock.into_split();
    let hello = json!({
        "type": "hello",
        "protocol": 2,
        "min_protocol": 0,
        "gateway": "android-remote-297",
        "gateway_version": "2.3.14",
        "devices": [{
            "stable_id": "android-297",
            "model": "android-phone",
            "capabilities": ["audio_out", "stop", "play_stream"],
            "audio_profile": "android_farfield_v1"
        }]
    });
    wr.write_all(format!("{hello}\n").as_bytes()).await.unwrap();
    let mut lines = BufReader::new(rd).lines();
    for _ in 0..2 {
        let _ = lines.next_line().await.unwrap().unwrap();
    }

    let wire_frames = Arc::new(std::sync::Mutex::new(Vec::<(
        std::time::Instant,
        serde_json::Value,
    )>::new()));
    let wf_rx = wire_frames.clone();
    let start_instant = std::time::Instant::now();
    let reader = tokio::spawn(async move {
        let mut sent_streaming = false;
        while let Ok(Some(line)) = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
            .await
            .unwrap_or(Ok(None))
        {
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            let now = std::time::Instant::now();
            match v["type"].as_str() {
                Some("play_begin") => {
                    let tid = v["task_id"].as_str().unwrap();
                    let ack =
                        json!({"type":"play_receipt","task_id":tid,"stage":"accepted","ts":1});
                    let _ = wr.write_all(format!("{ack}\n").as_bytes()).await;
                }
                Some("play_chunk") if !sent_streaming => {
                    sent_streaming = true;
                    let tid = v["task_id"].as_str().unwrap();
                    let st =
                        json!({"type":"play_receipt","task_id":tid,"stage":"streaming","ts":2});
                    let _ = wr.write_all(format!("{st}\n").as_bytes()).await;
                }
                _ => {}
            }
            wf_rx.lock().unwrap().push((now, v));
        }
    });

    let track = Track {
        source: Source::Online,
        id: "t297".into(),
        title: "TestSong".into(),
        artist: "Tester".into(),
        binding: String::new(),
    };
    let resp = reqwest::Client::new()
        .get(format!("http://{http_addr}/track.wav"))
        .send()
        .await
        .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let ctx = crate::tool::ToolCtx::new("T297", "C297", cancel.clone());
    let (media_play_id, _, _) = rt
        .start("android-297".into(), vec![track], 0, 0, resp, &ctx)
        .await
        .unwrap();

    // Wait 120 ms after music_play_start: smooth ramp must NOT have dumped 75 chunks (3.0 s).
    tokio::time::sleep(Duration::from_millis(120)).await;
    let initial_media_chunks = wire_frames
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, f)| f["type"] == "play_chunk" && f["task_id"] == media_play_id)
        .count();
    assert!(
        (12..=32).contains(&initial_media_chunks),
        "remote startup burst must cover Android 480ms prebuffer (>=12 chunks) without blasting 75 chunks (got {initial_media_chunks} in {:?})",
        start_instant.elapsed()
    );

    // Emit concurrent 5-chunk streaming Voice TTS ("正在播放...") staggered 30 ms apart.
    let voice = hub
        .play_begin(
            "android-297",
            "voice",
            16_000,
            Some("正在播放测试歌曲".into()),
        )
        .unwrap();
    let voice_pcm = vec![0x33u8; 1_280];
    for seq in 0..5u64 {
        hub.play_chunk(&voice.play_id, seq, &voice_pcm).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    hub.play_end(&voice.play_id).unwrap();
    tokio::time::sleep(Duration::from_millis(180)).await;

    // Stop music and inspect wire order.
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = hub.stop(&media_play_id);
    reader.abort();

    let recorded = wire_frames.lock().unwrap();
    let v_begin = recorded
        .iter()
        .position(|(_, f)| f["type"] == "play_begin" && f["task_id"] == voice.play_id)
        .expect("voice play_begin must reach wire");
    let v_end = recorded
        .iter()
        .position(|(_, f)| f["type"] == "play_end" && f["task_id"] == voice.play_id)
        .expect("voice play_end must reach wire");
    let interleaved_media = recorded[v_begin..=v_end]
        .iter()
        .filter(|(_, f)| f["task_id"] == media_play_id)
        .count();
    assert_eq!(
        interleaved_media, 0,
        "zero media chunks may interleave on TCP between streaming voice play_begin and play_end"
    );
    let media_after_voice = recorded[v_end + 1..]
        .iter()
        .filter(|(_, f)| f["type"] == "play_chunk" && f["task_id"] == media_play_id)
        .count();
    assert!(
        media_after_voice > 0,
        "media streaming must resume immediately after voice play_end"
    );
}

#[tokio::test]
async fn remote_pump_bounds_media_queue_under_stall_and_latches_congestion_per_peer() {
    use axum::{routing::get, Router};
    use std::os::fd::AsRawFd;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    // Build an 8-second 48 kHz mono WAV fixture (~1 MB raw -> ~200 chunks at 48 kHz)
    // so a stalled native-LAN socket fills `SO_SNDBUF` and hits `REMOTE_MEDIA_QUEUE_HIGH_WATER`.
    let wav_48k = {
        let rate: u32 = 48_000;
        let samples = (rate as usize) * 8;
        let data_len = (samples * 2) as u32;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(44 + data_len as usize, 0u8);
        out
    };
    let wav_shared = Arc::new(wav_48k);
    let app = Router::new().route(
        "/stall.wav",
        get({
            let w = wav_shared.clone();
            move || {
                let bytes = (*w).clone();
                async move { ([("content-type", "audio/wav")], bytes) }
            }
        }),
    );
    let http_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_l.local_addr().unwrap();
    let _http = tokio::spawn(async move {
        let _ = axum::serve(http_l, app).await;
    });

    let hub = crate::gateway::DeviceHub::new();
    hub.set_voice_gateway_open(true);
    let rt = MusicRuntime::new(
        Arc::new(crate::store::Store::open_in_memory().unwrap()),
        hub.clone(),
    );

    // Connect a healthy sibling LAN peer alongside the soon-to-stall LAN peer.
    let healthy_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let healthy_addr = healthy_l.local_addr().unwrap();
    let hub_h = hub.clone();
    tokio::spawn(async move {
        let (sock, _) = healthy_l.accept().await.unwrap();
        let _ = hub_h
            .serve_connection_from(sock, Some("192.168.100.51:41030".parse().unwrap()))
            .await;
    });
    let healthy_sock = TcpStream::connect(healthy_addr).await.unwrap();
    let (healthy_rd, mut healthy_wr) = healthy_sock.into_split();
    let healthy_hello = json!({
        "type": "hello",
        "protocol": 2,
        "min_protocol": 0,
        "gateway": "android-healthy-sibling",
        "gateway_version": "2.3.14",
        "devices": [{
            "stable_id": "android-healthy-297",
            "model": "android-phone",
            "capabilities": ["audio_out", "stop", "play_stream"],
            "audio_profile": "android_farfield_v1"
        }]
    });
    healthy_wr
        .write_all(format!("{healthy_hello}\n").as_bytes())
        .await
        .unwrap();
    let mut healthy_lines = BufReader::new(healthy_rd).lines();
    for _ in 0..2 {
        let _ = healthy_lines.next_line().await.unwrap().unwrap();
    }
    hub.set_device_link_rtt_us_for_test("android-healthy-297", 4_000);

    // Connect the stalled LAN peer with tiny kernel socket buffers so TCP write
    // backpressures immediately once the peer stops reading.
    let stall_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stall_addr = stall_l.local_addr().unwrap();
    let hub_s = hub.clone();
    tokio::spawn(async move {
        let (sock, _) = stall_l.accept().await.unwrap();
        let small: libc::c_int = 4096;
        unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&small as *const libc::c_int).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        let _ = hub_s
            .serve_connection_from(sock, Some("192.168.100.52:41031".parse().unwrap()))
            .await;
    });
    let stall_sock = TcpStream::connect(stall_addr).await.unwrap();
    {
        let small: libc::c_int = 4096;
        unsafe {
            libc::setsockopt(
                stall_sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&small as *const libc::c_int).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    let (stall_rd, mut stall_wr) = stall_sock.into_split();
    let stall_hello = json!({
        "type": "hello",
        "protocol": 2,
        "min_protocol": 0,
        "gateway": "android-stalled-pump",
        "gateway_version": "2.3.14",
        "devices": [{
            "stable_id": "android-stalled-297",
            "model": "android-phone",
            "capabilities": ["audio_out", "stop", "play_stream"],
            "audio_profile": "android_farfield_v1"
        }]
    });
    stall_wr
        .write_all(format!("{stall_hello}\n").as_bytes())
        .await
        .unwrap();
    let mut stall_lines = BufReader::new(stall_rd).lines();
    for _ in 0..2 {
        let _ = stall_lines.next_line().await.unwrap().unwrap();
    }
    hub.set_device_link_rtt_us_for_test("android-stalled-297", 5_000);
    assert!(!hub.device_prefers_compact_media("android-stalled-297"));
    assert!(!hub.device_prefers_compact_media("android-healthy-297"));

    // Read only `play_begin` + first `play_chunk` to acknowledge `streaming`,
    // then intentionally freeze reading so `MusicRuntime::pump` hits real TCP backpressure.
    let ack_and_freeze = tokio::spawn(async move {
        let mut saw_begin = false;
        while let Ok(Some(line)) =
            tokio::time::timeout(Duration::from_secs(3), stall_lines.next_line())
                .await
                .unwrap_or(Ok(None))
        {
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            match v["type"].as_str() {
                Some("play_begin") => {
                    saw_begin = true;
                    let tid = v["task_id"].as_str().unwrap();
                    let ack =
                        json!({"type":"play_receipt","task_id":tid,"stage":"accepted","ts":1});
                    let _ = stall_wr.write_all(format!("{ack}\n").as_bytes()).await;
                }
                Some("play_chunk") if saw_begin => {
                    let tid = v["task_id"].as_str().unwrap();
                    let st =
                        json!({"type":"play_receipt","task_id":tid,"stage":"streaming","ts":2});
                    let _ = stall_wr.write_all(format!("{st}\n").as_bytes()).await;
                    break;
                }
                _ => {}
            }
        }
        // Keep the socket open without draining so kernel buffers stay full.
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop((stall_lines, stall_wr));
    });

    let track = Track {
        source: Source::Library,
        id: "t-stall-297".into(),
        title: "StallTrack".into(),
        artist: "Tester".into(),
        binding: String::new(),
    };
    let resp = reqwest::Client::new()
        .get(format!("http://{http_addr}/stall.wav"))
        .send()
        .await
        .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let ctx = crate::tool::ToolCtx::new("T297S", "C297S", cancel.clone());
    let (play_id, _, _) = rt
        .start("android-stalled-297".into(), vec![track], 0, 0, resp, &ctx)
        .await
        .unwrap();

    // Sample `device_media_queue_depth` across ~4.5 s while stalled so the ramp
    // reaches >32 chunks, backpressure holds `media_tx` bounded near 32 (never 75..229),
    // and >= 1.5 s of continuous backpressure latches `media_congested` ONLY on the stalled peer.
    let mut max_depth = 0usize;
    let mut latched = false;
    let poll_deadline = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < poll_deadline {
        let depth = hub.device_media_queue_depth("android-stalled-297");
        max_depth = max_depth.max(depth);
        assert!(
            depth <= REMOTE_MEDIA_QUEUE_HIGH_WATER + 2,
            "pump queue-depth backpressure must bound media_tx near {REMOTE_MEDIA_QUEUE_HIGH_WATER} (observed {depth})"
        );
        if hub.device_prefers_compact_media("android-stalled-297") {
            latched = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = hub.stop(&play_id);
    ack_and_freeze.abort();

    assert!(
        max_depth >= REMOTE_MEDIA_QUEUE_HIGH_WATER,
        "stalled socket must actually exercise REMOTE_MEDIA_QUEUE_HIGH_WATER ({REMOTE_MEDIA_QUEUE_HIGH_WATER}), observed peak {max_depth}"
    );
    assert!(
        latched,
        "sustained >= {REMOTE_MEDIA_CONGESTION_WAIT_MS} ms pump backpressure must latch compact mode on stalled peer"
    );
    assert!(
        !hub.device_prefers_compact_media("android-healthy-297"),
        "healthy sibling LAN peer must keep native Hi-Fi untouched"
    );
}
