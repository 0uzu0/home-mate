//! Unit and loopback-TCP tests for wire-2 negotiation, config_apply_v2
//! frames, capability gating, PlayRejected, and waiter retirement on link drop.

use super::*;
use crate::gateway::frames::{ConfigBlock, ConfigReceiptStatus};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[test]
fn wire2_negotiation_respects_supported_versions_and_never_invents_v1() {
    let mine = (0, 2);
    let my_sup = &[0, 2];

    assert_eq!(
        protocol_operating_version_with_supported(mine, my_sup, (0, 2), &[0, 2]),
        Some(2)
    );
    assert_eq!(
        protocol_operating_version_with_supported(mine, my_sup, (0, 0), &[]),
        Some(0)
    );
    assert_eq!(
        protocol_operating_version_with_supported(mine, my_sup, (0, 1), &[]),
        Some(0)
    );
    assert_eq!(
        protocol_operating_version_with_supported(mine, my_sup, (0, 1), &[0, 1]),
        Some(0)
    );
    assert_eq!(
        protocol_operating_version_with_supported(mine, my_sup, (1, 1), &[]),
        None
    );
    assert_eq!(
        protocol_operating_version_with_supported(mine, my_sup, (1, 1), &[1]),
        None
    );
    assert_eq!(
        protocol_operating_version_with_supported(mine, my_sup, (0, 2), &[1]),
        None
    );
}

#[test]
fn config_v2_and_keyword_frames_round_trip() {
    let kw = GatewayFrame::Keyword {
        device: "dot2-d".into(),
        origin: InputOrigin::Legacy,
        ts: 100,
        seq: 1,
        keyword: "你好小智".into(),
        fed_ms: Some(320),
        self_heard: false,
        rms_dbfs: Some(-28.0),
        lag_ms: Some(12),
        bundle_id: Some("bundle-1".into()),
        kws_generation: Some(4),
        kws_tier: None,
        wake_dur_ms: None,
        max_gap_ms: None,
        l2_passed: None,
    };
    let s = serde_json::to_string(&kw).unwrap();
    let decoded: GatewayFrame = serde_json::from_str(&s).unwrap();
    assert_eq!(decoded, kw);

    let rej = GatewayFrame::PlayRejected {
        task_id: "P1".into(),
        device: "dot2-d".into(),
        reason: "busy".into(),
        ts: 200,
    };
    let s = serde_json::to_string(&rej).unwrap();
    let decoded: GatewayFrame = serde_json::from_str(&s).unwrap();
    assert_eq!(decoded, rej);

    let apply = CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "dot2-d".into(),
        data_epoch: 1,
        application_id: "app-1".into(),
        operation_id: "op-1".into(),
        desired_revision: 3,
        block_generation: 1,
        block: ConfigBlock::AudioEq,
        schema_version: 1,
        block_hash: "abc".into(),
        expected_gateway_boot_id: Some("boot-1".into()),
        expected_state_revision: Some(5),
        capability_fingerprint: Some("fp-1".into()),
        values: json!({"curve":"echomuse","bands":[]}),
    };
    let s = serde_json::to_string(&apply).unwrap();
    let decoded: CoreFrame = serde_json::from_str(&s).unwrap();
    assert_eq!(decoded, apply);
}

