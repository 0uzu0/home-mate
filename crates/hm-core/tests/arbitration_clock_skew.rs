//! Tests for #76 / #171 RTT-bounded peer clock alignment & uncertainty-aware
//! wake arbitration tiebreaking.

use axum::{routing::post, Json, Router};
use hm_core::arbitration::{decide, decide_around, Arbiter, Claim, Why, COALESCE_MS};
use hm_core::fleet::FleetService;
use hm_core::store::Store;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

fn c(device: &str, heard: i64, rms: Option<f32>) -> Claim {
    Claim::new(device, heard, rms).with_calibrated(true)
}

#[test]
fn clock_uncertainty_overlap_resolves_via_deterministic_tiebreak() {
    // Two calibrated claims within LOUDER_DB (1.0 dB apart) and 25 ms apart in
    // normalized `heard_ms`, where the peer has a bounded 30 ms clock error.
    // Because 25 ms <= 30 ms uncertainty, raw wall-clock order must NOT decide;
    // deterministic fixed order (`Why::TieBreak`) must decide symmetrically.
    let a = c("dot2-d", 10_025, Some(-24.0)).with_clock_error_ms(Some(0));
    let b = c("dot2-e", 10_000, Some(-25.0)).with_clock_error_ms(Some(30));

    let d1 = decide(&[a.clone(), b.clone()], &[]).unwrap();
    let d2 = decide(&[b.clone(), a.clone()], &[]).unwrap();
    assert_eq!(d1.winner.device, "dot2-d");
    assert_eq!(d1.why, Why::TieBreak);
    assert_eq!(d2.winner.device, "dot2-d");
    assert_eq!(d2.why, Why::TieBreak);

    // When separated by 80 ms (> 30 ms uncertainty), arrival time IS reliable.
    let b_earlier = c("dot2-e", 9_945, Some(-25.0)).with_clock_error_ms(Some(30));
    let d3 = decide(&[a, b_earlier], &[]).unwrap();
    assert_eq!(d3.winner.device, "dot2-e");
    assert_eq!(d3.why, Why::First);
}

#[test]
fn peer_clock_skew_1100ms_is_normalized_for_coalesce_and_tiebreak() {
    // Reproduces the 2026-11-19 #76 live measurement:
    // dot2a wall clock lags dot2-d by 1100 ms (offset_ms = -1100).
    let store = Arc::new(Store::open_in_memory().unwrap());
    let arbiter = Arbiter::new(store);

    let local_mid = 1_795_060_000_000i64;
    let peer_server = local_mid - 1_100;
    arbiter.record_peer_clock("dot2a", peer_server, local_mid, 6);

    let (offset_ms, error_ms) = arbiter.peer_clock_estimate("dot2a").unwrap();
    assert_eq!(offset_ms, -1_100);
    assert_eq!(error_ms, 23); // rtt/2 (3) + 20ms margin

    // Same physical utterance: dot2-d heard at local_mid + 1400 (-20 dBFS),
    // dot2a heard at true time local_mid (so peer wall clock = local_mid - 1100,
    // -32 dBFS). Raw difference is 2500 ms (> COALESCE_MS = 2400 ms!), which
    // would previously fail to coalesce. After normalization, dot2a maps to
    // local_mid (1400 ms diff <= 2400 ms) and coalesces properly!
    let round_id = arbiter.begin(c("dot2-d", local_mid + 1_400, Some(-20.0)));
    let resp = arbiter.inbound(c("dot2a", local_mid - 1_100, Some(-32.0)));
    assert_eq!(
        resp["state"], "collected",
        "normalized peer claim must coalesce"
    );
    assert!(resp["server_time_ms"].as_u64().unwrap_or(0) > 0);

    let claims = arbiter.claims_so_far(&round_id);
    assert_eq!(claims.len(), 2);
    let peer = claims.iter().find(|x| x.device == "dot2a").unwrap();
    assert_eq!(peer.heard_ms, local_mid);
    assert_eq!(peer.clock_error_ms, Some(23));

    let dec = decide_around(&claims[0], &claims, &[]).unwrap();
    assert_eq!(dec.winner.device, "dot2-d");
    assert_eq!(dec.why, Why::Louder);
    assert!((peer.heard_ms - claims[0].heard_ms).abs() <= COALESCE_MS);
    arbiter.finish(&round_id);
}

