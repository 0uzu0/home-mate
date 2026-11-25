//! Deterministic tests for Gateway v2 `config_apply_v2` apply timeouts and
//! fleet readback budgets across fast blocks vs slow `hm-kws` reloads.

use super::fleet::readback_timeout_for;
use super::gateway_dispatch::{apply_timeout_for, dispatch_block_v2};
use crate::gateway::{
    ConfigBlock, ConfigReceiptStatus, CoreFrame, DeviceHub, GatewayFrame, HelloDevice,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

#[test]
fn apply_timeout_budget_allows_slow_kws_reloads_and_keeps_fast_blocks_at_5s() {
    assert_eq!(
        apply_timeout_for(ConfigBlock::Kws),
        Duration::from_secs(25),
        "ConfigBlock::Kws must exceed Linux Gateway's 20s KWS_RELOAD_TIMEOUT_MS"
    );
    for fast in [
        ConfigBlock::AudioEq,
        ConfigBlock::Led,
        ConfigBlock::Acoustics,
        ConfigBlock::Duplex,
    ] {
        assert_eq!(apply_timeout_for(fast), Duration::from_secs(5));
    }
}

#[test]
fn fleet_readback_timeout_tracks_kws_touching_blocks_and_rollback() {
    for kws_block in ["reflex", "mode", "node"] {
        assert_eq!(
            readback_timeout_for(&[kws_block.to_string()]),
            Duration::from_secs(30)
        );
    }
    assert_eq!(readback_timeout_for(&[]), Duration::from_secs(30));
    assert_eq!(
        readback_timeout_for(&["audio_eq".into(), "device_led_configs".into()]),
        Duration::from_secs(5)
    );
}

struct MockGatewayHandles {
    persisted_sent: Arc<Notify>,
    release_active: Option<Arc<Notify>>,
}

async fn spawn_mock_v2_gateway(
    hub: Arc<DeviceHub>,
    device: &'static str,
    capabilities: Vec<&'static str>,
    send_active_on_release: bool,
) -> MockGatewayHandles {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hub_srv = hub.clone();
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let _ = hub_srv.serve_connection(stream).await;
        }
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let hello = GatewayFrame::Hello {
        protocol: 2,
        min_protocol: 0,
        supported_versions: vec![0, 2],
        gateway: "mock-lx06-gw".into(),
        gateway_version: "0.3.0".into(),
        devices: vec![HelloDevice {
            stable_id: device.into(),
            model: "LX06".into(),
            firmware: Some("1.0".into()),
            capabilities: capabilities.into_iter().map(str::to_string).collect(),
            audio_profile: "lx06".into(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
        auth_token: None,
    };
    wr.write_all(serde_json::to_string(&hello).unwrap().as_bytes())
        .await
        .unwrap();
    wr.write_all(b"\n").await.unwrap();

    // Drain HelloAck + TimeSet
    let _ = lines.next_line().await.unwrap().unwrap();
    let _ = lines.next_line().await.unwrap().unwrap();

    for _ in 0..50 {
        if hub.device_exists(device) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let persisted_sent = Arc::new(Notify::new());
    let release_active = send_active_on_release.then(|| Arc::new(Notify::new()));
    let persisted_tx = persisted_sent.clone();
    let release_rx = release_active.clone();

    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(frame) = serde_json::from_str::<CoreFrame>(&line) else {
                continue;
            };
            match frame {
                CoreFrame::ConfigStateGet { request_id, device } => {
                    let resp = GatewayFrame::ConfigState {
                        request_id: Some(request_id),
                        device,
                        gateway_boot_id: Some("boot-lx06".into()),
                        state_revision: Some(3),
                        capability_fingerprint: Some("fp-3".into()),
                        blocks: vec![],
                        ts: 1000,
                    };
                    let _ = wr
                        .write_all(
                            format!("{}\n", serde_json::to_string(&resp).unwrap()).as_bytes(),
                        )
                        .await;
                }
                CoreFrame::ConfigValidate {
                    request_id,
                    device,
                    block,
                    schema_version,
                    block_hash,
                    ..
                } => {
                    let resp = GatewayFrame::ConfigValidated {
                        request_id,
                        device,
                        block,
                        schema_version,
                        block_hash,
                        ok: true,
                        supported: true,
                        blockers: vec![],
                        state: ConfigReceiptStatus::Validated,
                        reason: None,
                        retryable: false,
                        gateway_boot_id: Some("boot-lx06".into()),
                        state_revision: Some(3),
                        capability_fingerprint: Some("fp-3".into()),
                        evidence: None,
                        ts: 1001,
                    };
                    let _ = wr
                        .write_all(
                            format!("{}\n", serde_json::to_string(&resp).unwrap()).as_bytes(),
                        )
                        .await;
                }
                CoreFrame::ConfigApply {
                    device,
                    core_node_id,
                    data_epoch,
                    application_id,
                    operation_id,
                    desired_revision,
                    block_generation,
                    block,
                    schema_version,
                    block_hash,
                    ..
                } => {
                    let persisted = GatewayFrame::ConfigReceipt {
                        device: device.clone(),
                        core_node_id: core_node_id.clone(),
                        data_epoch,
                        application_id: application_id.clone(),
                        operation_id: operation_id.clone(),
                        desired_revision,
                        block_generation,
                        block,
                        schema_version,
                        block_hash: block_hash.clone(),
                        state: ConfigReceiptStatus::Persisted,
                        reason: None,
                        retryable: false,
                        gateway_boot_id: Some("boot-lx06".into()),
                        state_revision: Some(4),
                        persisted_hash: Some(block_hash.clone()),
                        active_hash: None,
                        active_values: None,
                        evidence: None,
                        ts: 1002,
                    };
                    let _ = wr
                        .write_all(
                            format!("{}\n", serde_json::to_string(&persisted).unwrap()).as_bytes(),
                        )
                        .await;
                    let _ = wr.flush().await;
                    persisted_tx.notify_one();

                    if let Some(ref gate) = release_rx {
                        gate.notified().await;
                        let active = GatewayFrame::ConfigReceipt {
                            device,
                            core_node_id,
                            data_epoch,
                            application_id,
                            operation_id,
                            desired_revision,
                            block_generation,
                            block,
                            schema_version,
                            block_hash: block_hash.clone(),
                            state: ConfigReceiptStatus::Active,
                            reason: None,
                            retryable: false,
                            gateway_boot_id: Some("boot-lx06".into()),
                            state_revision: Some(4),
                            persisted_hash: Some(block_hash.clone()),
                            active_hash: Some(block_hash),
                            active_values: None,
                            evidence: None,
                            ts: 1014,
                        };
                        let _ = wr
                            .write_all(
                                format!("{}\n", serde_json::to_string(&active).unwrap()).as_bytes(),
                            )
                            .await;
                        let _ = wr.flush().await;
                    }
                }
                _ => {}
            }
        }
    });

    MockGatewayHandles {
        persisted_sent,
        release_active,
    }
}

#[tokio::test]
async fn dispatch_block_v2_kws_waits_past_5s_for_delayed_active_receipt() {
    let hub = DeviceHub::new();
    let gw = spawn_mock_v2_gateway(
        hub.clone(),
        "lx06-a",
        vec!["config_apply_v2", "config_block_kws"],
        true,
    )
    .await;

    let hub_call = hub.clone();
    let task = tokio::spawn(async move {
        dispatch_block_v2(
            &hub_call,
            "lx06-a",
            "lx06-a",
            1,
            "app-kws-1",
            2,
            21,
            ConfigBlock::Kws,
            json!({
                "keywords": "n ǐ h ǎo g āo d á @你好高达",
                "threshold": 0.20,
                "score": 2.50,
            }),
        )
        .await
    });

    gw.persisted_sent.notified().await;
    // Allow Core to consume `Persisted` and park on the terminal-receipt timeout.
    tokio::time::sleep(Duration::from_millis(30)).await;

    // Simulate a 12s LX06 `hm-kws` cold reload without real wall-clock wait.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(12)).await;
    tokio::time::resume();

    gw.release_active.unwrap().notify_one();
    let outcome = task.await.unwrap();
    assert_eq!(outcome.state, "applied", "unexpected outcome: {outcome:?}");
    assert!(outcome.error.is_none());
    assert_eq!(outcome.receipt["state"], json!("active"));
}

#[tokio::test]
async fn dispatch_block_v2_led_times_out_at_5s_and_kws_times_out_at_25s() {
    // 1) Non-KWS block (Led) times out once 5s elapse without terminal receipt.
    let hub_led = DeviceHub::new();
    let gw_led = spawn_mock_v2_gateway(
        hub_led.clone(),
        "dot2-a",
        vec!["config_apply_v2", "config_block_led"],
        false,
    )
    .await;

    let hub_call = hub_led.clone();
    let led_task = tokio::spawn(async move {
        dispatch_block_v2(
            &hub_call,
            "dot2-a",
            "dot2-a",
            1,
            "app-led-1",
            2,
            21,
            ConfigBlock::Led,
            json!({ "night_enabled": false }),
        )
        .await
    });

    gw_led.persisted_sent.notified().await;
    tokio::time::sleep(Duration::from_millis(30)).await;

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::time::resume();

    let led_outcome = led_task.await.unwrap();
    assert_eq!(led_outcome.state, "failed");
    assert_eq!(
        led_outcome.error.as_deref(),
        Some("GATEWAY_RECEIPT_TIMEOUT")
    );

    // 2) KWS block survives 20s wait, then times out once 25s elapse and preserves persisted_hash.
    let hub_kws = DeviceHub::new();
    let gw_kws = spawn_mock_v2_gateway(
        hub_kws.clone(),
        "lx06-b",
        vec!["config_apply_v2", "config_block_kws"],
        false,
    )
    .await;

    let hub_call = hub_kws.clone();
    let kws_task = tokio::spawn(async move {
        dispatch_block_v2(
            &hub_call,
            "lx06-b",
            "lx06-b",
            1,
            "app-kws-2",
            2,
            21,
            ConfigBlock::Kws,
            json!({
                "keywords": "n ǐ h ǎo g āo d á @你好高达",
                "threshold": 0.20,
                "score": 2.50,
            }),
        )
        .await
    });

    gw_kws.persisted_sent.notified().await;
    tokio::time::sleep(Duration::from_millis(30)).await;

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(20)).await;
    assert!(
        !kws_task.is_finished(),
        "KWS block must remain pending at 20s (Gateway KWS_RELOAD_TIMEOUT_MS)"
    );
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::time::resume();

    let kws_outcome = kws_task.await.unwrap();
    assert_eq!(kws_outcome.state, "failed");
    assert_eq!(
        kws_outcome.error.as_deref(),
        Some("GATEWAY_RECEIPT_TIMEOUT")
    );
    assert!(kws_outcome.receipt["persisted_hash"].is_string());
}
