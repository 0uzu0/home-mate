//! Wire-compat gate for `config_apply_v2` (Gateway -> Core frames and full
//! `DeviceHub` + `ConfigApplyCoordinator` loopback TCP execution).
//!
//! `fixtures/gateway_config_v2_wire.jsonl` mirrors the REAL
//! `home_mate_device_gateway::protocol::GatewayFrame` wire output at gateway
//! `dc5cb0c` (`state:"idle"` for never-applied blocks, `supported`/`blockers`
//! + `capability_fingerprint` without `ok`/`state`, `status`/`error_code`
//! without `state`/`reason`). Both unit deserialization and real TCP transport
//! tests below feed raw Gateway-shaped JSON strings rather than serializing
//! Core's `GatewayFrame` types.

use hm_core::api::AppState;
use hm_core::config_delivery::{
    Block, Commit, Compatibility, ConfigApplyCoordinator, ConfigDelivery, Operation, Package,
    Purpose, Source, Target,
};
use hm_core::gateway::{ConfigBlock, ConfigReceiptStatus, CoreFrame, GatewayFrame};
use hm_core::store::Store;
use hm_core::tool::mocks::MockInject;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn frames() -> Vec<GatewayFrame> {
    include_str!("fixtures/gateway_config_v2_wire.jsonl")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str::<GatewayFrame>(l)
                .unwrap_or_else(|e| panic!("core cannot decode gateway frame: {e}\n{l}"))
        })
        .collect()
}

#[test]
fn core_decodes_every_real_gateway_config_frame() {
    let f = frames();
    assert_eq!(f.len(), 7);

    match &f[0] {
        GatewayFrame::ConfigState {
            blocks,
            gateway_boot_id,
            state_revision,
            ..
        } => {
            assert_eq!(gateway_boot_id.as_deref(), Some("boot-abc"));
            assert_eq!(*state_revision, Some(3));
            assert_eq!(blocks.len(), 2);
            assert_eq!(blocks[0].block, ConfigBlock::Kws);
            assert_eq!(blocks[0].state, ConfigReceiptStatus::Unknown);
            assert!(!blocks[0].state.is_terminal());
            assert_eq!(blocks[1].state, ConfigReceiptStatus::Active);
            assert!(blocks[1].state.is_terminal());
        }
        other => panic!("expected ConfigState, got {other:?}"),
    }

    match &f[1] {
        GatewayFrame::ConfigValidated {
            supported,
            blockers,
            capability_fingerprint,
            ..
        } => {
            assert!(*supported);
            assert!(blockers.is_empty());
            assert_eq!(capability_fingerprint.as_deref(), Some("fp-gw-real-1"));
        }
        other => panic!("expected ConfigValidated, got {other:?}"),
    }

    match &f[2] {
        GatewayFrame::ConfigValidated {
            supported,
            blockers,
            ..
        } => {
            assert!(!*supported);
            assert_eq!(blockers, &vec!["unsupported timezone 'X'".to_string()]);
        }
        other => panic!("expected ConfigValidated, got {other:?}"),
    }

    match &f[3] {
        GatewayFrame::ConfigValidated {
            supported,
            blockers,
            ..
        } => {
            assert!(*supported);
            assert_eq!(
                blockers,
                &vec!["line 1: expected '<tokens> @<text>'".to_string()]
            );
        }
        other => panic!("expected ConfigValidated, got {other:?}"),
    }

    match &f[4] {
        GatewayFrame::ConfigReceipt {
            state,
            persisted_hash,
            ..
        } => {
            assert_eq!(*state, ConfigReceiptStatus::Persisted);
            assert!(!state.is_terminal());
            assert_eq!(persisted_hash.as_deref(), Some("ph1"));
        }
        other => panic!("expected ConfigReceipt(Persisted), got {other:?}"),
    }

    match &f[5] {
        GatewayFrame::ConfigReceipt {
            state,
            operation_id,
            active_hash,
            ..
        } => {
            assert_eq!(*state, ConfigReceiptStatus::Active);
            assert!(state.is_terminal());
            assert_eq!(operation_id, "op-1");
            assert_eq!(active_hash.as_deref(), Some("ah1"));
        }
        other => panic!("expected ConfigReceipt(Active), got {other:?}"),
    }

    match &f[6] {
        GatewayFrame::ConfigReceipt {
            state,
            reason,
            retryable,
            ..
        } => {
            assert_eq!(*state, ConfigReceiptStatus::Failed);
            assert!(state.is_terminal());
            assert_eq!(reason.as_deref(), Some("persistence_failed"));
            assert!(*retryable);
        }
        other => panic!("expected ConfigReceipt(Failed), got {other:?}"),
    }
}

