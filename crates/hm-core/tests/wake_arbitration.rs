//! Wake arbitration over the real management API and a real fleet peer
//! (2026-11-15, contracts v0.30).
//!
//! The peer here is a small TCP stub that speaks the household API: it
//! answers `POST /api/v0/fleet/wake` with a claim of its own, which is exactly
//! what a second dot2 does. That makes the acceptance criteria testable
//! without a second speaker in the room:
//!
//! - a loser stays silent (no ack, no window),
//! - the winner answers as always,
//! - an unreachable peer degrades to single-node behaviour and says so,
//! - observe mode records the decision without changing behaviour.

use hm_core::api::AppState;
use hm_core::gateway::{GatewayFrame, InputOrigin};
use hm_core::llm::fake::FakeLlm;
use hm_core::speech::MockTts;
use hm_core::tool::mocks::MockInject;
use hm_core::voice::WakeArbitration;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A peer that answers every wake claim with a fixed claim.
struct PeerStub {
    base: String,
    calls: Arc<AtomicUsize>,
}

fn start_peer(claim: Value) -> PeerStub {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let mut raw = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut buf = Vec::new();
            let mut line = String::new();
            // Headers, then the body by Content-Length.
            loop {
                line.clear();
                if raw.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                buf.extend_from_slice(line.as_bytes());
                if line == "\r\n" {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
            let want = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; want];
            if want > 0 {
                let _ = raw.read_exact(&mut body);
            }
            if head.starts_with("post /api/v0/fleet/wake") {
                seen.fetch_add(1, Ordering::SeqCst);
                let answer = json!({ "state": "collected", "claim": claim }).to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    answer.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(answer.as_bytes());
            } else {
                let body = "{}";
                let head = format!(
                    "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            }
        }
    });
    PeerStub {
        base: format!("http://{addr}"),
        calls,
    }
}

/// This node's own gateway link: a real protocol connection, so the keyword
/// frame travels the same path a dot2's does (JSON over TCP → DeviceEvent).
struct Link {
    write: tokio::sync::Mutex<Option<tokio::net::tcp::OwnedWriteHalf>>,
    rx: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<hm_core::gateway::CoreFrame>>,
}

impl Link {
    async fn send(&self, f: &GatewayFrame) {
        let line = serde_json::to_string(f).unwrap();
        let mut guard = self.write.lock().await;
        let w = guard.as_mut().expect("link closed");
        use tokio::io::AsyncWriteExt;
        w.write_all(line.as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
    }
}

/// A minimal HTTP/1.1 POST: the tests have no client dependency, and what is
/// being exercised is the core's handler, not a client library.
async fn post_json(addr: std::net::SocketAddr, path: &str, body: String) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = sock.read_to_end(&mut out).await;
    String::from_utf8_lossy(&out).to_string()
}

async fn spawn_node() -> (Arc<AppState>, Arc<Link>, std::net::SocketAddr) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(FakeLlm::new(vec![]))),
        Some(Arc::new(MockTts::default())),
        None,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // The listener runs on its own thread; give it the socket before dialling.
    let mut last = None;
    let mut stream = None;
    for _ in 0..50 {
        match tokio::net::TcpStream::connect(("127.0.0.1", gw_port)).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    let stream = stream.unwrap_or_else(|| panic!("gateway listener never came up: {last:?}"));
    let (rd, wr) = stream.into_split();
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let link = Arc::new(Link {
        write: tokio::sync::Mutex::new(Some(wr)),
        rx: tokio::sync::Mutex::new(rx),
    });
    tokio::spawn(async move {
        let mut lines = BufReader::new(rd).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if let Ok(f) = serde_json::from_str::<hm_core::gateway::CoreFrame>(&l) {
                let _ = tx.send(f).await;
            }
        }
    });
    link.send(&GatewayFrame::Hello {
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
            ],
            audio_profile: "fireos_builtin_speaker".into(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
    })
    .await;
    // The hello_ack is the proof the device is registered.
    let mut rx = link.rx.lock().await;
    let ack = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("no hello_ack")
        .expect("link closed");
    assert!(
        matches!(ack, hm_core::gateway::CoreFrame::HelloAck { .. }),
        "{ack:?}"
    );
    drop(rx);
    (state, link, addr)
}

fn claim_at(device: &str, heard_ms: i64, rms: f64) -> Value {
    json!({ "device": device, "heard_ms": heard_ms, "rms_dbfs": rms, "room": "", "word": "你好高达" })
}