#[tokio::test]
async fn fleet_wake_exchange_measures_peer_clock_offset_and_normalizes_reply() {
    // Peer server whose wall clock is 1200 ms behind local wall clock.
    let skew_ms: i64 = -1_200;
    let peer_app = Router::new().route(
        "/api/v0/fleet/wake",
        post(move |Json(_body): Json<Value>| async move {
            let local_now = hm_core::id::now_ms() as i64;
            let peer_now = local_now + skew_ms;
            let peer_claim = c("dot2a", peer_now, Some(-26.0));
            Json(json!({
                "state": "collected",
                "claim": peer_claim,
                "server_time_ms": peer_now,
            }))
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, peer_app).await;
    });

    let store = Arc::new(Store::open_in_memory().unwrap());
    let fleet = FleetService::new(store.clone());
    fleet
        .upsert(
            "dot2a",
            &format!("http://{addr}"),
            Some("test-peer-token"),
            false,
        )
        .unwrap();

    let arbiter = Arbiter::new(store);
    let now = hm_core::id::now_ms() as i64;
    let round_id = arbiter.begin(c("dot2-d", now, Some(-25.0)));

    let (claims, answered, peers) = arbiter
        .exchange(&round_id, Duration::from_millis(120))
        .await;
    assert_eq!(peers, 1);
    assert_eq!(answered, 1);
    assert_eq!(claims.len(), 2);

    let (offset_ms, error_ms) = arbiter
        .peer_clock_estimate("dot2a")
        .expect("peer clock recorded");
    assert!(
        (offset_ms - skew_ms).abs() <= 100,
        "expected offset near {skew_ms}, got {offset_ms}"
    );
    assert!(
        error_ms >= 20 && error_ms <= 200,
        "unexpected error_ms={error_ms}"
    );

    let peer_claim = claims.iter().find(|x| x.device == "dot2a").unwrap();
    assert!(peer_claim.clock_error_ms.is_some());
    assert!(
        (peer_claim.heard_ms - now).abs() <= 200,
        "peer heard_ms should be normalized near local now ({now}), got {}",
        peer_claim.heard_ms
    );

    arbiter.finish(&round_id);
    server.abort();
}

#[test]
fn three_node_uncertainty_tiebreak_has_unique_unanimous_winner() {
    // Reproduces the 3-node observer-perspective case: 0 ms true skew,
    // 10 ms LAN RTT (remote error=25 ms, local error=0 ms), 3 equidistant
    // speakers within LOUDER_DB at true times e=10_000, d=10_020, a=10_040.
    // All 3 observers MUST unanimously elect the exact same single winner.
    let view_on = |local_dev: &str| {
        let mk = |dev: &str, t: i64, db: f32| {
            let err = if dev == local_dev { Some(0) } else { Some(25) };
            c(dev, t, Some(db)).with_clock_error_ms(err)
        };
        let claims = [
            mk("dot2-e", 10_000, -24.5),
            mk("dot2-d", 10_020, -24.0),
            mk("dot2-a", 10_040, -24.2),
        ];
        decide(&claims, &[]).unwrap()
    };

    let dd = view_on("dot2-d");
    let da = view_on("dot2-a");
    let de = view_on("dot2-e");

    assert_eq!(dd.winner.device, "dot2-d");
    assert_eq!(da.winner.device, "dot2-d");
    assert_eq!(de.winner.device, "dot2-d");
    assert_eq!(dd.why, Why::TieBreak);
    assert_eq!(da.why, Why::TieBreak);
    assert_eq!(de.why, Why::TieBreak);
}

