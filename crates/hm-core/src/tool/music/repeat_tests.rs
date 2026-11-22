//! Serial 91-C regression tests: music repeat mode (`off | one | all`),
//! `music.control` `action="repeat"`, natural-end replay/wrap, and radio rejection.

use super::session::next_repeat_index;
use super::*;
use crate::agent::facts::render_fact;
use crate::config::{ConfigService, MusicConfig, MusicConnection};
use crate::gateway::{DeviceEvent, DeviceHub, GatewayFrame, HelloDevice};
use crate::store::Store;
use crate::tool::{Tool, ToolCtx, ToolStatus};
use axum::{routing::get, Router};
use std::sync::atomic::AtomicBool;
use tokio::io::AsyncWriteExt;

fn tiny_wav_bytes() -> Vec<u8> {
    let sample_rate: u32 = 16_000;
    let num_samples: u32 = 1_600; // 100 ms mono PCM16
    let data_bytes: u32 = num_samples * 2;
    let mut buf = Vec::with_capacity(44 + data_bytes as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_bytes.to_le_bytes());
    for i in 0..num_samples {
        let s: i16 = if i % 2 == 0 { 800 } else { -800 };
        buf.extend_from_slice(&s.to_le_bytes());
    }
    buf
}

async fn connect_mock_gateway(hub: &Arc<DeviceHub>, dev: &str) -> tokio::task::JoinHandle<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut ctrl = hub.subscribe_control();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv_hub = hub.clone();
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let _ = srv_hub.serve_connection(stream).await;
        }
    });
    let client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (read_half, mut write_half) = client.into_split();
    let hello = GatewayFrame::Hello {
        protocol: 2,
        min_protocol: 0,
        supported_versions: vec![0, 1, 2],
        gateway: "gw-repeat".into(),
        gateway_version: "0.1.0".into(),
        devices: vec![HelloDevice {
            stable_id: dev.into(),
            model: "AEOBC".into(),
            firmware: Some("hm-firmware 0.0.53".into()),
            capabilities: vec!["audio_out".into(), "audio_in".into(), "play_stream".into()],
            audio_profile: String::new(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
    };
    let mut line = serde_json::to_string(&hello).unwrap();
    line.push('\n');
    write_half.write_all(line.as_bytes()).await.unwrap();
    write_half.flush().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(DeviceEvent::DeviceJoined { device, .. }) = ctrl.recv().await {
                if device.stable_id == dev {
                    break;
                }
            }
        }
    })
    .await
    .expect("DeviceJoined");
    let dev_owned = dev.to_string();
    tokio::spawn(async move {
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        while let Ok(n) = reader.read_line(&mut buf).await {
            if n == 0 {
                break;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&buf) {
                if v["type"] == "play_begin" {
                    if let Some(play_id) = v["task_id"].as_str().or_else(|| v["play_id"].as_str()) {
                        let ack = json!({
                            "type": "play_receipt",
                            "device": dev_owned,
                            "task_id": play_id,
                            "stage": "streaming",
                            "detail": "mock",
                            "ts": crate::id::now_ms()
                        });
                        let mut out = serde_json::to_string(&ack).unwrap();
                        out.push('\n');
                        let _ = write_half.write_all(out.as_bytes()).await;
                        let _ = write_half.flush().await;
                    }
                }
            }
            buf.clear();
        }
    })
}

