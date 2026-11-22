//! Real music/filler consumers must not confuse a dropped stop waiter with a flush.
use super::*;

async fn start_media(state: &Arc<AppState>, gw: &Arc<FakeGateway>) -> String {
    let search = state
        .registry
        .call(
            "music.search",
            json!({"query":"*","source":"library"}),
            &ToolCtx::new("nb-play", "nb-search", Arc::new(AtomicBool::new(false))),
        )
        .await;
    assert_eq!(search.status, ToolStatus::Completed);
    let track = search.result.unwrap()["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let s = state.clone();
    let play = tokio::spawn(async move {
        s.registry
            .call(
                "music.play",
                json!({"source":"library","id":track}),
                &ToolCtx::new("nb-play", "nb-play", Arc::new(AtomicBool::new(false))),
            )
            .await
    });
    let (id, _) = gw.until_play("media").await;
    // Streaming is an actual progress receipt, not a forged terminal drain.
    gw.send(&GatewayFrame::PlayReceipt {
        task_id: id.clone(),
        stage: "streaming".into(),
        detail: None,
        ts: hm_core::id::now_ms(),
    })
    .await;
    assert_eq!(play.await.unwrap().status, ToolStatus::Completed);
    id
}

#[tokio::test]
async fn northbound_music_stop_pause_disconnect_timeout_cancellation_and_success() {
    for action in ["pause", "stop"] {
        for outcome in [
            "disconnect",
            "timeout",
            "cancel",
            "confirmed",
            "flush_failed",
            "failed",
            "unknown",
            "future_stage",
            "unexpected_detail",
            "not_found",
            "forgotten",
            "play_finished",
        ] {
            let (_base, state, gw, _service) =
                spawn_app_with_gateway_media(FakeLlm::new(vec![])).await;
            let play_id = start_media(&state, &gw).await;
            let cancel = Arc::new(AtomicBool::new(false));
            let flag = cancel.clone();
            let s = state.clone();
            let controlling = tokio::spawn(async move {
                s.registry
                    .call(
                        "music.control",
                        json!({"action":action,"target":"media"}),
                        &ToolCtx::new("nb-control", "nb-control", flag),
                    )
                    .await
            });
            gw.until(|f| matches!(f,CoreFrame::Stop {task_id,..} if task_id==&play_id))
                .await;
            match outcome {
                "disconnect" => gw.close().await,
                "cancel" => {
                    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                    gw.confirm_stop(&play_id).await;
                }
                "confirmed" => gw.confirm_stop(&play_id).await,
                "forgotten" => state.hub.forget_play(&play_id),
                "play_finished" => {
                    gw.send(&GatewayFrame::PlayFinished {
                        device: "dot2-d".into(),
                        task_id: play_id.clone(),
                        played_ms: 120,
                        consumed_ms: None,
                        progress_basis: None,
                        ts: hm_core::id::now_ms(),
                    })
                    .await
                }
                "flush_failed" | "failed" | "unknown" | "future_stage" | "unexpected_detail"
                | "not_found" => {
                    let (stage, detail, played_ms) = match outcome {
                        "flush_failed" => (
                            "unknown",
                            Some("flush_failed: synthetic hardware fault".into()),
                            Some(120),
                        ),
                        "unexpected_detail" => (
                            "device_confirmed",
                            Some("future: unverified flush".into()),
                            Some(120),
                        ),
                        "not_found" => ("device_confirmed", None, None),
                        other => (other, None, Some(120)),
                    };
                    gw.send(&GatewayFrame::StopReceipt {
                        task_id: play_id.clone(),
                        stage: stage.into(),
                        detail,
                        played_ms,
                        consumed_ms: None,
                        progress_basis: None,
                    })
                    .await;
                }
                _ => {}
            }
            let result = tokio::time::timeout(Duration::from_secs(3), controlling)
                .await
                .unwrap()
                .unwrap();
            if outcome != "cancel" {
                let typed =
                    &result.result.as_ref().expect("actual music tool result")["stop_outcome"];
                assert_eq!(
                    typed["outcome"],
                    match outcome {
                        "confirmed" => "confirmed",
                        "flush_failed" | "failed" => "failed",
                        _ => "unknown",
                    },
                    "{action}/{outcome}: {result:?}"
                );
            }
            if outcome == "flush_failed" || outcome == "failed" {
                assert_eq!(result.status, ToolStatus::Failed);
                assert_eq!(result.error.as_ref().unwrap().code, "CONTROL_FAILED");
            }
            if outcome == "confirmed" {
                assert_eq!(
                    result.evidence,
                    hm_core::tool::Evidence::DeviceConfirmed,
                    "{action}"
                );
                assert!(result.fact.as_ref().is_some_and(|f| f.confirmed));
                if action == "pause" {
                    let no_op = state
                        .registry
                        .call(
                            "music.control",
                            json!({"action":"pause","target":"media"}),
                            &ToolCtx::new("no-op", "no-op", Arc::new(AtomicBool::new(false))),
                        )
                        .await;
                    assert_eq!(no_op.status, ToolStatus::Completed);
                    assert_eq!(no_op.evidence, hm_core::tool::Evidence::Observed);
                    assert!(no_op.fact.is_none());
                    assert_eq!(no_op.result.unwrap()["no_op"], true);
                }
            } else {
                assert_ne!(
                    result.evidence,
                    hm_core::tool::Evidence::DeviceConfirmed,
                    "{action}/{outcome}"
                );
                assert!(
                    result
                        .summary()
                        .contains(if matches!(outcome, "flush_failed" | "failed") {
                            "CONTROL_FAILED"
                        } else if outcome == "cancel" {
                            "cancelled"
                        } else {
                            "CONTROL_UNCONFIRMED"
                        }),
                    "existing trace/chat UI must receive truthful summary: {result:?}"
                );
                assert!(
                    !result.fact.as_ref().is_some_and(|f| f.confirmed),
                    "closed/cancelled/timed-out waiter cannot confirm flush"
                );
                assert!(matches!(
                    result.status,
                    ToolStatus::Unknown | ToolStatus::Cancelled | ToolStatus::Failed
                ));
            }
        }
    }
}

#[tokio::test]
async fn northbound_actual_filler_disconnect_records_unknown_and_keeps_bounded_proceed_policy() {
    let llm = FakeLlm::new(vec![Scripted::Delayed {
        delay_ms: 700,
        chunks: vec!["已经关好了。".into()],
    }]);
    let (_base, state, gw) = spawn_app_with_gateway(llm).await;
    set_filler(&state, one_cue(200, "我想想。")).await;
    explicit_request(&state, &gw, 1, "今天有什么新鲜事").await;
    let cue=gw.until(|f|matches!(f,CoreFrame::Play {content_ref,..}|CoreFrame::PlayBegin {content_ref,..} if content_ref.starts_with("filler:"))).await;
    let id = match cue {
        CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } => task_id,
        _ => unreachable!(),
    };
    gw.until(|f| matches!(f,CoreFrame::Stop {task_id,..} if task_id==&id))
        .await;
    gw.close().await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entries = voice_log_has(&state, "filler_stop_settled");
            if !entries.is_empty() {
                assert_eq!(entries[0]["settled"], false);
                assert_eq!(entries[0]["detail"], "connection_retired");
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("closed filler waiter must settle as unknown without waiting the full bound");
}

/// Actual filler and spoken-output consumers share Hub's typed stop fact;
/// the existing management snapshot must retain uncertainty for the UI.
#[tokio::test]
async fn typed_stop_filler_error_outcomes_reach_management_without_false_audio_end() {
    for (stage, detail, expected) in [
        ("unknown", Some("flush_failed: fixture"), "failed"),
        ("unknown", None, "unknown"),
        ("future_stage", None, "unknown"),
        ("device_confirmed", Some("future_detail"), "unknown"),
    ] {
        let llm = FakeLlm::new(vec![Scripted::Delayed {
            delay_ms: 700,
            chunks: vec!["已经关好了。".into()],
        }]);
        let (base, state, gw) = spawn_app_with_gateway(llm).await;
        set_filler(&state, one_cue(200, "我想想。")).await;
        explicit_request(&state, &gw, 1, "今天有什么新鲜事").await;
        let cue = gw.until(|f|matches!(f,CoreFrame::Play {content_ref,..}|CoreFrame::PlayBegin {content_ref,..} if content_ref.starts_with("filler:"))).await;
        let id = match cue {
            CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } => task_id,
            _ => unreachable!(),
        };
        gw.until(|f| matches!(f,CoreFrame::Stop {task_id,..} if task_id==&id))
            .await;
        gw.send(&GatewayFrame::StopReceipt {
            task_id: id.clone(),
            stage: stage.into(),
            detail: detail.map(str::to_string),
            played_ms: Some(0),
            consumed_ms: None,
            progress_basis: None,
        })
        .await;
        // The bounded proceed policy still produces the actual answer.
        gw.until_play("voice").await;
        let snapshot: Value = reqwest::Client::new()
            .get(format!("{base}/api/v0/devices"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let settled = snapshot["voice_log"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["kind"] == "filler_stop_settled")
            .expect("actual filler completion in management API");
        assert_eq!(settled["settled"], false, "{snapshot}");
        assert_eq!(settled["stop_outcome"]["outcome"], expected);
        let typed = snapshot["recent"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["kind"] == "stop_outcome" && r["play_id"] == id)
            .expect("typed stop fact in gateway API");
        assert_eq!(typed["outcome"]["outcome"], expected);
        let mode: Value = reqwest::Client::new()
            .get(format!("{base}/api/v0/mode"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let device = mode["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["device"] == "dot2-d")
            .unwrap();
        let outputs = device["outputs"].as_array().unwrap();
        assert!(
            outputs
                .iter()
                .any(|r| r["play_id"] == id && r["stage"] == "unknown"),
            "OutputsCard must receive unknown, not disappear as a confirmed flush: {mode}"
        );
    }
}