/// The house: this node plus one peer. `mode` selects the rollout step.
async fn house(
    peer_claim: Value,
    mode: WakeArbitration,
    with_token: bool,
) -> (Arc<AppState>, Arc<Link>, PeerStub, std::net::SocketAddr) {
    let peer = start_peer(peer_claim);
    let (state, link, addr) = spawn_node().await;
    state
        .fleet
        .upsert(
            "dot2-e",
            &peer.base,
            if with_token {
                Some("hm1_test_token")
            } else {
                None
            },
            false,
        )
        .unwrap();
    state
        .voice
        .set_wake_arbitration("dot2-d", mode, "test")
        .unwrap();
    (state, link, peer, addr)
}

/// One wake hit: the phrase ended 30 ms ago at −30 dBFS on this node.
async fn hit(link: &Arc<Link>) {
    let ts = hm_core::id::now_ms();
    link.send(&GatewayFrame::Keyword {
        origin: InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts,
        seq: 1,
        keyword: "你好高达".into(),
        fed_ms: Some(1000),
        self_heard: false,
        rms_dbfs: Some(-30.0),
        lag_ms: Some(30),
        bundle_id: None,
        kws_generation: None,
        kws_tier: None,
        wake_dur_ms: None,
        max_gap_ms: None,
        l2_passed: None,
    })
    .await;
}

fn kinds(state: &Arc<AppState>) -> Vec<String> {
    state
        .voice
        .recent(200)
        .iter()
        .filter_map(|e| e["kind"].as_str().map(str::to_string))
        .collect()
}

/// A peer stub that records every cue broadcast and answers 200.
fn start_cue_peer() -> (String, Arc<AtomicUsize>) {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let mut raw = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut head = String::new();
            loop {
                line.clear();
                if raw.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let want = head
                .to_ascii_lowercase()
                .lines()
                .find_map(|l| {
                    l.strip_prefix("content-length:")
                        .map(|v| v.trim().to_string())
                })
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; want];
            if want > 0 {
                let _ = raw.read_exact(&mut body);
            }
            if head
                .to_ascii_lowercase()
                .starts_with("post /api/v0/fleet/cue")
            {
                seen.fetch_add(1, Ordering::SeqCst);
            }
            let ok = "{}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                ok.len(),
                ok
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    (format!("http://{addr}"), calls)
}

/// Contract v2 #2: cues travel between the house's devices. Two facts are
/// pinned here: a peer's cue POST is recorded locally, and OUR cue is
/// broadcast to the peers (each device runs its own core, so this is the only
/// way a neighbour can recognise the echo).
#[tokio::test]
async fn cues_travel_between_the_houses_speakers() {
    let (peer_base, peer_cues) = start_cue_peer();
    let (state, _link, addr) = spawn_node().await;
    state
        .fleet
        .upsert("dot2-e", &peer_base, Some("hm1_test_token"), false)
        .unwrap();

    // Inbound: dot2-e says it just spoke 「来了」.
    let status = post_json(
        addr,
        "/api/v0/fleet/cue",
        json!({"device": "dot2-e", "phrase": "来了"}).to_string(),
    )
    .await;
    assert!(
        status.contains("200"),
        "the cue POST must be accepted: {status}"
    );
    assert!(
        kinds(&state).iter().any(|k| k == "mic_peer_cue"),
        "the peer cue must be recorded: {:?}",
        kinds(&state)
    );
    // An empty phrase is refused — a cue nobody said is not a fact.
    let bad = post_json(
        addr,
        "/api/v0/fleet/cue",
        json!({"device": "dot2-e", "phrase": " "}).to_string(),
    )
    .await;
    assert!(bad.contains("400"), "an empty cue must be refused: {bad}");

    // Outbound: our own cue reaches the peer.
    state
        .fleet
        .broadcast_cue("dot2-d", "来了", hm_core::id::now_ms());
    for _ in 0..40 {
        if peer_cues.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        peer_cues.load(Ordering::SeqCst),
        1,
        "a cue we speak must be broadcast to the peer"
    );
}