async fn spawn_wav_library_runtime(
    dev: &str,
) -> (
    Arc<MusicRuntime>,
    tokio::task::JoinHandle<()>,
    Arc<Mutex<Vec<String>>>,
    String,
) {
    let fetched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetched_cb = fetched.clone();
    let wav = Arc::new(tiny_wav_bytes());
    let app = Router::new().route(
        "/rest/stream.view",
        get(
            move |axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>| {
                let fetched_cb = fetched_cb.clone();
                let wav = wav.clone();
                async move {
                    if let Some(id) = params.get("id") {
                        fetched_cb.lock().unwrap().push(id.clone());
                    }
                    ([("content-type", "audio/wav")], (*wav).clone())
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let svc = ConfigService::new(store.clone());
    svc.save_music(
        MusicConfig {
            enabled: true,
            prefer_local: true,
            library: Some(MusicConnection {
                endpoint: base.clone(),
                user: "u".into(),
            }),
            online: None,
        },
        Some("lib-secret"),
        None,
        "test",
    )
    .unwrap();
    let hub = DeviceHub::new();
    let gw = connect_mock_gateway(&hub, dev).await;
    let rt = MusicRuntime::with_client(store, hub, reqwest::Client::new());
    let binding = format!("{}|u", base.trim_end_matches('/'));
    (rt, gw, fetched, binding)
}

fn two_tracks(binding: &str) -> Vec<Track> {
    vec![
        Track {
            source: Source::Library,
            id: "t1".into(),
            title: "晴天".into(),
            artist: "周杰伦".into(),
            binding: binding.into(),
        },
        Track {
            source: Source::Library,
            id: "t2".into(),
            title: "稻香".into(),
            artist: "周杰伦".into(),
            binding: binding.into(),
        },
    ]
}

#[test]
fn next_repeat_index_handles_off_one_all_and_rejects_radio_and_story() {
    let now = crate::id::now_ms();
    let mut sess = MediaSession {
        tracks: two_tracks("lib"),
        index: 0,
        state: MediaState::Playing,
        position_ms: 1_000,
        play_id: Some("p1".into()),
        started_ms: now,
        base_ms: 0,
        updated_ms: now,
        repeat: RepeatMode::Off,
    };
    assert_eq!(next_repeat_index(&sess), None);

    sess.repeat = RepeatMode::One;
    assert_eq!(next_repeat_index(&sess), Some(0));

    sess.repeat = RepeatMode::All;
    assert_eq!(next_repeat_index(&sess), Some(1));
    sess.index = 1;
    assert_eq!(
        next_repeat_index(&sess),
        Some(0),
        "list repeat wraps to index 0"
    );

    // Radio and Story never repeat even if repeat field were mutated.
    sess.tracks[1].source = Source::Radio;
    assert_eq!(next_repeat_index(&sess), None);
    sess.tracks[1].source = Source::Story;
    assert_eq!(next_repeat_index(&sess), None);
}

#[tokio::test]
async fn music_control_repeat_sets_mode_for_music_and_rejects_radio_and_story() {
    let dev = "dot2-d";
    let (rt, _gw, _fetched, binding) = spawn_wav_library_runtime(dev).await;
    let tool = MusicControlTool::new(rt.clone());
    let ctx = ToolCtx::new("t-rep", "c-rep", Arc::new(AtomicBool::new(false)));
    let now = crate::id::now_ms();

    // 1) Music session: "单曲循环" / one -> succeeds and renders confirmed fact.
    rt.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks: two_tracks(&binding),
            index: 0,
            state: MediaState::Playing,
            position_ms: 2_000,
            play_id: Some("p-music".into()),
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::Off,
        },
    );
    let res = tool
        .execute(json!({ "action": "repeat", "mode": "one" }), &ctx)
        .await;
    assert_eq!(res.status, ToolStatus::Completed);
    assert_eq!(res.result.as_ref().unwrap()["repeat"], json!("one"));
    assert_eq!(rt.media_session(dev).unwrap().repeat, RepeatMode::One);
    let fact = res
        .fact
        .as_ref()
        .expect("repeat must emit confirmed ToolFact");
    assert_eq!(render_fact(&res, fact), "《晴天》已设为单曲循环。");

    // Switch to list repeat ("all") and back to "off".
    let res_all = tool
        .execute(json!({ "action": "repeat", "mode": "all" }), &ctx)
        .await;
    assert_eq!(res_all.status, ToolStatus::Completed);
    assert_eq!(rt.media_session(dev).unwrap().repeat, RepeatMode::All);
    assert_eq!(
        render_fact(&res_all, res_all.fact.as_ref().unwrap()),
        "《晴天》已设为列表循环。"
    );

    let res_off = tool
        .execute(json!({ "action": "repeat", "mode": "off" }), &ctx)
        .await;
    assert_eq!(res_off.status, ToolStatus::Completed);
    assert_eq!(rt.media_session(dev).unwrap().repeat, RepeatMode::Off);

    // 2) Radio session: explicitly tells user "电台不能循环".
    rt.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks: vec![Track {
                source: Source::Radio,
                id: "yes933".into(),
                title: "Yes933".into(),
                artist: String::new(),
                binding: "radio|local".into(),
            }],
            index: 0,
            state: MediaState::Playing,
            position_ms: 0,
            play_id: Some("p-radio".into()),
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::Off,
        },
    );
    let res_radio = tool
        .execute(json!({ "action": "repeat", "mode": "one" }), &ctx)
        .await;
    assert_eq!(res_radio.status, ToolStatus::Failed);
    let err = res_radio.error.as_ref().unwrap();
    assert_eq!(err.code, "MUSIC_REPEAT_UNSUPPORTED");
    assert_eq!(err.detail, "电台不能循环");
    assert_eq!(rt.media_session(dev).unwrap().repeat, RepeatMode::Off);

    // 3) Story session: also rejected (music only).
    rt.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks: vec![Track {
                source: Source::Story,
                id: "s1".into(),
                title: "小红帽".into(),
                artist: String::new(),
                binding: "story|local".into(),
            }],
            index: 0,
            state: MediaState::Playing,
            position_ms: 0,
            play_id: Some("p-story".into()),
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::Off,
        },
    );
    let res_story = tool
        .execute(json!({ "action": "repeat", "mode": "one" }), &ctx)
        .await;
    assert_eq!(res_story.status, ToolStatus::Failed);
    assert_eq!(
        res_story.error.as_ref().unwrap().code,
        "MUSIC_REPEAT_UNSUPPORTED"
    );
}

