//! R2-BM6: volume and duplex mode as TOOLS. The proof here is threefold:
//! the tool drives the SAME `VoicePipeline` methods the console endpoints use;
//! the spoken fact is rendered from the DEVICE's receipt; and a request the
//! device cannot satisfy is a FAILED switch, never "已切换".

mod common;

use hm_core::agent::facts::render_fact;
use hm_core::api::AppState;
use hm_core::tool::mocks::MockInject;
use hm_core::tool::{Evidence, ToolCtx, ToolStatus};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

async fn app(duplex: &'static str) -> Arc<AppState> {
    app_custom(duplex, "device_confirmed", true).await
}

async fn app_custom(
    duplex: &'static str,
    volume_stage: &'static str,
    reply_mode: bool,
) -> Arc<AppState> {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        std::path::PathBuf::from("./data-test"),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], port).into());
    common::spawn_fake_device_custom(port, true, duplex, volume_stage, reply_mode).await;
    for _ in 0..100 {
        if state.hub.device_exists("dot2-d") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    state
        .config
        .save_device_meta("dot2-d", "客厅音箱", "客厅", "test")
        .unwrap();
    state
}

fn ctx() -> ToolCtx {
    ToolCtx::new("T1", "C1", Arc::new(AtomicBool::new(false)))
}

fn reported_volume(state: &AppState) -> Option<f64> {
    state.hub.snapshot()["devices"]
        .as_array()?
        .iter()
        .find(|d| d["stable_id"] == "dot2-d")?
        .get("volume")?
        .as_f64()
}

#[tokio::test]
async fn volume_is_spoken_from_the_device_receipt() {
    let state = app("full").await;
    let r = state
        .registry
        .call("system.volume", json!({ "percent": 90 }), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Completed, "{r:?}");
    assert_eq!(r.evidence, Evidence::DeviceConfirmed);
    let fact = r.fact.as_ref().unwrap();
    assert_eq!(fact.action, "set");
    assert_eq!(fact.value.as_deref(), Some("90%"));
    assert_eq!(render_fact(&r, fact), "客厅的音量已调到90%。");
    let v = reported_volume(&state).expect("the fake device echoed a receipt");
    assert!((v - 0.9).abs() < 0.02, "{v}");

    // Same method as POST /devices/{id}/volume: a bad value is rejected by
    // the SAME validation, not a second one.
    let r = state
        .registry
        .call("system.volume", json!({ "percent": 101 }), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Failed);
    assert_eq!(r.error.unwrap().code, "BAD_ARGS");
}

#[tokio::test]
async fn volume_for_an_unknown_device_is_refused_before_any_action() {
    let state = app("full").await;
    let before = reported_volume(&state);
    let r = state
        .registry
        .call(
            "system.volume",
            json!({ "percent": 50, "device": "书房" }),
            &ctx(),
        )
        .await;
    assert_eq!(r.status, ToolStatus::Failed);
    assert!(r.error.unwrap().code == "DEVICE_NOT_FOUND");
    assert_eq!(
        reported_volume(&state),
        before,
        "no volume command was sent"
    );
}

#[tokio::test]
async fn duplex_uses_the_same_validation_and_history_as_the_page() {
    let state = app("full").await;
    let r = state
        .registry
        .call("system.duplex", json!({ "mode": "full" }), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Completed, "{r:?}");
    assert_eq!(r.evidence, Evidence::DeviceConfirmed);
    let fact = r.fact.as_ref().unwrap();
    assert_eq!(fact.action, "switch");
    assert_eq!(fact.value.as_deref(), Some("全双工"));
    assert_eq!(render_fact(&r, fact), "已把客厅的对话模式切换到全双工。");

    // `set_mode` records one row with actor=voice, exactly like the console
    // path (actor=console). That is the shared change record.
    let history = state.config.history(20).unwrap();
    let row = history
        .iter()
        .find(|h| {
            h["scope"] == "voice"
                && h["summary"]
                    .as_str()
                    .unwrap_or("")
                    .contains("duplex=Some(\"full\")")
        })
        .expect("the mode change is in the config history");
    assert_eq!(row["actor"], "voice", "{row}");

    let r = state
        .registry
        .call("system.duplex", json!({ "mode": "sideways" }), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Failed);
    assert_eq!(r.error.unwrap().code, "BAD_ARGS");
}

#[tokio::test]
async fn a_half_duplex_device_cannot_be_switched_to_full() {
    let state = app("half").await;
    let r = state
        .registry
        .call("system.duplex", json!({ "mode": "full" }), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Failed, "{r:?}");
    assert_eq!(r.evidence, Evidence::Accepted);
    let err = r.error.as_ref().unwrap();
    assert_eq!(err.code, "MODE_UNCONFIRMED");
    assert!(err.detail.contains("只支持半双工"), "{err:?}");
    let fact = r.fact.as_ref().unwrap();
    assert_eq!(
        render_fact(&r, fact),
        "没有完成客厅的对话模式的切换：设备只支持半双工。"
    );

    // The request itself was still recorded (the console does the same);
    // only the FACT is denied.
    let history = state.config.history(20).unwrap();
    assert!(history.iter().any(|h| h["scope"] == "voice"));
}

#[tokio::test]
async fn duplex_without_a_connected_device_is_refused() {
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        std::path::PathBuf::from("./data-test"),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let r = state
        .registry
        .call("system.duplex", json!({ "mode": "half" }), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Failed);
    assert_eq!(r.error.unwrap().code, "DEVICE_NOT_FOUND");
    let _: Value = json!({});
}

// --- audit P1: no receipt means no 「已…」 ---------------------------------

#[tokio::test]
async fn a_silent_volume_command_stays_unknown() {
    let state = app_custom("full", "none", true).await;
    let r = state
        .registry
        .call("system.volume", json!({ "percent": 90 }), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Unknown, "{r:?}");
    assert_eq!(r.evidence, Evidence::Unknown);
    assert_eq!(r.error.as_ref().unwrap().code, "VOLUME_UNCONFIRMED");
    let fact = r.fact.as_ref().unwrap();
    assert!(!fact.confirmed);
    assert_eq!(render_fact(&r, fact), "没能确认客厅的音量是否调节。");
    assert_eq!(
        reported_volume(&state),
        None,
        "no confirmed receipt, no snapshot"
    );
}

#[tokio::test]
async fn a_failed_volume_receipt_is_not_upgraded_to_success() {
    let state = app_custom("full", "failed", true).await;
    let r = state
        .registry
        .call("system.volume", json!({ "percent": 90 }), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Unknown, "{r:?}");
    assert_eq!(r.error.as_ref().unwrap().code, "VOLUME_UNCONFIRMED");
    assert_eq!(
        reported_volume(&state),
        None,
        "a failed receipt must not move the snapshot"
    );
    assert_eq!(
        render_fact(&r, r.fact.as_ref().unwrap()),
        "没能确认客厅的音量是否调节。"
    );
}

#[tokio::test]
async fn a_silent_mode_command_stays_unknown_not_a_capability_failure() {
    let state = app_custom("full", "device_confirmed", false).await;
    let r = state
        .registry
        .call("system.duplex", json!({ "mode": "full" }), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Unknown, "{r:?}");
    assert_eq!(r.evidence, Evidence::Unknown);
    assert_eq!(r.error.as_ref().unwrap().code, "MODE_UNCONFIRMED");
    assert_eq!(r.error.as_ref().unwrap().detail, "设备没有回报对话模式");
    assert_eq!(
        render_fact(&r, r.fact.as_ref().unwrap()),
        "没能确认客厅的对话模式是否切换。"
    );
}
