//! Test fixtures shared by the integration suites.
//!
//! `FakeMusic` is a REAL protocol partner, not a stub: a Subsonic endpoint
//! (`search3`/`getRandomSongs`/`stream`, salted-token auth actually checked)
//! and a Personal Services endpoint (`/v1/search`, `/v1/stream/<id>`,
//! `X-Internal-Token` actually checked). It counts every request, which is the
//! only honest way to assert "prefer_local did not touch online" (MU-5).
//!
//! `FakeDevice` speaks the device protocol over a socket and confirms streamed
//! plays, so `music.play` reaches `device_confirmed` the same way it does on
//! the speaker.

#![allow(dead_code)]

use base64::Engine;
use hm_core::gateway::{CoreFrame, GatewayFrame};
use hm_core::store::Store;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

// --- the fake music services ------------------------------------------------

pub const LIB_SECRET: &str = "lib-secret-123";
pub const ONLINE_TOKEN: &str = "online-token-456";

/// The library's catalogue. `t08` is deliberately absent (no substitution).
pub fn catalogue() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("t01", "晚风信笺", "林晚"),
        ("t02", "光年之外的光", "陈屿"),
        ("t03", "小城路灯", "周与"),
        ("t04", "白噪与海", "Simple Sea"),
    ]
}

pub struct FakeMusic {
    pub base: String,
    /// Every request, in order: `search3:<q>`, `random`, `stream:<id>`,
    /// `online_search:<q>`, `online_stream:<id>`. The auth-checked ones only
    /// appear when the credentials were right.
    pub hits: Arc<Mutex<Vec<String>>>,
    /// Force a stream to fail (500) for a given id — the honest-failure path.
    pub fail_stream: Arc<Mutex<Option<String>>>,
    pub duration_secs: Arc<std::sync::atomic::AtomicU64>,
    pub stream_gate: Arc<Mutex<Option<tokio::sync::watch::Receiver<bool>>>>,
}

impl FakeMusic {
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
    pub fn count(&self, prefix: &str) -> usize {
        self.hits().iter().filter(|h| h.starts_with(prefix)).count()
    }
    pub fn clear(&self) {
        self.hits.lock().unwrap().clear();
    }
}