#[tokio::test]
async fn natural_play_end_replays_one_wraps_all_and_ignores_stop_or_pause() {
    let dev = "dot2-d";
    let (rt, _gw, fetched, binding) = spawn_wav_library_runtime(dev).await;
    let notes: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = notes.clone();
    rt.set_event_sink(Arc::new(move |kind: &str, payload: Value| {
        sink.lock().unwrap().push((kind.to_string(), payload));
    }));
    let tracks = two_tracks(&binding);
    let now = crate::id::now_ms();

    // 1) RepeatMode::One -> natural end replays track 0 (t1) from start_ms=0.
    rt.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks: tracks.clone(),
            index: 0,
            state: MediaState::Playing,
            position_ms: 5_000,
            play_id: Some("p-one".into()),
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::One,
        },
    );
    let cancel = AtomicBool::new(false);
    rt.on_natural_play_end(dev, "p-one", &cancel).await;
    assert_eq!(fetched.lock().unwrap().as_slice(), &["t1".to_string()]);
    let sess = rt.media_session(dev).unwrap();
    assert_eq!(sess.index, 0);
    assert_eq!(sess.repeat, RepeatMode::One);
    assert_eq!(sess.state, MediaState::Playing);
    assert_ne!(sess.play_id.as_deref(), Some("p-one"));
    assert!(notes
        .lock()
        .unwrap()
        .iter()
        .any(|(k, v)| k == "media_repeat_advance" && v["repeat"] == "one" && v["index"] == 0));

    // 2) RepeatMode::All on last track (index 1) -> wraps to track 0 (t1).
    fetched.lock().unwrap().clear();
    rt.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks: tracks.clone(),
            index: 1,
            state: MediaState::Playing,
            position_ms: 5_000,
            play_id: Some("p-all-end".into()),
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::All,
        },
    );
    rt.on_natural_play_end(dev, "p-all-end", &cancel).await;
    assert_eq!(fetched.lock().unwrap().as_slice(), &["t1".to_string()]);
    let sess = rt.media_session(dev).unwrap();
    assert_eq!(sess.index, 0);
    assert_eq!(sess.repeat, RepeatMode::All);

    // 3) User stop (cancel=true or session discarded) MUST NOT trigger repeat.
    fetched.lock().unwrap().clear();
    rt.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks: tracks.clone(),
            index: 0,
            state: MediaState::Playing,
            position_ms: 2_000,
            play_id: Some("p-stop".into()),
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::One,
        },
    );
    let cancelled = AtomicBool::new(true);
    rt.on_natural_play_end(dev, "p-stop", &cancelled).await;
    assert!(
        fetched.lock().unwrap().is_empty(),
        "cancelled stop must not trigger repeat"
    );
    assert!(
        rt.media_session(dev).is_none(),
        "cancelled stop must retire session"
    );

    // 4) User pause (`state=Paused`, `play_id=None`) MUST NOT trigger repeat or erase session.
    rt.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks,
            index: 0,
            state: MediaState::Paused,
            position_ms: 2_000,
            play_id: None,
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::One,
        },
    );
    rt.on_natural_play_end(dev, "p-paused", &cancel).await;
    assert!(
        fetched.lock().unwrap().is_empty(),
        "paused session must not trigger repeat"
    );
    let sess = rt
        .media_session(dev)
        .expect("paused session must remain intact");
    assert_eq!(sess.state, MediaState::Paused);
    assert_eq!(sess.repeat, RepeatMode::One);
}