#[tokio::test]
async fn a_louder_neighbour_takes_the_wake_and_this_node_stays_silent() {
    // The peer heard the same phrase 10 ms earlier AND 20 dB louder: it is in
    // the room with the person. This node must not answer at all.
    let claim = claim_at("dot2-e", hm_core::id::now_ms() as i64, -10.0);
    let (state, link, peer, _addr) = house(claim, WakeArbitration::Enforce, true).await;
    hit(&link).await;
    wait_for_event(&state, Duration::from_secs(3), |e| {
        e["kind"] == "arbitration_lost"
    })
    .await;

    let log = kinds(&state);
    assert!(
        peer.calls.load(Ordering::SeqCst) >= 1,
        "the claim must be broadcast"
    );
    assert!(log.iter().any(|k| k == "wake_claim"), "{log:?}");
    assert!(
        log.iter().any(|k| k == "arbitration_lost"),
        "the loser must say so: {log:?}"
    );
    assert!(
        !log.iter()
            .any(|k| k == "keyword_wake" || k == "keyword_stop"),
        "a loser must not stop anything or open a window: {log:?}"
    );
}

#[tokio::test]
async fn the_earlier_node_keeps_the_wake_when_the_peer_is_later_and_quieter() {
    let claim = claim_at("dot2-e", (hm_core::id::now_ms() as i64) + 400, -45.0);
    let (state, link, _peer, _addr) = house(claim, WakeArbitration::Enforce, true).await;
    hit(&link).await;
    wait_for_event(&state, Duration::from_secs(3), |e| {
        e["kind"] == "keyword_wake"
    })
    .await;

    let log = kinds(&state);
    assert!(log.iter().any(|k| k == "arbitration_won"), "{log:?}");
    assert!(
        log.iter().any(|k| k == "keyword_wake"),
        "the winner answers exactly as before: {log:?}"
    );
    assert!(!log.iter().any(|k| k == "arbitration_lost"), "{log:?}");
}

#[tokio::test]
async fn an_unreachable_household_answers_alone_and_says_unavailable() {
    // A peer is configured but has no token: nothing can be asked, so the
    // node must behave like a single speaker and record the degradation.
    let claim = claim_at("dot2-e", hm_core::id::now_ms() as i64, -5.0);
    let (state, link, peer, _addr) = house(claim, WakeArbitration::Enforce, false).await;
    hit(&link).await;
    wait_for_event(&state, Duration::from_secs(3), |e| {
        e["kind"] == "keyword_wake"
    })
    .await;

    let log = kinds(&state);
    assert_eq!(
        peer.calls.load(Ordering::SeqCst),
        0,
        "no token = no request"
    );
    assert!(
        log.iter().any(|k| k == "arbitration_unavailable"),
        "{log:?}"
    );
    assert!(
        log.iter().any(|k| k == "keyword_wake"),
        "single-node behaviour must be unchanged: {log:?}"
    );
}

#[tokio::test]
async fn observe_records_the_decision_and_answers_anyway() {
    let claim = claim_at("dot2-e", hm_core::id::now_ms() as i64, -5.0);
    let (state, link, _peer, _addr) = house(claim, WakeArbitration::Observe, true).await;
    hit(&link).await;
    wait_for_event(&state, Duration::from_secs(3), |e| {
        e["kind"] == "arbitration_observed"
    })
    .await;

    let log = state.voice.recent(200);
    let observed = log
        .iter()
        .find(|e| e["kind"] == "arbitration_observed")
        .unwrap_or_else(|| panic!("no arbitration_observed: {log:?}"));
    assert_eq!(
        observed["would_win"],
        json!(false),
        "the evidence says this node would have lost: {observed:?}"
    );
    let kinds: Vec<&str> = log.iter().filter_map(|e| e["kind"].as_str()).collect();
    assert!(
        kinds.contains(&"keyword_wake"),
        "observe mode must not change behaviour: {kinds:?}"
    );
    assert!(!kinds.contains(&"arbitration_lost"), "{kinds:?}");
}

#[tokio::test]
async fn a_node_with_no_peers_never_bothers_the_log() {
    // Single speaker: arbitration is off by configuration AND there is
    // nothing to arbitrate — no claim events, no extra latency.
    let (state, link, _addr) = spawn_node().await;
    state
        .voice
        .set_wake_arbitration("dot2-d", WakeArbitration::Enforce, "test")
        .unwrap();
    hit(&link).await;
    wait_for_event(&state, Duration::from_secs(3), |e| {
        e["kind"] == "keyword_wake"
    })
    .await;
    let log = kinds(&state);
    assert!(!log.iter().any(|k| k.starts_with("arbitration")), "{log:?}");
    assert!(!log.iter().any(|k| k == "wake_claim"), "{log:?}");
    assert!(log.iter().any(|k| k == "keyword_wake"), "{log:?}");
}

