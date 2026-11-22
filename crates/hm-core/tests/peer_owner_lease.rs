//! Tests for #116 (`PEER-OWNER`): distributed `/api/v0/fleet/owner` lease
//! replication, monotonic epoch fencing (`ARB-04`), near-end survival over
//! remote playback text match (`ARB-08`), and synchronous loser win revocation (`ARB-09`).

use hm_core::api::{router, AppState};
use hm_core::arbitration::Arbiter;
use hm_core::fleet::FleetService;
use hm_core::sovereignty::{FleetOwnerNotice, FleetPlaybackNotice, RetreatAuth, RoomSessionDomain};
use hm_core::store::Store;
use hm_core::tool::mocks::MockInject;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

struct TestCore {
    state: Arc<AppState>,
    addr: SocketAddr,
    token: String,
    server: tokio::task::JoinHandle<()>,
    dir: std::path::PathBuf,
}

impl Drop for TestCore {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn spawn_test_core(label: &str) -> TestCore {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let dir = std::env::temp_dir().join(format!("hm-owner-test-{label}-{}", hm_core::id::now_ms()));
    let state = AppState::new(store, dir.clone(), MockInject::default(), None);
    state.voice.set_memory_extraction(false);
    let token = state.auth.provision(label).unwrap();
    let app = router(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    TestCore {
        state,
        addr,
        token,
        server,
        dir,
    }
}

#[tokio::test]
async fn fleet_owner_http_replicates_commit_rejects_stale_epoch_and_releases() {
    let core = spawn_test_core("peer-test").await;
    let state = core.state.clone();
    let addr = core.addr;
    let token = core.token.clone();

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/api/v0/fleet/owner");

    // 1. Peer dot2-a commits room ownership in "客厅" (epoch 1).
    let commit = FleetOwnerNotice {
        session_id: "rsess-1".into(),
        room: "客厅".into(),
        owner_device: "dot2-a".into(),
        epoch: 1,
        boot_id: "rboot-a1".into(),
        active: true,
        lease_ttl_ms: 15_000,
        voice_ttl_ms: 9_000,
        context_ttl_ms: 240_000,
        clear_context: false,
    };
    let r1 = client
        .post(&url)
        .bearer_auth(&token)
        .json(&commit)
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);
    let b1: serde_json::Value = r1.json().await.unwrap();
    assert_eq!(b1["accepted"], true);

    let now = hm_core::id::now_ms();
    assert!(state
        .voice
        .sovereignty
        .has_active_voice_window("客厅", "dot2-a", now));
    assert!(matches!(
        state.voice.sovereignty.can_retreat("卧室", "dot2-b", false, now),
        RetreatAuth::DeniedNotOwner { ref owner_device, .. } if owner_device == "dot2-a"
    ));

    // 2. Duplicate/stale epoch 1 must be rejected (`accepted: false`).
    let r_dup = client
        .post(&url)
        .bearer_auth(&token)
        .json(&commit)
        .send()
        .await
        .unwrap();
    assert_eq!(r_dup.status(), 200);
    let b_dup: serde_json::Value = r_dup.json().await.unwrap();
    assert_eq!(b_dup["accepted"], false);

    // 3. Independent room "卧室" can commit concurrently without evicting "客厅".
    let bedroom = FleetOwnerNotice {
        session_id: "rsess-2".into(),
        room: "卧室".into(),
        owner_device: "dot2-c".into(),
        epoch: 1,
        boot_id: "rboot-c1".into(),
        active: true,
        lease_ttl_ms: 15_000,
        voice_ttl_ms: 9_000,
        context_ttl_ms: 240_000,
        clear_context: false,
    };
    let r_br = client
        .post(&url)
        .bearer_auth(&token)
        .json(&bedroom)
        .send()
        .await
        .unwrap();
    assert_eq!(r_br.status(), 200);
    assert_eq!(
        state
            .voice
            .sovereignty
            .snapshot(hm_core::id::now_ms())
            .len(),
        2
    );

    // 4. Peer dot2-a releases ownership (epoch 2, active: false).
    let release = FleetOwnerNotice {
        session_id: "rsess-1".into(),
        room: "客厅".into(),
        owner_device: "dot2-a".into(),
        epoch: 2,
        boot_id: "rboot-a1".into(),
        active: false,
        lease_ttl_ms: 0,
        voice_ttl_ms: 0,
        context_ttl_ms: 0,
        clear_context: true,
    };
    let r_rel = client
        .post(&url)
        .bearer_auth(&token)
        .json(&release)
        .send()
        .await
        .unwrap();
    assert_eq!(r_rel.status(), 200);
    let b_rel: serde_json::Value = r_rel.json().await.unwrap();
    assert_eq!(b_rel["accepted"], true);
    assert!(!state.voice.sovereignty.has_active_voice_window(
        "客厅",
        "dot2-a",
        hm_core::id::now_ms()
    ));

    // 5. Out-of-order commit with old epoch 1 cannot resurrect released ownership.
    let r_stale = client
        .post(&url)
        .bearer_auth(&token)
        .json(&commit)
        .send()
        .await
        .unwrap();
    let b_stale: serde_json::Value = r_stale.json().await.unwrap();
    assert_eq!(b_stale["accepted"], false);
    assert!(!state.voice.sovereignty.has_active_voice_window(
        "客厅",
        "dot2-a",
        hm_core::id::now_ms()
    ));