#[tokio::test]
async fn wire2_handshake_config_waiters_and_link_drop_retirement() {
    let hub = DeviceHub::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hub_c = hub.clone();
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let _ = hub_c.serve_connection(stream).await;
        }
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let hello = GatewayFrame::Hello {
        protocol: 2,
        min_protocol: 0,
        supported_versions: vec![0, 2],
        gateway: "test-gw".into(),
        gateway_version: "0.2.0".into(),
        devices: vec![HelloDevice {
            stable_id: "dot2-d".into(),
            model: "AEOBC".into(),
            firmware: None,
            capabilities: vec![
                "audio_out".into(),
                "config_apply_v2".into(),
                "config_block_audio_eq".into(),
            ],
            audio_profile: "p".into(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
    };
    wr.write_all(serde_json::to_string(&hello).unwrap().as_bytes())
        .await
        .unwrap();
    wr.write_all(b"\n").await.unwrap();

    let ack_line = lines.next_line().await.unwrap().unwrap();
    let ack: CoreFrame = serde_json::from_str(&ack_line).unwrap();
    match ack {
        CoreFrame::HelloAck {
            protocol,
            supported_versions,
            capabilities,
            ..
        } => {
            assert_eq!(protocol, 2);
            assert_eq!(supported_versions, vec![0, 2]);
            assert!(capabilities.contains(&"config_apply_v2".to_string()));
        }
        other => panic!("expected HelloAck, got {other:?}"),
    }
    // Drain TimeSet
    let _ = lines.next_line().await.unwrap().unwrap();

    // Wait for hub to register device
    for _ in 0..50 {
        if hub.device_exists("dot2-d") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Capability gating: AudioEq allowed, Led rejected (missing config_block_led)
    assert!(hub
        .supports_config_apply_v2("dot2-d", ConfigBlock::AudioEq)
        .is_ok());
    assert!(hub
        .supports_config_apply_v2("dot2-d", ConfigBlock::Led)
        .is_err());

    // Frame size cap check
    let oversized = json!({ "curve": "echomuse", "pad": "x".repeat(MAX_CONFIG_FRAME_BYTES) });
    let err = hub
        .config_validate(
            ConfigValidateRequest {
                device: "dot2-d".into(),
                expected_gateway_boot_id: None,
                expected_state_revision: None,
                block: ConfigBlock::AudioEq,
                schema_version: 1,
                block_hash: "h".into(),
                values: oversized,
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
    assert!(err.contains("MAX_CONFIG_FRAME_BYTES"), "{err}");

    // ConfigStateGet -> ConfigState
    let hub_get = hub.clone();
    let get_fut = tokio::spawn(async move {
        hub_get
            .config_state_get("dot2-d", Duration::from_secs(2))
            .await
    });
    let req_line = lines.next_line().await.unwrap().unwrap();
    let CoreFrame::ConfigStateGet { request_id, device } = serde_json::from_str(&req_line).unwrap()
    else {
        panic!("expected ConfigStateGet");
    };
    assert_eq!(device, "dot2-d");
    let state_resp = GatewayFrame::ConfigState {
        request_id: Some(request_id),
        device: "dot2-d".into(),
        gateway_boot_id: Some("boot-xyz".into()),
        state_revision: Some(7),
        capability_fingerprint: Some("fp-7".into()),
        blocks: vec![],
        ts: 1000,
    };
    wr.write_all(serde_json::to_string(&state_resp).unwrap().as_bytes())
        .await
        .unwrap();
    wr.write_all(b"\n").await.unwrap();
    assert!(matches!(
        get_fut.await.unwrap(),
        Ok(GatewayFrame::ConfigState {
            state_revision: Some(7),
            ..
        })
    ));
    assert_eq!(
        hub.device_config_fencing("dot2-d"),
        Some((Some("boot-xyz".into()), Some(7), Some("fp-7".into())))
    );

    // ConfigApply -> Persisted then Active
    let mut rx = hub
        .config_apply(ConfigApplyRequest {
            device: "dot2-d".into(),
            core_node_id: "dot2-d".into(),
            data_epoch: 1,
            application_id: "app-1".into(),
            operation_id: "op-1".into(),
            desired_revision: 2,
            block_generation: 1,
            block: ConfigBlock::AudioEq,
            schema_version: 1,
            block_hash: "hash-1".into(),
            expected_gateway_boot_id: Some("boot-xyz".into()),
            expected_state_revision: Some(7),
            capability_fingerprint: Some("fp-7".into()),
            values: json!({"curve": "echomuse", "bands": []}),
        })
        .unwrap();
    let apply_line = lines.next_line().await.unwrap().unwrap();
    assert!(matches!(
        serde_json::from_str::<CoreFrame>(&apply_line).unwrap(),
        CoreFrame::ConfigApply { .. }
    ));
    for st in [ConfigReceiptStatus::Persisted, ConfigReceiptStatus::Active] {
        let rcpt = GatewayFrame::ConfigReceipt {
            device: "dot2-d".into(),
            core_node_id: "dot2-d".into(),
            data_epoch: 1,
            application_id: "app-1".into(),
            operation_id: "op-1".into(),
            desired_revision: 2,
            block_generation: 1,
            block: ConfigBlock::AudioEq,
            schema_version: 1,
            block_hash: "hash-1".into(),
            state: st,
            reason: None,
            retryable: false,
            gateway_boot_id: Some("boot-xyz".into()),
            state_revision: Some(8),
            persisted_hash: Some("ph".into()),
            active_hash: (st == ConfigReceiptStatus::Active).then(|| "ah".into()),
            active_values: None,
            evidence: None,
            ts: 1001,
        };
        wr.write_all(serde_json::to_string(&rcpt).unwrap().as_bytes())
            .await
            .unwrap();
        wr.write_all(b"\n").await.unwrap();
    }
    let r1 = rx.recv().await.unwrap();
    let r2 = rx.recv().await.unwrap();
    assert!(matches!(
        r1,
        GatewayFrame::ConfigReceipt {
            state: ConfigReceiptStatus::Persisted,
            ..
        }
    ));
    assert!(matches!(
        r2,
        GatewayFrame::ConfigReceipt {
            state: ConfigReceiptStatus::Active,
            ..
        }
    ));

    // Link drop retires pending config_state_get waiter immediately
    let hub_drop = hub.clone();
    let pending_get = tokio::spawn(async move {
        hub_drop
            .config_state_get("dot2-d", Duration::from_secs(30))
            .await
    });
    let _ = lines.next_line().await.unwrap().unwrap();
    drop(wr);
    drop(lines);
    let drop_err = pending_get.await.unwrap().unwrap_err();
    assert!(drop_err.contains("retired"), "{drop_err}");
}

#[tokio::test]
async fn legacy_wire0_hello_receives_hello_ack_with_protocol_zero() {
    let hub = DeviceHub::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hub_c = hub.clone();
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let _ = hub_c.serve_connection(stream).await;
        }
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let hello = GatewayFrame::Hello {
        protocol: 0,
        min_protocol: 0,
        supported_versions: vec![],
        gateway: "legacy-gw".into(),
        gateway_version: "0.0.53".into(),
        devices: vec![HelloDevice {
            stable_id: "dot2-d".into(),
            model: "AEOBC".into(),
            firmware: None,
            capabilities: vec!["audio_out".into()],
            audio_profile: "p".into(),
            wifi_ssid: None,
        }],
        aec_tail_ms: None,
        profile_version: None,
    };
    wr.write_all(serde_json::to_string(&hello).unwrap().as_bytes())
        .await
        .unwrap();
    wr.write_all(b"\n").await.unwrap();

    let ack_line = lines.next_line().await.unwrap().unwrap();
    let ack: CoreFrame = serde_json::from_str(&ack_line).unwrap();
    match ack {
        CoreFrame::HelloAck {
            protocol,
            supported_versions,
            capabilities,
            ..
        } => {
            assert_eq!(protocol, 0);
            assert_eq!(supported_versions, vec![0]);
            assert!(capabilities.is_empty());
        }
        other => panic!("expected HelloAck, got {other:?}"),
    }
}