async fn connect_mock_gateway_with_play_finished(
    hub: &Arc<DeviceHub>,
    dev: &str,
    play_begins: Arc<std::sync::atomic::AtomicUsize>,
    emit_play_finished: bool,
    stop_first: bool,
) -> tokio::task::JoinHandle<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut ctrl = hub.subscribe_control();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv_hub = hub.clone();
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let _ = srv_hub.serve_connection(stream).await;
        }
    });
    let client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (read_half, mut write_half) = client.into_split();
    let hello = GatewayFrame::Hello {
        protocol: 2,
        min_protocol: 0,
        supported_versions: vec![0, 1, 2],
        gateway: "gw-repeat-chain".into(),
        gateway_version: "0.1.0".into(),
        devices: vec![HelloDevice {
            stable_id: dev.into(),
            model: "AEOBC".into(),
            firmware: Some("hm-firmware 0.0.53".into()),
            capabilities: vec!["audio_out".into(), "audio_in".into(), "play_stream".into()],
            audio_profile: String::new(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
    };
    let mut line = serde_json::to_string(&hello).unwrap();
    line.push('\n');
    write_half.write_all(line.as_bytes()).await.unwrap();
    write_half.flush().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(DeviceEvent::DeviceJoined { device, .. }) = ctrl.recv().await {
                if device.stable_id == dev {
                    break;
                }
            }
        }
    })
    .await
    .expect("DeviceJoined");
    let dev_owned = dev.to_string();
    tokio::spawn(async move {
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        while let Ok(n) = reader.read_line(&mut buf).await {
            if n == 0 {
                break;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&buf) {
                match v["type"].as_str() {
                    Some("play_begin") => {
                        play_begins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if let Some(play_id) = v["task_id"].as_str() {
                            let ack = json!({
                                "type": "play_receipt",
                                "device": dev_owned,
                                "task_id": play_id,
                                "stage": "streaming",
                                "detail": "mock",
                                "ts": crate::id::now_ms()
                            });
                            let mut out = serde_json::to_string(&ack).unwrap();
                            out.push('\n');
                            let _ = write_half.write_all(out.as_bytes()).await;
                            let _ = write_half.flush().await;
                        }
                    }
                    Some("play_end") if emit_play_finished => {
                        if let Some(play_id) = v["task_id"].as_str() {
                            // A device that is stopped by a button reports the stop
                            // receipt first and *then* play_finished. Replay that
                            // order so the supervisor can tell "the person stopped
                            // it" from "the track ran out".
                            if stop_first {
                                let receipt = json!({
                                    "type": "stop_receipt",
                                    "device": dev_owned,
                                    "task_id": play_id,
                                    "stage": "device_confirmed",
                                    "played_ms": 100,
                                    "ts": crate::id::now_ms()
                                });
                                let mut out = serde_json::to_string(&receipt).unwrap();
                                out.push('\n');
                                let _ = write_half.write_all(out.as_bytes()).await;
                                let _ = write_half.flush().await;
                            }
                            let fin = json!({
                                "type": "play_finished",
                                "device": dev_owned,
                                "task_id": play_id,
                                "played_ms": 100,
                                "ts": crate::id::now_ms()
                            });
                            let mut out = serde_json::to_string(&fin).unwrap();
                            out.push('\n');
                            let _ = write_half.write_all(out.as_bytes()).await;
                            let _ = write_half.flush().await;
                        }
                    }
                    _ => {}
                }
            }
            buf.clear();
        }
    })
}