    // 6. Invalid epoch 0 is rejected with 400 Bad Request.
    let r_zero = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({
            "session_id": "s",
            "room": "客厅",
            "owner_device": "dot2-a",
            "epoch": 0,
            "active": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r_zero.status(), 400);
}

#[tokio::test]
async fn fleet_service_broadcast_owner_lease_reaches_peer() {
    let core = spawn_test_core("sender").await;
    let peer_state = core.state.clone();
    let addr = core.addr;
    let peer_token = core.token.clone();

    let sender_store = Arc::new(Store::open_in_memory().unwrap());
    let fleet = FleetService::new(sender_store);
    fleet
        .upsert(
            "dot2-b",
            &format!("http://{addr}"),
            Some(&peer_token),
            false,
        )
        .unwrap();

    let domain = RoomSessionDomain::new();
    let now = hm_core::id::now_ms();
    let owner = domain.claim_or_renew("书房", "dot2-d", now, 9_000, 240_000);
    let notice = RoomSessionDomain::notice_from_owner(&owner, now);
    fleet.broadcast_owner_lease(&notice);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if peer_state.voice.sovereignty.has_active_voice_window(
            "书房",
            "dot2-d",
            hm_core::id::now_ms(),
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        peer_state.voice.sovereignty.has_active_voice_window(
            "书房",
            "dot2-d",
            hm_core::id::now_ms()
        ),
        "peer must receive broadcast_owner_lease commit"
    );
}

#[test]
fn owner_core_restart_new_boot_id_resyncs_from_epoch_1_and_retires_old_boot() {
    let peer = RoomSessionDomain::new();
    let now = 200_000u64;

    // Boot 1 reaches epoch 30.
    assert!(peer.apply_remote_owner(
        FleetOwnerNotice {
            session_id: "s-old".into(),
            room: "儿童房".into(),
            owner_device: "dot2-d".into(),
            epoch: 30,
            boot_id: "rboot-1".into(),
            active: true,
            lease_ttl_ms: 15_000,
            voice_ttl_ms: 9_000,
            context_ttl_ms: 240_000,
            clear_context: false,
        },
        now,
    ));

    // dot2-d restarts with fresh `rboot-2` and `epoch = 1`: MUST be accepted!
    assert!(peer.apply_remote_owner(
        FleetOwnerNotice {
            session_id: "s-new".into(),
            room: "儿童房".into(),
            owner_device: "dot2-d".into(),
            epoch: 1,
            boot_id: "rboot-2".into(),
            active: true,
            lease_ttl_ms: 15_000,
            voice_ttl_ms: 9_000,
            context_ttl_ms: 240_000,
            clear_context: false,
        },
        now + 1_000,
    ));

    // Delayed packet from retired `rboot-1` (even with higher epoch 31) MUST be rejected!
    assert!(!peer.apply_remote_owner(
        FleetOwnerNotice {
            session_id: "s-old".into(),
            room: "儿童房".into(),
            owner_device: "dot2-d".into(),
            epoch: 31,
            boot_id: "rboot-1".into(),
            active: false,
            lease_ttl_ms: 0,
            voice_ttl_ms: 0,
            context_ttl_ms: 0,
            clear_context: true,
        },
        now + 1_500,
    ));
    assert!(peer.has_active_voice_window("儿童房", "dot2-d", now + 2_000));
}

#[test]
fn arb09_passive_contention_loss_clears_stale_won_recently() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let arbiter = Arbiter::new(store);
    let now = 50_000i64;

    arbiter.record_win("dot2-b");
    assert!(arbiter.won_recently("dot2-b", Duration::from_secs(5)));

    // dot2-b has weak passive presence (-38 dBFS); stronger peer dot2-a (-20 dBFS) arrives.
    arbiter.record_passive_presence(
        hm_core::arbitration::Claim::new("dot2-b", now, Some(-38.0)).with_calibrated(true),
    );
    let _ = arbiter.inbound(
        hm_core::arbitration::Claim::new("dot2-a", now, Some(-20.0)).with_calibrated(true),
    );
    assert!(
        !arbiter.won_recently("dot2-b", Duration::from_secs(5)),
        "losing passive contention must synchronously clear stale won_recently"
    );
}

#[test]
fn arb08_strong_local_near_end_survives_remote_playback_text_match() {
    let domain = RoomSessionDomain::new();
    let now = 100_000u64;
    domain.record_playback_notice(FleetPlaybackNotice {
        device: "dot2-a".into(),
        room: "客厅".into(),
        channel: "voice".into(),
        active: true,
        text_preview: "以后叫我你好小智就可以唤醒我".into(),
        at_ms: now,
        ttl_ms: 15_000,
    });

    // Without strong local near-end evidence, remote text match suppresses bleed.
    let suppressed =
        domain.suppress_bleed_for_phrase("dot2-b", "你好小智", false, false, now + 500);
    assert!(
        suppressed.is_some(),
        "weak/absent near-end must abstain on text match"
    );

    // With strong independent local near-end evidence, local request survives (ARB-08).
    let survived = domain.suppress_bleed_for_phrase("dot2-b", "你好小智", false, true, now + 500);
    assert!(
        survived.is_none(),
        "strong local near-end evidence must NOT be unconditionally suppressed by remote text match"
    );
}

#[test]
fn arb09_clear_win_synchronously_revokes_recent_win_eligibility() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let arbiter = Arbiter::new(store);
    arbiter.record_win("dot2-c");
    assert!(arbiter.won_recently("dot2-c", Duration::from_secs(5)));
    arbiter.clear_win("dot2-c");
    assert!(!arbiter.won_recently("dot2-c", Duration::from_secs(5)));
}