#[test]
fn cold_start_inbound_sent_at_ms_normalizes_before_rtt_and_seeds_begin() {
    // No pre-seeded `peer_clocks`: simulates >300s idle cold start AND
    // peer_id ("dot2-a") vs claim.device ("dot2a") mismatch.
    let store = Arc::new(Store::open_in_memory().unwrap());
    let arbiter = Arbiter::new(store);

    let now = hm_core::id::now_ms() as i64;
    let peer_skew = -1_100i64;
    let mut inbound_claim = c("dot2a", now + peer_skew, Some(-32.0));
    inbound_claim.sent_at_ms = Some(now + peer_skew);

    // Inbound arrives BEFORE local `begin` (and with zero prior RTT history).
    let resp = arbiter.inbound(inbound_claim);
    assert_eq!(resp["state"], "no_round");

    // Local wake occurs 1400 ms later (raw diff 2500 ms > COALESCE_MS = 2400 ms),
    // yet cold-start normalization already shifted dot2a to `~now`, so `begin()`
    // seeds it into the round and coalesces properly!
    let round_id = arbiter.begin(c("dot2-d", now + 1_400, Some(-20.0)));
    let claims = arbiter.claims_so_far(&round_id);
    assert_eq!(
        claims.len(),
        2,
        "cold-start normalized foreign claim must seed begin()"
    );
    let peer = claims.iter().find(|x| x.device == "dot2a").unwrap();
    assert_eq!(peer.clock_error_ms, Some(40));
    assert!((peer.heard_ms - now).abs() <= 100);
    assert!(peer.sent_at_ms.is_none());
    arbiter.finish(&round_id);
}

#[test]
fn untrusted_wire_clock_error_ms_is_stripped_when_unnormalized() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let arbiter = Arbiter::new(store);
    let now = hm_core::id::now_ms() as i64;

    // Caller injects a bogus 5000 ms clock_error_ms on the wire without sent_at_ms.
    let spoofed = c("dot2-c", now, Some(-25.0)).with_clock_error_ms(Some(5_000));
    let _ = arbiter.inbound(spoofed);
    let recent = arbiter.recent_foreign_claims("dot2-d");
    assert_eq!(recent.len(), 1);
    assert_eq!(
        recent[0].clock_error_ms, None,
        "untrusted wire clock_error_ms must be stripped"
    );
}

#[test]
fn inbound_active_wake_claim_upgrades_earlier_passive_presence_in_open_round() {
    // Reproduces 2026-11-19 18:04:29 live multi-room round:
    // dot2-d (-20.93 dBFS) fires KWS ~60 ms before dot2-e, queries dot2-e and
    // receives dot2-e's passive_presence (word: "", -26.81 dBFS). Then dot2-e
    // fires KWS (word: "你好小智", -9.37 dBFS) and pushes its active claim via
    // inbound(). dot2-d MUST upgrade dot2-e's foreign claim in the open round
    // and agree that dot2-e won.
    let store = Arc::new(Store::open_in_memory().unwrap());
    let arbiter = Arbiter::new(store);
    let now = hm_core::id::now_ms() as i64;

    let mut passive_e = c("dot2-e", now - 20, Some(-26.81));
    passive_e.word = String::new();
    let _ = arbiter.inbound(passive_e);

    let round_id = arbiter.begin(c("dot2-d", now, Some(-20.93)));
    let before = arbiter.claims_so_far(&round_id);
    assert_eq!(
        before.iter().find(|x| x.device == "dot2-e").unwrap().word,
        ""
    );

    let mut active_e = c("dot2-e", now + 60, Some(-9.37));
    active_e.word = "你好小智".into();
    let resp = arbiter.inbound(active_e);
    assert_eq!(resp["state"], "collected");

    let after = arbiter.claims_so_far(&round_id);
    let peer_e = after.iter().find(|x| x.device == "dot2-e").unwrap();
    assert_eq!(peer_e.word, "你好小智");
    assert_eq!(peer_e.rms_dbfs, Some(-9.37));
    let dec = hm_core::arbitration::decide_around(&after[0], &after, &[]).unwrap();
    assert_eq!(dec.winner.device, "dot2-e");
    assert_eq!(dec.why, Why::Louder);

    // Negative check: a subsequent quieter active claim from dot2-e (-35.0 dBFS)
    // with a smaller clock_error_ms must NOT overwrite the louder -9.37 dBFS claim.
    arbiter.record_peer_clock("dot2-e", now + 70, now + 70, 4);
    let mut quieter_e = c("dot2-e", now + 70, Some(-35.0));
    quieter_e.word = "你好小智".into();
    let _ = arbiter.inbound(quieter_e);
    let after_quiet = arbiter.claims_so_far(&round_id);
    let peer_e_after = after_quiet.iter().find(|x| x.device == "dot2-e").unwrap();
    assert_eq!(
        peer_e_after.rms_dbfs,
        Some(-9.37),
        "quieter active claim with tighter clock_error_ms must not clobber louder active claim"
    );
    arbiter.finish(&round_id);
}