/// Serial 92 P1-1 end-to-end chain test: drives real `pump` + mock gateway
/// `play_finished` frame -> `DeviceHub` removes active play & emits
/// `DeviceEvent::PlayFinished` -> `supervise_playback` deterministically
/// advances repeat for `One` / `All` and retires on `Off`.
#[tokio::test]
async fn play_finished_supervisor_chain_advances_one_all_and_stops_on_off() {
    let dev = "dot2-d";
    let fetched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetched_cb = fetched.clone();
    let wav = Arc::new(tiny_wav_bytes());
    let app = Router::new().route(
        "/rest/stream.view",
        get(
            move |axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>| {
                let fetched_cb = fetched_cb.clone();
                let wav = wav.clone();
                async move {
                    if let Some(id) = params.get("id") {
                        fetched_cb.lock().unwrap().push(id.clone());
                    }
                    ([("content-type", "audio/wav")], (*wav).clone())
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    ConfigService::new(store.clone())
        .save_music(
            MusicConfig {
                enabled: true,
                prefer_local: true,
                library: Some(MusicConnection {
                    endpoint: base.clone(),
                    user: "u".into(),
                }),
                online: None,
            },
            Some("lib-secret"),
            None,
            "test",
        )
        .unwrap();
    let hub = DeviceHub::new();
    let play_begins = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let _gw =
        connect_mock_gateway_with_play_finished(&hub, dev, play_begins.clone(), true, false).await;
    let rt = MusicRuntime::with_client(store, hub, reqwest::Client::new());
    let notes: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = notes.clone();
    rt.set_event_sink(Arc::new(move |kind: &str, payload: Value| {
        sink.lock().unwrap().push((kind.to_string(), payload));
    }));
    let binding = format!("{}|u", base.trim_end_matches('/'));
    let tracks = two_tracks(&binding);

    // 1) RepeatMode::One -> pump finishes -> gateway emits PlayFinished ->
    // supervisor advances and replays track 0 (index 0) automatically.
    let resp = reqwest::get(format!("{base}/rest/stream.view?id=t1"))
        .await
        .unwrap();
    let ctx = ToolCtx::new("voice:dot2-d", "c-one", Arc::new(AtomicBool::new(false)));
    let (first_play_id, _, _) = rt
        .start_with_repeat(
            dev.into(),
            tracks.clone(),
            0,
            0,
            resp,
            &ctx,
            RepeatMode::One,
        )
        .await
        .expect("start_with_repeat One");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let advanced = notes.lock().unwrap().iter().any(|(k, v)| {
                k == "media_repeat_advance"
                    && v["prev_play_id"] == first_play_id
                    && v["repeat"] == "one"
                    && v["index"] == 0
            });
            if advanced {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("supervisor PlayFinished chain must emit media_repeat_advance for RepeatMode::One");
    let _ = rt.stop_now(dev, "test_cleanup");

    // 2) RepeatMode::All on last track (index 1) -> wraps to index 0 (t1).
    notes.lock().unwrap().clear();
    let resp = reqwest::get(format!("{base}/rest/stream.view?id=t2"))
        .await
        .unwrap();
    let ctx = ToolCtx::new("voice:dot2-d", "c-all", Arc::new(AtomicBool::new(false)));
    let (all_play_id, _, _) = rt
        .start_with_repeat(
            dev.into(),
            tracks.clone(),
            1,
            0,
            resp,
            &ctx,
            RepeatMode::All,
        )
        .await
        .expect("start_with_repeat All");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let advanced = notes.lock().unwrap().iter().any(|(k, v)| {
                k == "media_repeat_advance"
                    && v["prev_play_id"] == all_play_id
                    && v["repeat"] == "all"
                    && v["index"] == 0
            });
            if advanced {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("supervisor PlayFinished chain must wrap RepeatMode::All to index 0");
    let _ = rt.stop_now(dev, "test_cleanup");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 3) RepeatMode::Off -> PlayFinished ends session with no repeat advance.
    notes.lock().unwrap().clear();
    let begins_before_off = play_begins.load(std::sync::atomic::Ordering::SeqCst);
    let resp = reqwest::get(format!("{base}/rest/stream.view?id=t1"))
        .await
        .unwrap();
    let ctx = ToolCtx::new("voice:dot2-d", "c-off", Arc::new(AtomicBool::new(false)));
    let (off_play_id, _, _) = rt
        .start_with_repeat(dev.into(), tracks, 0, 0, resp, &ctx, RepeatMode::Off)
        .await
        .expect("start_with_repeat Off");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let ended = notes
                .lock()
                .unwrap()
                .iter()
                .any(|(k, v)| k == "media_session_ended" && v["play_id"] == off_play_id);
            if ended {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("RepeatMode::Off must retire session on PlayFinished");
    assert!(
        !notes
            .lock()
            .unwrap()
            .iter()
            .any(|(k, v)| k == "media_repeat_advance" && v["prev_play_id"] == off_play_id),
        "RepeatMode::Off must not emit media_repeat_advance"
    );
    assert_eq!(
        play_begins.load(std::sync::atomic::Ordering::SeqCst),
        begins_before_off + 1,
        "RepeatMode::Off must not start any follow-up play"
    );
    assert!(rt.media_session(dev).is_none());
}

/// Serial 92 P1-1b: a device stopped by a button reports `stop_receipt` and
/// then `play_finished`. That finish is NOT a natural end — repeat must not
/// start the next track (found by audit on 2a1bb88: `finished_naturally`
/// overrode `stopped_by_device`).
#[tokio::test]
async fn device_stop_receipt_then_play_finished_does_not_advance_repeat() {
    let dev = "dot2-d";
    let fetched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetched_cb = fetched.clone();
    let wav = Arc::new(tiny_wav_bytes());
    let app = Router::new().route(
        "/rest/stream.view",
        get(
            move |axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>| {
                let fetched_cb = fetched_cb.clone();
                let wav = wav.clone();
                async move {
                    if let Some(id) = params.get("id") {
                        fetched_cb.lock().unwrap().push(id.clone());
                    }
                    ([("content-type", "audio/wav")], (*wav).clone())
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    ConfigService::new(store.clone())
        .save_music(
            MusicConfig {
                enabled: true,
                prefer_local: true,
                library: Some(MusicConnection {
                    endpoint: base.clone(),
                    user: "u".into(),
                }),
                online: None,
            },
            Some("lib-secret"),
            None,
            "test",
        )
        .unwrap();
    let hub = DeviceHub::new();
    let play_begins = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let _gw =
        connect_mock_gateway_with_play_finished(&hub, dev, play_begins.clone(), true, true).await;
    let rt = MusicRuntime::with_client(store, hub, reqwest::Client::new());
    let notes: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = notes.clone();
    rt.set_event_sink(Arc::new(move |kind: &str, payload: Value| {
        sink.lock().unwrap().push((kind.to_string(), payload));
    }));
    let binding = format!("{}|u", base.trim_end_matches('/'));
    let tracks = two_tracks(&binding);

    let resp = reqwest::get(format!("{base}/rest/stream.view?id=t1"))
        .await
        .unwrap();
    let ctx = ToolCtx::new("voice:dot2-d", "c-stop", Arc::new(AtomicBool::new(false)));
    let (play_id, _, _) = rt
        .start_with_repeat(dev.into(), tracks, 0, 0, resp, &ctx, RepeatMode::One)
        .await
        .expect("start_with_repeat One");

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if rt.media_session(dev).is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a stopped play must retire its session");

    assert!(
        !notes
            .lock()
            .unwrap()
            .iter()
            .any(|(k, v)| k == "media_repeat_advance" && v["prev_play_id"] == play_id),
        "a device stop receipt is not a natural end: repeat must not advance"
    );
    assert_eq!(
        play_begins.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no follow-up play may start after a device stop"
    );
}

/// Serial 92 P1-2 concurrency test: a user `pause_now` or `stop_now` arriving
/// while `on_natural_play_end` is mid-flight (opening/decoding the next stream)
/// must never be overwritten back to `Playing` and must not start a new play.
#[tokio::test]
async fn pause_or_stop_during_repeat_advance_is_never_overwritten_and_starts_no_new_play() {
    let dev = "dot2-d";
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let entered_cb = entered.clone();
    let release_cb = release.clone();
    let wav = Arc::new(tiny_wav_bytes());
    let app = Router::new().route(
        "/rest/stream.view",
        get(move || {
            let entered_cb = entered_cb.clone();
            let release_cb = release_cb.clone();
            let wav = wav.clone();
            async move {
                entered_cb.notify_one();
                release_cb.notified().await;
                ([("content-type", "audio/wav")], (*wav).clone())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    ConfigService::new(store.clone())
        .save_music(
            MusicConfig {
                enabled: true,
                prefer_local: true,
                library: Some(MusicConnection {
                    endpoint: base.clone(),
                    user: "u".into(),
                }),
                online: None,
            },
            Some("lib-secret"),
            None,
            "test",
        )
        .unwrap();
    let hub = DeviceHub::new();
    let play_begins = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let _gw =
        connect_mock_gateway_with_play_finished(&hub, dev, play_begins.clone(), false, false).await;
    let rt = MusicRuntime::with_client(store, hub, reqwest::Client::new());
    let notes: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = notes.clone();
    rt.set_event_sink(Arc::new(move |kind: &str, payload: Value| {
        sink.lock().unwrap().push((kind.to_string(), payload));
    }));
    let binding = format!("{}|u", base.trim_end_matches('/'));
    let tracks = two_tracks(&binding);
    let now = crate::id::now_ms();

    // Sub-case A: user pauses while repeat advance is opening the next track.
    rt.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks: tracks.clone(),
            index: 0,
            state: MediaState::Playing,
            position_ms: 4_000,
            play_id: Some("p-race-pause".into()),
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::One,
        },
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let rt_task = rt.clone();
    let cancel_task = cancel.clone();
    let handle = tokio::spawn(async move {
        rt_task
            .on_natural_play_end(dev, "p-race-pause", &cancel_task)
            .await;
    });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("repeat advance should enter stream fetch");
    assert!(rt.pause_now(dev, "user_pause").is_some());
    release.notify_one();
    handle.await.unwrap();

    let sess = rt.media_session(dev).expect("paused session must remain");
    assert_eq!(
        sess.state,
        MediaState::Paused,
        "in-flight repeat advance must not overwrite Paused back to Playing"
    );
    assert!(sess.play_id.is_none());
    assert_eq!(
        play_begins.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "paused repeat advance must not start a new play on the gateway"
    );
    assert!(rt.hub.active_plays_for(dev).is_empty());
    assert!(!notes
        .lock()
        .unwrap()
        .iter()
        .any(|(k, _)| k == "media_repeat_advance"));

    // Sub-case B: user stops while repeat advance is opening the next track.
    notes.lock().unwrap().clear();
    rt.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks,
            index: 0,
            state: MediaState::Playing,
            position_ms: 4_000,
            play_id: Some("p-race-stop".into()),
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::One,
        },
    );
    let rt_task = rt.clone();
    let cancel_task = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(async move {
        rt_task
            .on_natural_play_end(dev, "p-race-stop", &cancel_task)
            .await;
    });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("repeat advance should enter stream fetch");
    assert!(rt.stop_now(dev, "user_stop").is_some());
    release.notify_one();
    handle.await.unwrap();

    assert!(
        rt.media_session(dev).is_none(),
        "in-flight repeat advance must not resurrect a stopped session"
    );
    assert_eq!(
        play_begins.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "stopped repeat advance must not start a new play on the gateway"
    );
}

/// #234 P1-4a: `supervise_playback` must continue draining queued control
/// events after `TryRecvError::Lagged(n)` and still distinguish a natural end
/// (`PlayFinished` only -> repeat advances) from a device/manual stop
/// (`StopStage` + `PlayFinished` -> session retires without repeat).
#[tokio::test]
async fn supervise_playback_drains_across_lagged_events_and_distinguishes_natural_end_from_stop() {
    use crate::gateway::StopOutcome;
    let dev = "dot2-d";
    let (rt, _gw, fetched, binding) = spawn_wav_library_runtime(dev).await;
    let notes: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = notes.clone();
    rt.set_event_sink(Arc::new(move |kind: &str, payload: Value| {
        sink.lock().unwrap().push((kind.to_string(), payload));
    }));
    let tracks = two_tracks(&binding);

    // Sub-case A: lagged receiver followed by PlayFinished -> repeat advances.
    let ticket_a = rt
        .hub
        .play_begin_ref(dev, "media", 16_000, Some("lag-natural".into()), None)
        .unwrap();
    let play_a = ticket_a.play_id.clone();
    let now = crate::id::now_ms();
    rt.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks: tracks.clone(),
            index: 0,
            state: MediaState::Playing,
            position_ms: 1_000,
            play_id: Some(play_a.clone()),
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::One,
        },
    );
    rt.register_playback(dev, &play_a);
    let (tx_a, rx_a) = tokio::sync::broadcast::channel::<DeviceEvent>(4);
    let stream_ended_a = Arc::new(AtomicBool::new(true));
    let cancel_a = Arc::new(AtomicBool::new(false));
    for i in 0..6 {
        let _ = tx_a.send(DeviceEvent::VolumeStage {
            play_id: format!("noise-{i}"),
            effective: 0.5,
            stage: "device_confirmed".into(),
        });
    }
    let _ = tx_a.send(DeviceEvent::PlayFinished {
        device: dev.into(),
        play_id: play_a.clone(),
        played_ms: 100,
        consumed_ms: Some(100),
        progress_basis: None,
        ts: now,
    });
    rt.supervise_playback(dev.into(), play_a.clone(), rx_a, cancel_a, stream_ended_a);
    tokio::time::sleep(Duration::from_millis(20)).await;
    rt.hub.forget_play(&play_a);

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let advanced = notes
                .lock()
                .unwrap()
                .iter()
                .any(|(k, v)| k == "media_repeat_advance" && v["prev_play_id"] == play_a);
            if advanced {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "supervisor must drain PlayFinished after TryRecvError::Lagged and advance repeat; notes={:?}, session={:?}, active_plays={:?}",
            *notes.lock().unwrap(),
            rt.media_session(dev),
            rt.hub.active_plays_for(dev)
        )
    });
    assert_eq!(fetched.lock().unwrap().as_slice(), &["t1".to_string()]);
    let _ = rt.stop_now(dev, "cleanup_lag_a");

    // Sub-case B: lagged receiver followed by StopStage + PlayFinished -> no repeat.
    notes.lock().unwrap().clear();
    fetched.lock().unwrap().clear();
    let ticket_b = rt
        .hub
        .play_begin_ref(dev, "media", 16_000, Some("lag-stop".into()), None)
        .unwrap();
    let play_b = ticket_b.play_id.clone();
    rt.seed_media_session_for_test(
        dev,
        MediaSession {
            tracks,
            index: 0,
            state: MediaState::Playing,
            position_ms: 1_000,
            play_id: Some(play_b.clone()),
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::One,
        },
    );
    rt.register_playback(dev, &play_b);
    let (tx_b, rx_b) = tokio::sync::broadcast::channel::<DeviceEvent>(4);
    let stream_ended_b = Arc::new(AtomicBool::new(true));
    let cancel_b = Arc::new(AtomicBool::new(false));
    for i in 0..6 {
        let _ = tx_b.send(DeviceEvent::VolumeStage {
            play_id: format!("noise-b-{i}"),
            effective: 0.5,
            stage: "device_confirmed".into(),
        });
    }
    let _ = tx_b.send(DeviceEvent::StopStage {
        play_id: play_b.clone(),
        stage: "device_confirmed".into(),
        outcome: StopOutcome::Confirmed {
            played_ms: 50,
            consumed_ms: Some(50),
            progress_basis: None,
        },
        played_ms: Some(50),
        consumed_ms: Some(50),
        progress_basis: None,
    });
    let _ = tx_b.send(DeviceEvent::PlayFinished {
        device: dev.into(),
        play_id: play_b.clone(),
        played_ms: 50,
        consumed_ms: Some(50),
        progress_basis: None,
        ts: now,
    });
    rt.supervise_playback(dev.into(), play_b.clone(), rx_b, cancel_b, stream_ended_b);
    tokio::time::sleep(Duration::from_millis(20)).await;
    rt.hub.forget_play(&play_b);

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if rt.media_session(dev).is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("supervisor must retire stopped session after Lagged drain");
    assert!(
        !notes
            .lock()
            .unwrap()
            .iter()
            .any(|(k, v)| k == "media_repeat_advance" && v["prev_play_id"] == play_b),
        "StopStage after Lagged must block repeat advance"
    );
    assert!(fetched.lock().unwrap().is_empty());
}

/// #234 P1-4b: mid-stream HTTP read timeout (`stream_client` read_timeout) and
/// decoder/downstream no-progress stall (`stream_idle_timeout_ms`) must both
/// reclaim the playing session without triggering a false repeat or hanging 60s.
#[tokio::test]
async fn http_read_timeout_and_decoder_stall_reclaim_session_without_false_repeat() {
    let dev = "dot2-d";
    let wav_first = Arc::new({
        // Claim a larger data chunk in the WAV header so symphonia keeps reading
        // after decoding the initial 100 ms PCM block.
        let mut w = tiny_wav_bytes();
        let inflated_data: u32 = 160_000;
        w[4..8].copy_from_slice(&(36 + inflated_data).to_le_bytes());
        w[40..44].copy_from_slice(&inflated_data.to_le_bytes());
        w
    });
    let wav_cb = wav_first.clone();
    let app = Router::new().route(
        "/rest/stream.view",
        get(move || {
            let wav_cb = wav_cb.clone();
            async move {
                let (tx, rx) =
                    tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(2);
                tokio::spawn(async move {
                    let _ = tx
                        .send(Ok(axum::body::Bytes::from((*wav_cb).clone())))
                        .await;
                    // Hang mid-stream without closing so either reqwest's read_timeout
                    // or pump's no-progress idle guard must reclaim the session.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });
                let stream = futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                });
                (
                    [("content-type", "audio/wav")],
                    axum::body::Body::from_stream(stream),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    ConfigService::new(store.clone())
        .save_music(
            MusicConfig {
                enabled: true,
                prefer_local: true,
                library: Some(MusicConnection {
                    endpoint: base.clone(),
                    user: "u".into(),
                }),
                online: None,
            },
            Some("lib-secret"),
            None,
            "test",
        )
        .unwrap();
    let binding = format!("{}|u", base.trim_end_matches('/'));
    let tracks = two_tracks(&binding);

    // 1) HTTP read_timeout fires mid-stream -> records `music_stream_error`,
    // forgets session, and never sets `stream_ended` or advances repeat.
    let hub1 = DeviceHub::new();
    let _gw1 = connect_mock_gateway(&hub1, dev).await;
    let short_read_client = reqwest::Client::builder()
        .read_timeout(Duration::from_millis(250))
        .build()
        .unwrap();
    let rt1 = MusicRuntime::with_client(store.clone(), hub1, short_read_client.clone());
    let notes1: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink1 = notes1.clone();
    rt1.set_event_sink(Arc::new(move |kind: &str, payload: Value| {
        sink1.lock().unwrap().push((kind.to_string(), payload));
    }));
    let resp1 = short_read_client
        .get(format!("{base}/rest/stream.view?id=t1"))
        .send()
        .await
        .unwrap();
    let ctx1 = ToolCtx::new(
        "voice:dot2-d",
        "c-read-to",
        Arc::new(AtomicBool::new(false)),
    );
    let (play1, _, _) = rt1
        .start_with_repeat(
            dev.into(),
            tracks.clone(),
            0,
            0,
            resp1,
            &ctx1,
            RepeatMode::One,
        )
        .await
        .expect("start_with_repeat before mid-stream read timeout");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if rt1.media_session(dev).is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("HTTP read timeout must reclaim playing session");
    {
        let log = notes1.lock().unwrap();
        assert!(
            log.iter()
                .any(|(k, v)| k == "music_stream_error" && v["play_id"] == play1),
            "HTTP read timeout must emit music_stream_error"
        );
        assert!(
            !log.iter()
                .any(|(k, v)| k == "media_repeat_advance" && v["prev_play_id"] == play1),
            "HTTP read timeout must not advance repeat"
        );
    }

    // 2) Decoder/downstream no-progress stall without HTTP error -> `Step::Idle`
    // guard fires, emits `music_stream_stalled`, and reclaims session.
    let hub2 = DeviceHub::new();
    let _gw2 = connect_mock_gateway(&hub2, dev).await;
    let no_to_client = reqwest::Client::new();
    let rt2 = MusicRuntime::with_client(store, hub2, no_to_client.clone());
    rt2.set_stream_idle_timeout_ms_for_test(250);
    let notes2: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink2 = notes2.clone();
    rt2.set_event_sink(Arc::new(move |kind: &str, payload: Value| {
        sink2.lock().unwrap().push((kind.to_string(), payload));
    }));
    let resp2 = no_to_client
        .get(format!("{base}/rest/stream.view?id=t1"))
        .send()
        .await
        .unwrap();
    let ctx2 = ToolCtx::new(
        "voice:dot2-d",
        "c-idle-stall",
        Arc::new(AtomicBool::new(false)),
    );
    let (play2, _, _) = rt2
        .start_with_repeat(dev.into(), tracks, 0, 0, resp2, &ctx2, RepeatMode::One)
        .await
        .expect("start_with_repeat before decoder stall");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if rt2.media_session(dev).is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("decoder/downstream no-progress stall must reclaim playing session");
    {
        let log = notes2.lock().unwrap();
        assert!(
            log.iter()
                .any(|(k, v)| k == "music_stream_stalled" && v["play_id"] == play2),
            "decoder/downstream stall must emit music_stream_stalled"
        );
        assert!(
            !log.iter()
                .any(|(k, v)| k == "media_repeat_advance" && v["prev_play_id"] == play2),
            "decoder/downstream stall must not advance repeat"
        );
    }
}