/// The measured reality (dot2-b/dot2-c, 2026-11-15): the same phrase is
/// reported up to ~750 ms apart by two speakers, because detection latency
/// depends on how each microphone and model saw it. A node therefore decides
/// at 200 ms against an incomplete picture — and must still give the utterance
/// up when a stronger claim arrives before anything audible happens.
///
/// Here the peer is silent during the exchange (so we win provisionally) and
/// then POSTS a much louder claim 300 ms later, exactly as a slower peer
/// broadcast would arrive.
#[tokio::test]
async fn a_late_louder_claim_takes_the_utterance_away() {
    // During the exchange the peer hears nothing (`claim: null`), so this node
    // wins provisionally and opens its window — the situation a slower peer
    // creates by simply not having broadcast yet.
    let (state, link, _peer, addr) = house(Value::Null, WakeArbitration::Enforce, true).await;
    hit(&link).await;
    wait_for_event(&state, Duration::from_secs(3), |e| {
        e["kind"] == "keyword_wake"
    })
    .await;
    // A peer that heard it 400 ms BEFORE us and 25 dB louder.
    let ours = state
        .voice
        .recent(50)
        .iter()
        .find(|e| e["kind"] == "wake_claim")
        .and_then(|e| e["heard_ms"].as_i64())
        .expect("our claim is recorded");
    let late = json!({
        "device": "dot2-e",
        "heard_ms": ours - 400,
        "rms_dbfs": -5.0,
        "room": "客厅",
        "word": "你好高达",
    });
    let status = post_json(addr, "/api/v0/fleet/wake", late.to_string()).await;
    assert!(
        status.contains("200"),
        "the claim POST must be accepted: {status}"
    );
    assert!(
        status.contains("\"collected\""),
        "the late claim must join the open round: {status}"
    );
    // Wait deterministically for the coalesce window to fire the late-claim loss.
    wait_for_event(&state, Duration::from_secs(5), |e| {
        e["kind"] == "arbitration_lost" && e["late"] == json!(true)
    })
    .await;

    let log = state.voice.recent(200);
    let late_loss = log
        .iter()
        .find(|e| e["kind"] == "arbitration_lost" && e["late"] == json!(true))
        .unwrap_or_else(|| panic!("no late arbitration_lost: {log:?}"));
    assert_eq!(late_loss["decision"]["winner"], json!("dot2-e"));
    // The window this wake opened is gone again: nothing was spoken yet, so
    // giving the utterance up costs nothing audible.
    let listening = state
        .voice
        .mode_snapshot()
        .pointer("/devices/0/participation/conversation")
        .cloned()
        .unwrap_or(json!(null));
    assert_ne!(listening, json!(true), "the window must be closed again");
}

fn start_gated_peer(
    claim: Value,
) -> (
    PeerStub,
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
) {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    std::thread::spawn(move || {
        let mut arrived_tx = Some(arrived_tx);
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let mut raw = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut buf = Vec::new();
            let mut line = String::new();
            loop {
                line.clear();
                if raw.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                buf.extend_from_slice(line.as_bytes());
                if line == "\r\n" {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
            let want = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; want];
            if want > 0 {
                let _ = raw.read_exact(&mut body);
            }
            if head.starts_with("post /api/v0/fleet/wake") {
                seen.fetch_add(1, Ordering::SeqCst);
                if let Some(tx) = arrived_tx.take() {
                    let _ = tx.send(());
                    let _ = release_rx.recv_timeout(Duration::from_secs(5));
                }
                let answer = json!({ "state": "collected", "claim": claim }).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    answer.len(),
                    answer
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        }
    });
    (
        PeerStub {
            base: format!("http://{addr}"),
            calls,
        },
        arrived_rx,
        release_tx,
    )
}

fn start_delayed_peer(claim: Value, delay: Duration) -> PeerStub {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let mut raw = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut buf = Vec::new();
            let mut line = String::new();
            loop {
                line.clear();
                if raw.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                buf.extend_from_slice(line.as_bytes());
                if line == "\r\n" {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
            let want = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; want];
            if want > 0 {
                let _ = raw.read_exact(&mut body);
            }
            if head.starts_with("post /api/v0/fleet/wake") {
                seen.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(delay);
                let answer = json!({ "state": "collected", "claim": claim }).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    answer.len(),
                    answer
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        }
    });
    PeerStub {
        base: format!("http://{addr}"),
        calls,
    }
}