/// A tiny WAV of a 3 s tone at 24 kHz mono (symphonia decodes it; the device
/// resamples it). Long enough that a media play stays "live" across a second
/// turn's waiting-cue deadline (R1g/P4 tests: a cue must not start over a
/// song, and the song must not cut a live cue).
fn wav_bytes(freq: f32, duration_secs: u64) -> Vec<u8> {
    let rate = 24_000u32;
    let n = (rate as f32 * duration_secs as f32) as usize;
    let pcm: Vec<i16> = (0..n)
        .map(|i| {
            ((freq * i as f32 * 2.0 * std::f32::consts::PI / rate as f32).sin() * 6000.0) as i16
        })
        .collect();
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    let data_len = (pcm.len() * 2) as u32;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

pub async fn spawn_fake_music() -> FakeMusic {
    use axum::extract::{Path as AxPath, Query, State};
    use axum::http::HeaderMap;
    use axum::routing::get;

    #[derive(Clone)]
    struct S {
        hits: Arc<Mutex<Vec<String>>>,
        fail_stream: Arc<Mutex<Option<String>>>,
        duration_secs: Arc<std::sync::atomic::AtomicU64>,
        stream_gate: Arc<Mutex<Option<tokio::sync::watch::Receiver<bool>>>>,
    }

    async fn record(s: &S, hit: String) {
        s.hits.lock().unwrap().push(hit);
    }

    fn song_json(id: &str, title: &str, artist: &str) -> Value {
        json!({ "id": id, "title": title, "artist": artist })
    }

    async fn subsonic(
        State(s): State<S>,
        uri: axum::http::Uri,
        Query(q): Query<std::collections::HashMap<String, String>>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let ep = uri
            .path()
            .trim_start_matches("/rest/")
            .trim_end_matches(".view")
            .to_string();
        // Real Subsonic auth: t == md5(secret + s), and the user matches.
        let ok_auth = q.get("u").map(|u| u == "bakey").unwrap_or(false)
            && match (q.get("t"), q.get("s")) {
                (Some(t), Some(salt)) => {
                    use md5::{Digest, Md5};
                    let expect = format!("{:x}", Md5::digest(format!("{LIB_SECRET}{salt}")));
                    t == &expect
                }
                _ => false,
            };
        if !ok_auth {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(
                    json!({ "subsonic-response": { "status": "failed", "error": { "code": 40 } } }),
                ),
            )
                .into_response();
        }
        let songs = catalogue();
        let rows: Vec<Value> = match ep.as_str() {
            "search3" => {
                let query = q.get("query").cloned().unwrap_or_default();
                record(&s, format!("search3:{query}")).await;
                songs
                    .iter()
                    .filter(|(_, t, a)| {
                        t.contains(&query) || a.contains(&query) || query.is_empty() || query == "*"
                    })
                    .map(|(id, t, a)| song_json(id, t, a))
                    .collect()
            }
            "getRandomSongs" => {
                record(&s, "random".into()).await;
                songs
                    .iter()
                    .take(1)
                    .map(|(id, t, a)| song_json(id, t, a))
                    .collect()
            }
            "stream" => {
                let id = q.get("id").cloned().unwrap_or_default();
                record(&s, format!("stream:{id}")).await;
                let gate = s.stream_gate.lock().unwrap().clone();
                if let Some(mut gate) = gate {
                    while !*gate.borrow() {
                        gate.changed().await.unwrap();
                    }
                }
                if s.fail_stream.lock().unwrap().as_deref() == Some(id.as_str())
                    || !songs.iter().any(|(i, _, _)| *i == id.as_str())
                {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({ "error": "boom" })),
                    )
                        .into_response();
                }
                let body = wav_bytes(
                    330.0,
                    s.duration_secs.load(std::sync::atomic::Ordering::SeqCst),
                );
                return ([(axum::http::header::CONTENT_TYPE, "audio/wav")], body).into_response();
            }
            _ => {
                return (
                    axum::http::StatusCode::NOT_FOUND,
                    axum::Json(json!({ "subsonic-response": { "status": "failed" } })),
                )
                    .into_response();
            }
        };
        let key = if ep == "search3" {
            "searchResult3"
        } else {
            "randomSongs"
        };
        axum::Json(json!({ "subsonic-response": { "status": "ok", key: { "song": rows } } }))
            .into_response()
    }

    async fn online_search(
        State(s): State<S>,
        Query(q): Query<std::collections::HashMap<String, String>>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        if headers
            .get("x-internal-token")
            .and_then(|v| v.to_str().ok())
            != Some(ONLINE_TOKEN)
        {
            return (axum::http::StatusCode::UNAUTHORIZED, "bad token").into_response();
        }
        let query = q.get("q").cloned().unwrap_or_default();
        record(&s, format!("online_search:{query}")).await;
        // A catalogue search service answers ANY query with its best matches
        // (that is what makes "online" the fallback for an unknown title).
        let rows: Vec<Value> = catalogue()
            .iter()
            .map(|(id, t, a)| {
                json!({
                    "id": id.trim_start_matches('t').parse::<u64>().unwrap_or(1) + 1000,
                    "title": t, "artist": a
                })
            })
            .collect();
        axum::Json(json!({ "query": query, "results": rows })).into_response()
    }

    async fn online_stream(
        State(s): State<S>,
        AxPath(id): AxPath<String>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        if headers
            .get("x-internal-token")
            .and_then(|v| v.to_str().ok())
            != Some(ONLINE_TOKEN)
        {
            return (axum::http::StatusCode::UNAUTHORIZED, "bad token").into_response();
        }
        record(&s, format!("online_stream:{id}")).await;
        (
            [(axum::http::header::CONTENT_TYPE, "audio/wav")],
            wav_bytes(
                440.0,
                s.duration_secs.load(std::sync::atomic::Ordering::SeqCst),
            ),
        )
            .into_response()
    }

    let hits = Arc::new(Mutex::new(Vec::new()));
    let fail_stream = Arc::new(Mutex::new(None));
    let stream_gate = Arc::new(Mutex::new(None));
    let duration_secs = Arc::new(std::sync::atomic::AtomicU64::new(3));
    let state = S {
        duration_secs: duration_secs.clone(),
        hits: hits.clone(),
        fail_stream: fail_stream.clone(),
        stream_gate: stream_gate.clone(),
    };
    let app = axum::Router::new()
        .route("/rest/search3.view", get(subsonic))
        .route("/rest/getRandomSongs.view", get(subsonic))
        .route("/rest/stream.view", get(subsonic))
        .route("/v1/search", get(online_search))
        .route("/v1/stream/{id}", get(online_stream))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    FakeMusic {
        duration_secs,
        base: format!("http://{addr}"),
        hits,
        fail_stream,
        stream_gate,
    }
}