fn setup_enrolled_app() -> (TempDir, Arc<AppState>, String) {
    let dir = std::env::temp_dir().join(format!("hm_wire_compat_{}", hm_core::id::next("T")));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(&dir.join("hm.db")).unwrap());
    let state = AppState::with_overrides(
        store,
        dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let delivery = ConfigDelivery::for_app(&state);
    let enrolled = delivery.enroll("dot2-d").unwrap();
    let hh = enrolled["household_id"].as_str().unwrap().to_string();
    (TempDir(dir), state, hh)
}

async fn connect_raw_wire_gateway(
    state: &Arc<AppState>,
) -> (
    tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    tokio::net::tcp::OwnedWriteHalf,
) {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let hub = state.hub.clone();
    tokio::spawn(async move {
        let _ = hub.listen(addr).await;
    });

    let stream = loop {
        match TcpStream::connect(addr).await {
            Ok(s) => break s,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    };
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let hello = json!({
        "type": "hello",
        "protocol": 2,
        "min_protocol": 0,
        "supported_versions": [0, 2],
        "gateway": "home-mate-device-gateway",
        "gateway_version": "0.0.53",
        "devices": [{
            "stable_id": "dot2-d",
            "model": "AEOBC",
            "capabilities": [
                "audio_out",
                "config_apply_v2",
                "config_block_kws",
                "config_block_led"
            ],
            "audio_profile": "fireos_builtin_speaker"
        }]
    });
    send_raw_json(&mut wr, &hello).await;
    let _ack = lines.next_line().await.unwrap().unwrap();
    let _time = lines.next_line().await.unwrap().unwrap();
    for _ in 0..50 {
        if state.hub.device_exists("dot2-d") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (lines, wr)
}

async fn send_raw_json(wr: &mut tokio::net::tcp::OwnedWriteHalf, v: &Value) {
    wr.write_all(v.to_string().as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();
}

async fn next_config_frame(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
) -> CoreFrame {
    while let Some(line) = lines.next_line().await.unwrap() {
        let frame: CoreFrame = serde_json::from_str(&line).unwrap();
        if matches!(
            frame,
            CoreFrame::ConfigStateGet { .. }
                | CoreFrame::ConfigValidate { .. }
                | CoreFrame::ConfigApply { .. }
        ) {
            return frame;
        }
    }
    panic!("gateway socket closed before expected config frame");
}

fn submit_led_pkg(state: &Arc<AppState>, hh: &str, brightness: u64, app_id: &str) {
    let delivery = ConfigDelivery::for_app(state);
    let cur = delivery.state("admin").unwrap();
    let rev = cur["config_revision"].as_u64().unwrap();
    let pkg = Package {
        format: "home-mate-config".into(),
        version: 2,
        package_id: format!("00000000-0000-4000-8000-{:012x}", brightness + rev * 100),
        package_revision: 1,
        created_ms: 1,
        purpose: Purpose::NodeConfig,
        source: Source {
            node_id: "dot2-d".into(),
            config_revision: rev,
            core_version: "0.1.0".into(),
            schema_version: 2,
        },
        target: Target {
            household_id: hh.into(),
            node_id: Some("dot2-d".into()),
        },
        compatibility: Compatibility {
            core_api_schema: 2,
            gateway_protocol_min: 0,
            gateway_protocol_max: 2,
            required_capabilities: vec!["config_apply_v2".into(), "config_block_led".into()],
        },
        change_reason: "wire compat integration test".into(),
        blocks: vec![Block {
            name: "device_led_configs".into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: "device_led_configs.dot2-d.night_brightness_pct".into(),
                value: json!(brightness),
            }],
        }],
        bindings: vec![],
        content_hash: None,
    };
    let pre = delivery.preflight("admin", pkg, json!({})).unwrap();
    assert_eq!(pre["blocked"], json!([]), "preflight blocked: {pre}");
    delivery
        .commit(
            "admin",
            &Commit {
                plan_id: pre["plan_id"].as_str().unwrap().into(),
                plan_hash: pre["plan_hash"].as_str().unwrap().into(),
                expected_revision: pre["expected_revision"].as_u64().unwrap(),
                application_id: app_id.into(),
            },
        )
        .unwrap();
}

async fn wait_for_terminal_block(delivery: &ConfigDelivery, app_id: &str) -> Value {
    for _ in 0..80 {
        let rb = delivery.readback("admin", app_id).unwrap();
        if rb["publication"]["state"] != "saved_pending" {
            return rb;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for block completion on {app_id}");
}

#[tokio::test]
async fn coordinator_applies_over_real_gateway_wire_with_idle_persisted_unknown_and_active() {
    let (_dir, state, hh) = setup_enrolled_app();
    let (mut lines, mut wr) = connect_raw_wire_gateway(&state).await;
    let _worker = ConfigApplyCoordinator::start_for_app(&state);
    let app_id = "22222222-2222-4222-8222-222222222221";
    submit_led_pkg(&state, &hh, 15, app_id);

    let gw_task = tokio::spawn(async move {
        // 1. Core sends ConfigStateGet; Gateway answers with `"state":"idle"` blocks
        //    and NO capability_fingerprint (real Gateway puts it on ConfigValidated).
        let f1 = next_config_frame(&mut lines).await;
        let req_id = match f1 {
            CoreFrame::ConfigStateGet { request_id, .. } => request_id,
            other => panic!("expected ConfigStateGet, got {other:?}"),
        };
        send_raw_json(
            &mut wr,
            &json!({
                "type": "config_state",
                "request_id": req_id,
                "device": "dot2-d",
                "gateway_boot_id": "boot-real-72",
                "state_revision": 5,
                "blocks": [
                    { "block": "led", "block_generation": 0, "operation_id": "", "data_epoch": 0, "state": "idle" },
                    { "block": "kws", "block_generation": 0, "operation_id": "", "data_epoch": 0, "state": "idle" }
                ]
            }),
        )
        .await;

        // 2. Core sends ConfigValidate; Gateway answers with `supported: true, blockers: []`
        //    and `capability_fingerprint` (NO `ok` or `state`).
        let f2 = next_config_frame(&mut lines).await;
        let (val_id, block_hash) = match f2 {
            CoreFrame::ConfigValidate {
                request_id,
                block_hash,
                expected_gateway_boot_id,
                expected_state_revision,
                ..
            } => {
                assert_eq!(expected_gateway_boot_id.as_deref(), Some("boot-real-72"));
                assert_eq!(expected_state_revision, Some(5));
                (request_id, block_hash)
            }
            other => panic!("expected ConfigValidate, got {other:?}"),
        };
        send_raw_json(
            &mut wr,
            &json!({
                "type": "config_validated",
                "request_id": val_id,
                "device": "dot2-d",
                "gateway_boot_id": "boot-real-72",
                "state_revision": 5,
                "capability_fingerprint": "fp-from-validated-v2",
                "block": "led",
                "block_hash": block_hash,
                "supported": true,
                "blockers": []
            }),
        )
        .await;

        // 3. Core sends ConfigApply carrying the fingerprint captured from ConfigValidated.
        let f3 = next_config_frame(&mut lines).await;
        let (op_id, app_id_wire, cap_fp_wire) = match f3 {
            CoreFrame::ConfigApply {
                operation_id,
                application_id,
                capability_fingerprint,
                ..
            } => (operation_id, application_id, capability_fingerprint),
            other => panic!("expected ConfigApply, got {other:?}"),
        };
        assert_eq!(cap_fp_wire.as_deref(), Some("fp-from-validated-v2"));

        // Send non-terminal `persisted`, then an unknown non-terminal status `"applying"`,
        // then terminal `"active"` via Gateway's `status` field.
        send_raw_json(
            &mut wr,
            &json!({
                "type": "config_receipt",
                "device": "dot2-d",
                "gateway_boot_id": "boot-real-72",
                "operation_id": op_id,
                "application_id": app_id_wire,
                "block": "led",
                "state_revision": 5,
                "status": "persisted",
                "persisted_hash": "ph-led-15",
                "retryable": false
            }),
        )
        .await;
        send_raw_json(
            &mut wr,
            &json!({
                "type": "config_receipt",
                "device": "dot2-d",
                "gateway_boot_id": "boot-real-72",
                "operation_id": op_id,
                "application_id": app_id_wire,
                "block": "led",
                "state_revision": 5,
                "status": "applying",
                "retryable": false
            }),
        )
        .await;
        send_raw_json(
            &mut wr,
            &json!({
                "type": "config_receipt",
                "device": "dot2-d",
                "gateway_boot_id": "boot-real-72",
                "operation_id": op_id,
                "application_id": app_id_wire,
                "block": "led",
                "state_revision": 6,
                "status": "active",
                "persisted_hash": "ph-led-15",
                "active_hash": "ah-led-15",
                "active_values": { "night_brightness_pct": 15 },
                "retryable": false
            }),
        )
        .await;
        wr
    });

    let delivery = ConfigDelivery::for_app(&state);
    let _app = wait_for_terminal_block(&delivery, app_id).await;
    let _wr = gw_task.await.unwrap();

    let rb = delivery.readback("admin", app_id).unwrap();
    assert_eq!(
        rb["publication"]["state"],
        json!("applied"),
        "readback: {rb}"
    );
    assert_eq!(
        rb["publication"]["gateway_receipt"][0]["persisted_hash"],
        json!("ph-led-15")
    );
    assert_eq!(
        rb["publication"]["gateway_receipt"][0]["active_hash"],
        json!("ah-led-15")
    );

    // Hub snapshot must retain the captured `capability_fingerprint` on the device slot,
    // and hub recent events must record `ok: true` for valid Gateway `ConfigValidated`.
    let status = state.hub.snapshot();
    assert_eq!(
        status["devices"][0]["capability_fingerprint"],
        json!("fp-from-validated-v2")
    );
    let recent = state.hub.recent(20);
    let val_ev = recent
        .iter()
        .rfind(|e| e["kind"] == "config_validated")
        .expect("config_validated event recorded");
    assert_eq!(val_ev["ok"], json!(true), "event: {val_ev}");
}

#[tokio::test]
async fn coordinator_rejects_supported_true_with_non_empty_blockers_and_never_sends_apply() {
    let (_dir, state, hh) = setup_enrolled_app();
    let (mut lines, mut wr) = connect_raw_wire_gateway(&state).await;
    let _worker = ConfigApplyCoordinator::start_for_app(&state);
    let app_id = "22222222-2222-4222-8222-222222222222";
    submit_led_pkg(&state, &hh, 25, app_id);

    let gw_task = tokio::spawn(async move {
        let f1 = next_config_frame(&mut lines).await;
        let req_id = match f1 {
            CoreFrame::ConfigStateGet { request_id, .. } => request_id,
            other => panic!("expected ConfigStateGet, got {other:?}"),
        };
        send_raw_json(
            &mut wr,
            &json!({
                "type": "config_state",
                "request_id": req_id,
                "device": "dot2-d",
                "gateway_boot_id": "boot-real-72",
                "state_revision": 7,
                "blocks": [{ "block": "led", "state": "idle" }]
            }),
        )
        .await;

        let f2 = next_config_frame(&mut lines).await;
        let val_id = match f2 {
            CoreFrame::ConfigValidate { request_id, .. } => request_id,
            other => panic!("expected ConfigValidate, got {other:?}"),
        };
        // Exact real-Gateway rejection shape when block is supported (`supported: true`)
        // but payload fails validation (`blockers` non-empty, `ok`/`state` absent).
        send_raw_json(
            &mut wr,
            &json!({
                "type": "config_validated",
                "request_id": val_id,
                "device": "dot2-d",
                "gateway_boot_id": "boot-real-72",
                "state_revision": 7,
                "block": "led",
                "supported": true,
                "blockers": ["night_brightness_pct out of range"]
            }),
        )
        .await;

        // Ensure Core does NOT send ConfigApply after blocker rejection.
        let extra =
            tokio::time::timeout(Duration::from_millis(120), next_config_frame(&mut lines)).await;
        assert!(
            extra.is_err(),
            "Core must not send ConfigApply when blockers are non-empty: {extra:?}"
        );
        wr
    });

    let delivery = ConfigDelivery::for_app(&state);
    let _app = wait_for_terminal_block(&delivery, app_id).await;
    let _wr = gw_task.await.unwrap();

    let rb = delivery.readback("admin", app_id).unwrap();
    assert_eq!(
        rb["publication"]["state"],
        json!("failed"),
        "readback: {rb}"
    );
    assert_eq!(
        rb["publication"]["error"],
        json!("GATEWAY_BLOCKED: night_brightness_pct out of range")
    );
    assert_eq!(
        rb["publication"]["gateway_receipt"][0]["state"],
        json!("failed")
    );
}

#[tokio::test]
async fn coordinator_propagates_gateway_error_code_on_terminal_failed_receipt() {
    let (_dir, state, hh) = setup_enrolled_app();
    let (mut lines, mut wr) = connect_raw_wire_gateway(&state).await;
    let _worker = ConfigApplyCoordinator::start_for_app(&state);
    let app_id = "22222222-2222-4222-8222-222222222223";
    submit_led_pkg(&state, &hh, 35, app_id);

    let gw_task = tokio::spawn(async move {
        let f1 = next_config_frame(&mut lines).await;
        let req_id = match f1 {
            CoreFrame::ConfigStateGet { request_id, .. } => request_id,
            other => panic!("expected ConfigStateGet, got {other:?}"),
        };
        send_raw_json(
            &mut wr,
            &json!({
                "type": "config_state",
                "request_id": req_id,
                "device": "dot2-d",
                "gateway_boot_id": "boot-real-72",
                "state_revision": 8,
                "blocks": [{ "block": "led", "state": "idle" }]
            }),
        )
        .await;

        let f2 = next_config_frame(&mut lines).await;
        let val_id = match f2 {
            CoreFrame::ConfigValidate { request_id, .. } => request_id,
            other => panic!("expected ConfigValidate, got {other:?}"),
        };
        send_raw_json(
            &mut wr,
            &json!({
                "type": "config_validated",
                "request_id": val_id,
                "device": "dot2-d",
                "gateway_boot_id": "boot-real-72",
                "state_revision": 8,
                "block": "led",
                "supported": true,
                "blockers": []
            }),
        )
        .await;

        let f3 = next_config_frame(&mut lines).await;
        let (op_id, app_id_wire) = match f3 {
            CoreFrame::ConfigApply {
                operation_id,
                application_id,
                ..
            } => (operation_id, application_id),
            other => panic!("expected ConfigApply, got {other:?}"),
        };
        send_raw_json(
            &mut wr,
            &json!({
                "type": "config_receipt",
                "device": "dot2-d",
                "gateway_boot_id": "boot-real-72",
                "operation_id": op_id,
                "application_id": app_id_wire,
                "block": "led",
                "state_revision": 8,
                "status": "failed",
                "error_code": "persistence_failed",
                "retryable": false
            }),
        )
        .await;
        wr
    });

    let delivery = ConfigDelivery::for_app(&state);
    let _app = wait_for_terminal_block(&delivery, app_id).await;
    let _wr = gw_task.await.unwrap();

    let rb = delivery.readback("admin", app_id).unwrap();
    assert_eq!(
        rb["publication"]["state"],
        json!("failed"),
        "readback: {rb}"
    );
    assert_eq!(rb["publication"]["error"], json!("persistence_failed"));
}