#[test]
fn quieter_passive_presence_peers_never_beat_louder_low_snr_active_wake() {
    // Reproduces 2026-11-19 19:04:36.885 live multi-room round:
    // dot2-e (active KWS "你好小智", -9.62 dBFS, snr_db 5.52 < 6.0) competed
    // against two non-woken passive presence rooms: dot2-b (word: "", -36.33 dBFS,
    // snr_db 17.08) and dot2-c (word: "", -36.61 dBFS, snr_db 15.25).
    // Even if dot2-e fails MIN_SNR_DB, quieter passive rooms must never exclude
    // or beat the active KWS wake.
    let dot2_e = c("dot2-e", 1000, Some(-9.62))
        .with_snr_db(Some(5.52))
        .with_word("你好小智");
    let mut dot2_b = c("dot2-b", 664, Some(-36.33)).with_snr_db(Some(17.08));
    dot2_b.word = String::new();
    let mut dot2_c = c("dot2-c", 731, Some(-36.61)).with_snr_db(Some(15.25));
    dot2_c.word = String::new();

    let dec1 = hm_core::arbitration::decide(&[dot2_e.clone(), dot2_c.clone(), dot2_b.clone()], &[])
        .unwrap();
    assert_eq!(
        dec1.winner.device, "dot2-e",
        "quieter passive presence rooms must not win arrival-time fallback over active KWS wake"
    );
    assert_eq!(dec1.why, Why::IncompleteFallback);

    // Also when two quieter passive rooms differ by >= LOUDER_DB (-36.33 vs -50.49),
    // the louder of the two quiet passive rooms still cannot win Step 2 over a
    // -9.62 dBFS active KWS wake.
    let mut dot2a = c("dot2a", 600, Some(-50.49)).with_snr_db(Some(13.40));
    dot2a.word = String::new();
    let dec2 = hm_core::arbitration::decide(&[dot2_e.clone(), dot2_b.clone(), dot2a], &[]).unwrap();
    assert_eq!(
        dec2.winner.device, "dot2-e",
        "passive room at -36.33 dBFS must not win Step 2 loudness over active KWS wake at -9.62 dBFS"
    );

    // Conversely, a genuinely louder passive presence room (-8.0 dBFS, snr 18.0)
    // still beats a quieter far-room active KWS wake (-28.0 dBFS, snr 18.0) per #73 §3.
    let mut loud_passive = c("dot2-e", 1000, Some(-8.0)).with_snr_db(Some(18.0));
    loud_passive.word = String::new();
    let quiet_active = c("dot2-d", 950, Some(-28.0))
        .with_snr_db(Some(18.0))
        .with_word("你好小智");
    let dec3 = hm_core::arbitration::decide(&[quiet_active, loud_passive], &[]).unwrap();
    assert_eq!(dec3.winner.device, "dot2-e");
    assert_eq!(dec3.why, Why::Louder);

    // Two loud passive rooms tying within LOUDER_DB (-15.0 vs -16.0 dBFS) must
    // still beat a quiet far-room active KWS wake (-35.0 dBFS) per #73 §3.
    let mut loud_passive_a = c("dot2-e", 1000, Some(-15.0)).with_snr_db(Some(18.0));
    loud_passive_a.word = String::new();
    let mut loud_passive_b = c("dot2-c", 990, Some(-16.0)).with_snr_db(Some(18.0));
    loud_passive_b.word = String::new();
    let far_active = c("dot2-d", 950, Some(-35.0))
        .with_snr_db(Some(18.0))
        .with_word("你好小智");
    let dec4 =
        hm_core::arbitration::decide(&[far_active, loud_passive_a, loud_passive_b], &[]).unwrap();
    assert_ne!(
        dec4.winner.device, "dot2-d",
        "two loud passive rooms (-15/-16 dBFS) must still beat a quiet far-room KWS wake (-35 dBFS)"
    );
}