/// Write a music config into the app's store (the same validated path the
/// console uses — no back door).
pub fn configure_music(
    store: &Arc<Store>,
    fake: &FakeMusic,
    library: bool,
    online: bool,
    prefer_local: bool,
    enabled: bool,
) {
    store.allow_loopback_public_for_test();
    let svc = hm_core::config::ConfigService::new(store.clone());
    let cfg = hm_core::config::MusicConfig {
        enabled,
        prefer_local,
        library: library.then(|| hm_core::config::MusicConnection {
            endpoint: fake.base.clone(),
            user: "bakey".into(),
        }),
        online: online.then(|| hm_core::config::MusicConnection {
            endpoint: fake.base.clone(),
            user: String::new(),
        }),
    };
    svc.save_music(
        cfg,
        library.then_some(LIB_SECRET),
        online.then_some(ONLINE_TOKEN),
        "test",
    )
    .expect("music config saves");
}

// --- the fake device --------------------------------------------------------

/// A socket device that announces `play_stream` and confirms streamed plays.
/// Reads frames until the connection dies; every PlayBegin gets the accepted
/// + device_confirmed receipt pair the core waits for.
pub struct FakeDevice {
    pub port: u16,
}

pub async fn spawn_fake_device(port: u16) -> FakeDevice {
    spawn_fake_device_opts(port, true).await
}

/// `confirm=false` models a device that accepts the play and never confirms —
/// the honest `unknown` case (the tool must not report success).
pub async fn spawn_fake_device_opts(port: u16, confirm: bool) -> FakeDevice {
    spawn_fake_device_with(port, confirm, "full").await
}

/// R2-BM6: `duplex_capability` = what the device reports in `mode_report`
/// ("half" models a speaker that cannot do full duplex; a request for "full"
/// is then honestly negotiated back to "half").
pub async fn spawn_fake_device_with(
    port: u16,
    confirm: bool,
    duplex_capability: &'static str,
) -> FakeDevice {
    spawn_fake_device_custom(port, confirm, duplex_capability, "device_confirmed", true).await
}