async fn wait_for_event(
    state: &hm_core::api::AppState,
    timeout: Duration,
    pred: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let log = state.voice.recent(100);
        if log.iter().any(&pred) || tokio::time::Instant::now() >= deadline {
            return log;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn slow_peer_response_within_window_still_participates() {
    let (slow_peer, arrived_rx, release_tx) = start_gated_peer(json!({
        "device": "dot2-e",
        "heard_ms": hm_core::id::now_ms(),
        "rms_dbfs": -12.0,
        "room": "客厅",
        "word": "你好高达",
    }));
    let (state, link, _unused_peer, _addr) =
        house(Value::Null, WakeArbitration::Enforce, true).await;
    state
        .fleet
        .upsert("dot2-e", &slow_peer.base, Some("hm1_test_token"), false)
        .unwrap();
    hit(&link).await;
    // Wait deterministically until the peer receives the claim, then release its
    // response immediately so CPU load never races against the 80ms request cap.
    tokio::time::timeout(Duration::from_secs(3), arrived_rx)
        .await
        .expect("peer should receive wake claim")
        .expect("arrived channel should signal");
    let _ = release_tx.send(());
    let log = wait_for_event(&state, Duration::from_secs(3), |e| {
        e["kind"] == "arbitration_lost" && e["decision"]["winner"] == json!("dot2-e")
    })
    .await;
    assert!(
        log.iter()
            .any(|e| e["kind"] == "arbitration_lost" && e["decision"]["winner"] == json!("dot2-e")),
        "slow peer within exchange window must still win: {log:?}"
    );
}

#[tokio::test]
async fn partial_peer_timeout_completes_with_responding_peer() {
    let fast_peer = start_peer(json!({
        "device": "dot2-e",
        "heard_ms": hm_core::id::now_ms(),
        "rms_dbfs": -10.0,
        "room": "客厅",
        "word": "你好高达",
    }));
    let hanging_peer = start_delayed_peer(
        json!({
            "device": "dot2-f",
            "heard_ms": hm_core::id::now_ms(),
            "rms_dbfs": -5.0,
            "room": "卧室",
            "word": "你好高达",
        }),
        Duration::from_millis(900),
    );
    let (state, link, _unused_peer, _addr) =
        house(Value::Null, WakeArbitration::Enforce, true).await;
    state
        .fleet
        .upsert("dot2-e", &fast_peer.base, Some("hm1_test_token"), false)
        .unwrap();
    state
        .fleet
        .upsert("dot2-f", &hanging_peer.base, Some("hm1_test_token"), false)
        .unwrap();
    hit(&link).await;
    let log = wait_for_event(&state, Duration::from_secs(3), |e| {
        e["kind"] == "arbitration_lost" && e["decision"]["winner"] == json!("dot2-e")
    })
    .await;
    assert!(
        log.iter()
            .any(|e| e["kind"] == "arbitration_lost" && e["decision"]["winner"] == json!("dot2-e")),
        "arbitration must complete with fast peer despite hanging peer: {log:?}"
    );
}

#[tokio::test]
async fn close_dbfs_levels_across_rooms_decide_deterministically() {
    // Local hit sends rms_dbfs=-30.0; peer is within 0.2 dB (-29.8 dBFS).
    let (state, link, _peer, _addr) = house(
        json!({
            "device": "dot2-e",
            "heard_ms": hm_core::id::now_ms(),
            "rms_dbfs": -29.8,
            "room": "客厅",
            "word": "你好高达",
        }),
        WakeArbitration::Enforce,
        true,
    )
    .await;
    hit(&link).await;
    let log = wait_for_event(&state, Duration::from_secs(3), |e| {
        matches!(
            e["kind"].as_str(),
            Some("arbitration_won" | "arbitration_lost" | "arbitration_tiebreak")
        )
    })
    .await;
    let decided = log
        .iter()
        .filter(|e| {
            matches!(
                e["kind"].as_str(),
                Some("arbitration_won" | "arbitration_lost" | "arbitration_tiebreak")
            )
        })
        .count();
    assert_eq!(
        decided, 1,
        "close dBFS levels must produce exactly one deterministic verdict: {log:?}"
    );
}

#[tokio::test]
async fn late_arbitration_loss_triggers_silent_retreat_without_retreat_ack() {
    // Peer returns null at 200ms so dot2-d wins provisionally, then posts a
    // louder claim via /api/v0/fleet/wake before COALESCE_MS (2400ms) closes.
    let (state, link, _peer, addr) = house(Value::Null, WakeArbitration::Enforce, true).await;
    let sessions = hm_core::session::SessionStore::new(state.store.clone());
    sessions.ensure("dev:dot2-d").unwrap();
    sessions
        .append("dev:dot2-d", "user", "keep this history")
        .unwrap();

    hit(&link).await;
    wait_for_event(&state, Duration::from_secs(3), |e| {
        e["kind"] == "keyword_wake"
    })
    .await;

    // Late louder claim from peer arrives ~250ms after wake.
    let late_claim = json!({
        "device": "dot2-e",
        "heard_ms": hm_core::id::now_ms(),
        "rms_dbfs": -12.0,
        "room": "客厅",
        "word": "你好高达",
    });
    let resp = post_json(addr, "/api/v0/fleet/wake", late_claim.to_string()).await;
    assert!(
        resp.contains("collected"),
        "late claim must be collected: {resp}"
    );

    let log = wait_for_event(&state, Duration::from_secs(4), |e| {
        e["kind"] == "session_closed" && e["reason"] == "arbitration_late_silent"
    })
    .await;

    assert!(
        log.iter()
            .any(|e| e["kind"] == "arbitration_lost" && e["late"] == true),
        "expected late arbitration_lost event: {log:?}"
    );
    assert!(
        !log.iter()
            .any(|e| e["kind"] == "play_local_ack" || e["ack"] == "retreat_ack"),
        "silent retreat must NOT speak retreat_ack: {log:?}"
    );
    let msgs = sessions.messages("dev:dot2-d").unwrap();
    assert_eq!(
        msgs.len(),
        1,
        "silent retreat must preserve session history: {msgs:?}"
    );
    let voice_snap = state.voice.mode_snapshot();
    assert_eq!(
        voice_snap
            .pointer("/devices/0/activity")
            .cloned()
            .unwrap_or(json!(null)),
        json!("idle"),
        "wake LED / activity must be cleared to idle after silent retreat"
    );
}

#[tokio::test]
async fn mic_gain_offset_db_shifts_arbitration_decision() {
    // Peer is at -25.0 dBFS; dot2-d raw hit is -30.0 dBFS (would lose),
    // but with +10.0 dB mic_gain_offset_db it becomes -20.0 dBFS and wins.
    let (state, link, _peer, _addr) = house(
        json!({
            "device": "dot2-e",
            "heard_ms": hm_core::id::now_ms(),
            "rms_dbfs": -25.0,
            "room": "客厅",
            "word": "你好高达",
        }),
        WakeArbitration::Enforce,
        true,
    )
    .await;
    state
        .config
        .save_unit_acoustic_profile(
            "dot2-d",
            hm_core::config::UnitAcousticProfile {
                mic_gain_offset_db: 10.0,
                ..Default::default()
            },
            "test",
        )
        .unwrap();

    hit(&link).await;
    let log = wait_for_event(&state, Duration::from_secs(3), |e| {
        e["kind"] == "arbitration_won" && e["decision"]["winner"] == "dot2-d"
    })
    .await;
    assert!(
        log.iter()
            .any(|e| e["kind"] == "arbitration_won" && e["decision"]["winner"] == "dot2-d"),
        "calibrated mic_gain_offset_db should shift decision to dot2-d: {log:?}"
    );
}

async fn spawn_named_node(
    device_id: &str,
    room: &str,
) -> (Arc<AppState>, Arc<Link>, std::net::SocketAddr) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    hm_core::config::ConfigService::new(store.clone())
        .save_device_meta(device_id, device_id, room, "test")
        .unwrap();
    let state = AppState::with_overrides(
        store,
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(FakeLlm::new(vec![]))),
        Some(Arc::new(MockTts::default())),
        None,
        None,
    );
    state.fleet.set_self_id(device_id).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = tokio::net::TcpStream::connect(("127.0.0.1", gw_port)).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stream = stream.expect("gateway listener never came up");
    let (rd, wr) = stream.into_split();
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let link = Arc::new(Link {
        write: tokio::sync::Mutex::new(Some(wr)),
        rx: tokio::sync::Mutex::new(rx),
    });
    tokio::spawn(async move {
        let mut lines = BufReader::new(rd).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if let Ok(f) = serde_json::from_str::<hm_core::gateway::CoreFrame>(&l) {
                let _ = tx.send(f).await;
            }
        }
    });
    link.send(&GatewayFrame::Hello {
        protocol: 0,
        supported_versions: vec![],
        min_protocol: 0,
        gateway: "fake-gateway 0.0.1".into(),
        gateway_version: "0.0.1".into(),
        devices: vec![hm_core::gateway::HelloDevice {
            stable_id: device_id.into(),
            model: "AEOBC".into(),
            firmware: None,
            capabilities: vec![
                "audio_in".into(),
                "audio_out".into(),
                "volume".into(),
                "stop".into(),
            ],
            audio_profile: "fireos_builtin_speaker".into(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
    })
    .await;
    let mut rx = link.rx.lock().await;
    let ack = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("no hello_ack")
        .expect("link closed");
    assert!(matches!(ack, hm_core::gateway::CoreFrame::HelloAck { .. }));
    drop(rx);
    state
        .voice
        .set_wake_arbitration(device_id, WakeArbitration::Enforce, "test")
        .unwrap();
    (state, link, addr)
}

async fn mesh_three_cores() -> (
    (Arc<AppState>, Arc<Link>, std::net::SocketAddr),
    (Arc<AppState>, Arc<Link>, std::net::SocketAddr),
    (Arc<AppState>, Arc<Link>, std::net::SocketAddr),
) {
    let a = spawn_named_node("dot2-a", "主卧").await;
    let b = spawn_named_node("dot2-b", "书房").await;
    let c = spawn_named_node("dot2-c", "儿童房").await;
    let tok_a = a.0.auth.provision("mesh-a").unwrap();
    let tok_b = b.0.auth.provision("mesh-b").unwrap();
    let tok_c = c.0.auth.provision("mesh-c").unwrap();
    let nodes = [
        ("dot2-a", &a.0, a.2, tok_a.as_str()),
        ("dot2-b", &b.0, b.2, tok_b.as_str()),
        ("dot2-c", &c.0, c.2, tok_c.as_str()),
    ];
    for (me, state, _, _) in &nodes {
        for (peer_id, _, peer_addr, peer_tok) in &nodes {
            if me != peer_id {
                state
                    .fleet
                    .upsert(
                        peer_id,
                        &format!("http://{peer_addr}"),
                        Some(peer_tok),
                        false,
                    )
                    .unwrap();
            }
        }
    }
    (a, b, c)
}

#[tokio::test]
async fn three_cores_cross_room_bleed_single_winner_and_non_owner_local_exit_is_ignored() {
    // ARB-01 & A7 (#116): 3 independent Core AppStates across different rooms ("主卧", "书房", "儿童房").
    // Part 1: Same utterance bleeds across all 3 rooms simultaneously -> exactly 1 winner ("dot2-c" at -12 dBFS).
    let ((state_a, link_a, _), (state_b, link_b, _), (state_c, link_c, _)) =
        mesh_three_cores().await;
    let ts = hm_core::id::now_ms();
    for (dev, link, rms) in [
        ("dot2-a", &link_a, -34.0f32),
        ("dot2-b", &link_b, -26.0f32),
        ("dot2-c", &link_c, -12.0f32),
    ] {
        link.send(&GatewayFrame::Keyword {
            origin: InputOrigin::Legacy,
            device: dev.into(),
            ts,
            seq: 1,
            keyword: "你好高达".into(),
            fed_ms: Some(900),
            self_heard: false,
            rms_dbfs: Some(rms),
            lag_ms: Some(20),
            bundle_id: None,
            kws_generation: None,
            kws_tier: None,
            wake_dur_ms: None,
            max_gap_ms: None,
            l2_passed: None,
        })
        .await;
    }

    wait_for_event(&state_c, Duration::from_secs(3), |e| {
        e["kind"] == "keyword_wake"
    })
    .await;
    wait_for_event(&state_a, Duration::from_secs(3), |e| {
        e["kind"] == "arbitration_lost"
    })
    .await;
    wait_for_event(&state_b, Duration::from_secs(3), |e| {
        e["kind"] == "arbitration_lost"
    })
    .await;

    assert!(kinds(&state_c).iter().any(|k| k == "keyword_wake"));
    assert!(!kinds(&state_a).iter().any(|k| k == "keyword_wake"));
    assert!(!kinds(&state_b).iter().any(|k| k == "keyword_wake"));

    // Part 2 (A7): Non-owner dot2-a emits LocalExitFlushed("停止播放") -> ignored as non-owner,
    // while dot2-c's active session remains open.
    link_a
        .send(&GatewayFrame::LocalExitFlushed {
            device: "dot2-a".into(),
            keyword: "停止播放".into(),
            ts: hm_core::id::now_ms(),
            latency_ms: 8,
            channels: vec!["voice".into()],
            detail: None,
        })
        .await;
    let log_a = wait_for_event(&state_a, Duration::from_secs(2), |e| {
        e["kind"] == "local_exit_ignored_not_owner"
    })
    .await;
    assert!(
        log_a
            .iter()
            .any(|e| e["kind"] == "local_exit_ignored_not_owner"),
        "non-owner LocalExitFlushed must be ignored: {log_a:?}"
    );
    assert_eq!(
        state_c
            .voice
            .mode_snapshot()
            .pointer("/devices/0/participation")
            .cloned(),
        Some(json!("conversation")),
        "owner dot2-c session must remain open after non-owner local exit"
    );
}

#[tokio::test]
async fn two_passive_nodes_stronger_than_active_node_produce_unique_final_winner() {
    // ARB-03 (#116): Active node dot2-a spots KWS at -35 dBFS; passive nodes dot2-b (-18 dBFS)
    // and dot2-c (-10 dBFS) both have NearEndPresence stronger than dot2-a.
    // Not only must `won_recently` revoke on dot2-b, but if both passive nodes run ASR fallback
    // (`Utterance` / `admit_asr_wake`), ONLY dot2-c is admitted as the unique final winner.
    let ((state_a, link_a, _), (state_b, link_b, _), (state_c, link_c, _)) =
        mesh_three_cores().await;
    let now = hm_core::id::now_ms();
    link_b
        .send(&GatewayFrame::NearEndPresence {
            device: "dot2-b".into(),
            ts_ms: now,
            duration_ms: 400,
            rms_dbfs: Some(-18.0),
            noise_floor_dbfs: Some(-55.0),
            snr_db: Some(25.0),
            clipped: false,
            echo_health: Some("healthy".into()),
            profile_version: None,
        })
        .await;
    link_c
        .send(&GatewayFrame::NearEndPresence {
            device: "dot2-c".into(),
            ts_ms: now,
            duration_ms: 400,
            rms_dbfs: Some(-10.0),
            noise_floor_dbfs: Some(-55.0),
            snr_db: Some(32.0),
            clipped: false,
            echo_health: Some("healthy".into()),
            profile_version: None,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(60)).await;

    // Active node dot2-a spots KWS at -35 dBFS.
    link_a
        .send(&GatewayFrame::Keyword {
            origin: InputOrigin::Legacy,
            device: "dot2-a".into(),
            ts: now,
            seq: 1,
            keyword: "你好高达".into(),
            fed_ms: Some(900),
            self_heard: false,
            rms_dbfs: Some(-35.0),
            lag_ms: Some(10),
            bundle_id: None,
            kws_generation: None,
            kws_tier: None,
            wake_dur_ms: None,
            max_gap_ms: None,
            l2_passed: None,
        })
        .await;

    let log_a = wait_for_event(&state_a, Duration::from_secs(3), |e| {
        e["kind"] == "arbitration_lost"
    })
    .await;
    assert!(
        log_a
            .iter()
            .any(|e| e["kind"] == "arbitration_lost" && e["decision"]["winner"] == "dot2-c"),
        "active node dot2-a must lose to the strongest passive node dot2-c: {log_a:?}"
    );

    // Runner-up passive node dot2-b must NOT retain `won_recently` after dot2-a propagates dot2-c's win,
    // and only dot2-c may win passive eligibility.
    assert!(
        !state_b
            .voice
            .arbiter_won_recently_for_test("dot2-b", Duration::from_secs(5)),
        "runner-up passive node dot2-b must have its provisional win revoked"
    );
    assert!(
        state_c
            .voice
            .arbiter_won_recently_for_test("dot2-c", Duration::from_secs(5)),
        "strongest passive node dot2-c must hold the passive win"
    );
    let _ = (link_b, link_c);
}
