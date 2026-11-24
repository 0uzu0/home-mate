//! Reproduce the speaker wifi-change wedge: a fake gateway connects, the
//! API POST /devices/{id}/wifi is made, the client DROPS mid-request (what
//! the wlan cut did), WifiState arrives late, and the core must keep
//! serving. Runs against a local core spawned in-process.

use hm_core::api::AppState;
use hm_core::gateway::{CoreFrame, GatewayFrame, HelloDevice};
use hm_core::tool::mocks::MockInject;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wifi_change_with_client_drop_keeps_core_serving() {
    let dir = std::env::temp_dir().join(format!("hm-wifi-repro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(hm_core::store::Store::open(&dir.join("t.db")).unwrap());
    // The gateway listener binds from HM_GATEWAY_BIND at construction —
    // pick the port BEFORE building the state.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pre_addr = probe.local_addr().unwrap();
    drop(probe);
    std::env::set_var("HM_GATEWAY_BIND", pre_addr.to_string());
    let state = AppState::with_overrides(
        store,
        dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let gw_addr = pre_addr;

    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");

    // Fake gateway: hello with wifi capability, then answer WifiConfig with
    // a delayed WifiState (like the 40 s device watchdog).
    let gw = state.hub.clone();
    // The hub LISTENS; the fake gateway CONNECTS (production roles).
    state.start_gateway_listener(gw_addr);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    tokio::spawn(async move {
        // The hub's listener binds inside a spawned task, so connecting right
        // away races the bind (bit us as a "connection refused" flake once the
        // suite grew: more parallel gateways = wider window). Retry briefly.
        let sock = loop {
            match TcpStream::connect(gw_addr).await {
                Ok(s) => break s,
                Err(e) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    if tokio::time::Instant::now() > deadline {
                        panic!("fake gateway could not reach the hub listener: {e}");
                    }
                }
            }
        };
        let (rx, mut tx) = sock.into_split();
        // hello
        let hello = GatewayFrame::Hello {
            protocol: 0,
            supported_versions: vec![],
            min_protocol: 0,
            gateway: "fake".into(),
            gateway_version: "0.0.2".into(),
            devices: vec![HelloDevice {
                stable_id: "dot2-d".into(),
                model: "AEOBC".into(),
                firmware: Some("hm-firmware 0.0.2 (test)".into()),
                capabilities: vec!["audio_out".into(), "wifi".into()],
                audio_profile: "fireos_builtin_speaker".into(),
                wifi_ssid: Some("TestNet".into()),
            }],
            aec_tail_ms: None,
            profile_version: None,
            auth_token: None,
        };
        let line = serde_json::to_string(&hello).unwrap();
        tx.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        tx.flush().await.unwrap();

        let (r, mut w) = {
            let (r, w) = (rx, tx);
            (tokio::io::BufReader::new(r), w)
        };
        let mut lines = r.lines();
        // read frames until a WifiConfig arrives, reply WifiState after the
        // client drop is signalled (instead of a hardcoded multi-second sleep).
        let (drop_tx, drop_rx) = tokio::sync::oneshot::channel::<()>();
        let mut drop_rx = Some(drop_rx);
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(frame) = serde_json::from_str::<CoreFrame>(&line) {
                if matches!(frame, CoreFrame::WifiConfig { .. }) {
                    if let Some(rx) = drop_rx.take() {
                        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), rx).await;
                    }
                    let reply = GatewayFrame::WifiState {
                        device: "dot2-d".into(),
                        ts: 0,
                        ssid: "NewNet".into(),
                        state: "applied".into(),
                        detail: Some("joined".into()),
                    };
                    let l = serde_json::to_string(&reply).unwrap();
                    w.write_all(format!("{l}\n").as_bytes()).await.unwrap();
                    w.flush().await.unwrap();
                }
            }
        }
        drop(drop_tx);
    });

    // connect the hub listener: start it on gw_addr via the API state we
    // already have — the AppState constructed its gateway listener in
    // main.rs only; here use start_gateway_listener explicitly.
    let _ = gw;

    // Wait until the hello device is registered (wifi capability visible).
    for _ in 0..80 {
        let st: Value = reqwest::get(format!("{base}/api/v0/status"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let ok = st["gateway"]["devices"]
            .as_array()
            .map(|d| {
                !d.is_empty()
                    && d[0]["capabilities"]
                        .as_array()
                        .map(|c| c.iter().any(|x| x == "wifi"))
                        .unwrap_or(false)
            })
            .unwrap_or(false);
        if ok {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    // Client that drops mid-request (max-time < watchdog reply delay).
    let dropped = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(80))
        .build()
        .unwrap()
        .post(format!("{base}/api/v0/devices/dot2-d/wifi"))
        .json(&serde_json::json!({ "ssid": "NewNet", "psk": "12345678" }))
        .send()
        .await;
    // The wlan cut kills the HTTP client mid-request — the core must survive.
    assert!(dropped.is_err(), "client should have timed out mid-request");

    // Poll until the late WifiState event is observed by the core, then assert
    // the core is still serving.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let alive: Value = reqwest::get(format!("{base}/api/v0/status"))
                .await
                .expect("core must still answer after a dropped wifi client")
                .json()
                .await
                .unwrap();
            let applied = state
                .voice
                .recent(20)
                .iter()
                .any(|e| e["kind"] == "wifi_state" && e["state"] == "applied");
            if applied && alive["gateway"]["connected"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("late WifiState applied and core remained responsive");

    let _ = std::fs::remove_dir_all(&dir);
}