/// Audit/BMT coverage seams: a device can stay silent on a volume or mode
/// command, or answer a volume command with a failed receipt. Those are the
/// cases where a management tool must NOT speak 「已…」.
pub async fn spawn_fake_device_custom(
    port: u16,
    confirm: bool,
    duplex_capability: &'static str,
    volume_stage: &'static str,
    reply_mode: bool,
) -> FakeDevice {
    // The listener is spawned as a task: connect once it is actually bound.
    let mut stream = None;
    for _ in 0..60 {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    let stream = stream.expect("gateway listener came up");
    let (rd, wr) = stream.into_split();
    let wr = Arc::new(tokio::sync::Mutex::new(wr));
    let hello = GatewayFrame::Hello {
        protocol: 0,
        supported_versions: vec![],
        min_protocol: 0,
        gateway: "fake-gateway 0.0.1".into(),
        gateway_version: "0.0.1".into(),
        devices: vec![hm_core::gateway::HelloDevice {
            stable_id: "dot2-d".into(),
            model: "AEOBC".into(),
            firmware: None,
            capabilities: vec![
                "audio_in".into(),
                "audio_out".into(),
                "volume".into(),
                "stop".into(),
                "play_stream".into(),
            ],
            audio_profile: "fireos_builtin_speaker".into(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
    };
    let send = |frame: GatewayFrame| {
        let wr = wr.clone();
        async move {
            let line = serde_json::to_string(&frame).unwrap() + "\n";
            let mut w = wr.lock().await;
            let _ = w.write_all(line.as_bytes()).await;
        }
    };
    send(hello).await;

    let wr2 = wr.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(rd).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(frame) = serde_json::from_str::<CoreFrame>(&line) else {
                continue;
            };
            match frame {
                CoreFrame::PlayBegin { task_id, .. } => {
                    if !confirm {
                        continue;
                    }
                    // The firmware answers `accepted` when it opens the stream
                    // and `device_confirmed` only after the last chunk drained.
                    for stage in ["accepted"] {
                        let f = GatewayFrame::PlayReceipt {
                            task_id: task_id.clone(),
                            stage: stage.into(),
                            detail: None,
                            ts: hm_core::id::now_ms(),
                        };
                        let _ = wr2
                            .lock()
                            .await
                            .write_all((serde_json::to_string(&f).unwrap() + "\n").as_bytes())
                            .await;
                    }
                }
                CoreFrame::PlayChunk { task_id, seq, .. } => {
                    if !confirm || seq != 0 {
                        continue;
                    }
                    // R2-BM5: the real firmware reports `streaming` when the
                    // FIRST block is consumed by the playback thread. The
                    // fake must model that stage, otherwise a music play only
                    // ever reaches `accepted` (begin validation) and can no
                    // longer back an 「已开始播放」 fact (audit Q2).
                    let f = GatewayFrame::PlayReceipt {
                        task_id,
                        stage: "streaming".into(),
                        detail: None,
                        ts: hm_core::id::now_ms(),
                    };
                    let _ = wr2
                        .lock()
                        .await
                        .write_all((serde_json::to_string(&f).unwrap() + "\n").as_bytes())
                        .await;
                }
                CoreFrame::PlayEnd { task_id } => {
                    if !confirm {
                        continue;
                    }
                    // End of stream: confirm it the way the device does.
                    let f = GatewayFrame::PlayReceipt {
                        task_id: task_id.clone(),
                        stage: "device_confirmed".into(),
                        detail: None,
                        ts: hm_core::id::now_ms(),
                    };
                    let _ = wr2
                        .lock()
                        .await
                        .write_all((serde_json::to_string(&f).unwrap() + "\n").as_bytes())
                        .await;
                    let f = GatewayFrame::PlayFinished {
                        device: "dot2-d".into(),
                        task_id,
                        played_ms: 400,
                        ts: hm_core::id::now_ms(),

                        consumed_ms: None,
                        progress_basis: None,
                    };
                    let _ = wr2
                        .lock()
                        .await
                        .write_all((serde_json::to_string(&f).unwrap() + "\n").as_bytes())
                        .await;
                }
                CoreFrame::Play { task_id, .. } => {
                    if !confirm {
                        continue;
                    }
                    for stage in ["accepted", "device_confirmed"] {
                        let f = GatewayFrame::PlayReceipt {
                            task_id: task_id.clone(),
                            stage: stage.into(),
                            detail: None,
                            ts: hm_core::id::now_ms(),
                        };
                        let _ = wr2
                            .lock()
                            .await
                            .write_all((serde_json::to_string(&f).unwrap() + "\n").as_bytes())
                            .await;
                    }
                }
                CoreFrame::Stop { task_id, .. } => {
                    let f = GatewayFrame::StopReceipt {
                        task_id,
                        stage: "device_confirmed".into(),
                        played_ms: Some(120),

                        consumed_ms: None,
                        progress_basis: None,
                        detail: None,
                    };
                    let _ = wr2
                        .lock()
                        .await
                        .write_all((serde_json::to_string(&f).unwrap() + "\n").as_bytes())
                        .await;
                }
                // R2-BM6: the device's OWN volume receipt is the fact the
                // management tool may speak; echo what was actually applied.
                CoreFrame::VolumeSet { task_id, value, .. } => {
                    if volume_stage == "none" {
                        continue;
                    }
                    let f = GatewayFrame::VolumeReceipt {
                        task_id,
                        effective: value,
                        unit: "linear".into(),
                        stage: volume_stage.into(),
                        // Deliberately absent: this fake is the OLD firmware,
                        // and the pending-map attribution must keep working.
                        device: None,
                    };
                    let _ = wr2
                        .lock()
                        .await
                        .write_all((serde_json::to_string(&f).unwrap() + "\n").as_bytes())
                        .await;
                }
                // And its own mode report; a half-capability device reports a
                // negotiated/effective "half" even when "full" was requested.
                CoreFrame::ModeSet {
                    device,
                    duplex_requested,
                } => {
                    if !reply_mode {
                        continue;
                    }
                    let effective: String =
                        if duplex_requested == "full" && duplex_capability == "half" {
                            "half".into()
                        } else {
                            duplex_requested.clone()
                        };
                    let f = GatewayFrame::ModeReport {
                        device,
                        duplex: hm_core::gateway::DuplexState {
                            capability: duplex_capability.into(),
                            requested: duplex_requested,
                            negotiated: effective.clone(),
                            effective,
                        },
                        aec: hm_core::gateway::AecState {
                            owner: "core".into(),
                            reference: "available".into(),
                        },
                    };
                    let _ = wr2
                        .lock()
                        .await
                        .write_all((serde_json::to_string(&f).unwrap() + "\n").as_bytes())
                        .await;
                }
                _ => {}
            }
        }
    });
    FakeDevice { port }
}

/// Decode a base64 chunk of the core's stream (for tests that check content).
pub fn decode_pcm(b64: &str) -> Vec<i16> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap_or_default();
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Listen for the core's stream frames until `n` chunks arrived (or timeout).
pub async fn collect_stream(port: u16, n: usize) -> Vec<Vec<i16>> {
    // A second connection would be a different device; instead the fake device
    // records nothing. Keep this helper for future use.
    let _ = (port, n);
    Vec::new()
}

pub fn addr_of(base: &str) -> String {
    base.trim_start_matches("http://").to_string()
}

pub fn socket_addr(base: &str) -> SocketAddr {
    addr_of(base).parse().unwrap()
}
