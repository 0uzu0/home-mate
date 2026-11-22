use super::{
    consume_stream, empty_say, extract_next_step, is_cancel_utterance, is_continue_utterance,
    is_empty_result, truncate_at_char_boundary,
};
use crate::tool::{ToolResult, ToolStatus};
use futures::StreamExt;

fn ok(payload: serde_json::Value) -> ToolResult {
    ToolResult::observed_ok(payload)
}

/// M7 residual (2026-11-13): an empty result must not count as a
/// successful delivery shape. `music.search`'s no_matches is the live
/// case: it is `observed_ok`, so the old bookkeeping saw "success" and
/// let the model read a six-sentence explanation.
#[test]
fn an_empty_result_is_not_a_success_for_the_delivery_shape() {
    let no_match = ok(serde_json::json!({
        "status": "no_matches", "tracks": [], "rendered_answer": "没找到这首歌。"
    }));
    assert!(is_empty_result(&no_match));
    assert_eq!(empty_say(&no_match).as_deref(), Some("没找到这首歌。"));
    let hit = ok(serde_json::json!({
        "status": "available", "tracks": [{"id": "t01"}]
    }));
    assert!(!is_empty_result(&hit));
    assert!(is_empty_result(&ok(serde_json::json!({ "items": [] }))));
    assert!(!is_empty_result(&ok(serde_json::json!({ "items": [1] }))));
    assert!(!is_empty_result(&ok(serde_json::json!({ "volume": 0.5 }))));
    let mut action = ok(serde_json::json!({ "status": "no_matches", "tracks": [] }));
    action.fact = Some(crate::tool::ToolFact {
        subject: "客厅的灯".into(),
        action: "off".into(),
        basis: crate::tool::FactBasis::DeviceReceipt,
        confirmed: true,
        detail: None,
        value: None,
    });
    assert!(!is_empty_result(&action));
    let failed = ToolResult::failed("SEARCH_OFFLINE", "网络不通", false);
    assert!(!is_empty_result(&failed));
    let bare = ok(serde_json::json!({ "status": "empty" }));
    assert_eq!(empty_say(&bare).as_deref(), Some("没有找到结果。"));
    assert!(empty_say(&bare).unwrap().chars().count() <= 40);
    let _ = ToolStatus::Completed;
}

#[test]
fn truncation_never_splits_a_utf8_character() {
    let base = "晚风信笺光年之外".repeat(50);
    for max in [1usize, 2, 3, 4, 5, 600, 601, 602] {
        let mut s = base.clone();
        truncate_at_char_boundary(&mut s, max);
        let kept = s.strip_suffix('…').unwrap_or(&s);
        assert!(
            base.starts_with(kept),
            "truncated prefix must be a real prefix (max={max})"
        );
        assert!(
            s.len() <= max + '…'.len_utf8(),
            "bound respected (max={max})"
        );
        assert_ne!(s.len(), 0, "never truncates to an invalid state");
    }
}

#[test]
fn truncation_short_strings_untouched() {
    let mut s = "短".to_string();
    truncate_at_char_boundary(&mut s, 600);
    assert_eq!(s, "短");
}

#[test]
fn cross_turn_utterance_and_marker_helpers() {
    assert!(is_continue_utterance("继续"));
    assert!(is_continue_utterance("  Continue! "));
    assert!(!is_continue_utterance("继续讲刚才那个故事好吗"));

    assert!(is_cancel_utterance("取消"));
    assert!(is_cancel_utterance("算了。"));
    assert!(is_cancel_utterance("stop"));
    assert!(!is_cancel_utterance("不要取消闹钟"));

    let (clean, next) = extract_next_step("第一步天气已查完。\n[[NEXT_STEP: 打开客厅灯]]");
    assert_eq!(clean, "第一步天气已查完。");
    assert_eq!(next.as_deref(), Some("打开客厅灯"));

    let (plain, none) = extract_next_step("普通回答。");
    assert_eq!(plain, "普通回答。");
    assert!(none.is_none());
}

#[tokio::test]
async fn consume_stream_aborts_on_mid_stream_idle_timeout() {
    use crate::llm::LlmEvent;
    use std::time::Duration;
    let stream: crate::llm::LlmStream = Box::pin(
        futures::stream::iter(vec![LlmEvent::Delta("第一句。".into())])
            .chain(futures::stream::pending()),
    );
    let cancel = crate::task::CancellationScope::default();
    let (mut tx, _rx) = futures::channel::mpsc::unbounded();
    let res = consume_stream(
        stream,
        Duration::from_secs(2),
        Duration::from_millis(60),
        Duration::from_secs(5),
        &cancel,
        &mut tx,
        false,
        false,
    )
    .await;
    let (err, emitted) = match res {
        Ok(_) => panic!("stalled stream must time out"),
        Err(e) => e,
    };
    assert!(emitted, "first delta was emitted before stall");
    assert!(
        err.to_string().contains("idle timeout"),
        "unexpected err: {err}"
    );
}

#[tokio::test]
async fn consume_stream_wakes_immediately_on_cancel() {
    use std::sync::Arc;
    use std::time::Duration;
    let stream: crate::llm::LlmStream = Box::pin(futures::stream::pending());
    let cancel = Arc::new(crate::task::CancellationScope::default());
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        c2.cancel();
    });
    let (mut tx, _rx) = futures::channel::mpsc::unbounded();
    let t0 = tokio::time::Instant::now();
    let res = consume_stream(
        stream,
        Duration::from_secs(10),
        Duration::from_secs(10),
        Duration::from_secs(10),
        &cancel,
        &mut tx,
        false,
        false,
    )
    .await;
    assert!(res.is_err());
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "cancel must wake stream immediately"
    );
}

#[tokio::test]
async fn consume_stream_cancels_when_receiver_drops() {
    use std::time::Duration;
    let stream: crate::llm::LlmStream = Box::pin(futures::stream::pending());
    let cancel = crate::task::CancellationScope::default();
    let (mut tx, rx) = futures::channel::mpsc::unbounded();
    drop(rx);
    let res = consume_stream(
        stream,
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_secs(5),
        &cancel,
        &mut tx,
        false,
        false,
    )
    .await;
    assert!(res.is_err());
    assert!(cancel.flag().load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn multi_tool_batch_stops_before_second_tool_when_cancelled() {
    use super::{run_turn_with_cancellation, AgentEnv, AgentEvent};
    use crate::llm::fake::{FakeLlm, Scripted};
    use crate::tool::{
        Availability, Registry, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolResult,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CancellingTool {
        name: &'static str,
        calls: Arc<AtomicUsize>,
        cancel_on_run: Option<Arc<crate::task::CancellationScope>>,
    }
    #[async_trait::async_trait]
    impl Tool for CancellingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: self.name.into(),
                service: "test".into(),
                side_effect: SideEffect::Query,
                description: "test tool".into(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }
        }
        fn availability(&self) -> Availability {
            Availability::Ready
        }
        async fn execute(&self, _: serde_json::Value, _: &ToolCtx) -> ToolResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(c) = &self.cancel_on_run {
                c.cancel();
            }
            ToolResult::observed_ok(serde_json::json!({"ran": self.name}))
        }
    }

    let cancel = Arc::new(crate::task::CancellationScope::default());
    let calls_a = Arc::new(AtomicUsize::new(0));
    let calls_b = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::new();
    let _ = registry.register(Arc::new(CancellingTool {
        name: "probe.first",
        calls: calls_a.clone(),
        cancel_on_run: Some(cancel.clone()),
    }));
    let _ = registry.register(Arc::new(CancellingTool {
        name: "probe.second",
        calls: calls_b.clone(),
        cancel_on_run: None,
    }));

    let llm = FakeLlm::new(vec![Scripted::Calls {
        calls: vec![
            ("probe.first".into(), "{}".into()),
            ("probe.second".into(), "{}".into()),
        ],
    }]);

    let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
    let config = crate::config::ConfigService::new(store.clone());
    let env = Arc::new(AgentEnv {
        prompt_preferences: config.prompt_preferences().unwrap_or_default(),
        llm: Arc::new(llm),
        model: "fake".into(),
        llm_fallback: None,
        registry: Arc::new(registry),
        sessions: Arc::new(crate::session::SessionStore::new(store.clone())),
        tasks: Arc::new(crate::task::TaskRegistry::new(store.clone())),
        traces: Arc::new(crate::trace::TraceRecorder::new(store.clone())),
        memory: crate::memory::MemoryService::new(store.clone()),
        working_memory: crate::working_memory::WorkingMemoryStore::new(store),
        shared_memory: None,
        user_md: None,
        scope: Default::default(),
        assistant_name: "高达".into(),
        llm_first_event_timeout: std::time::Duration::from_secs(2),
    });

    let mut stream = Box::pin(run_turn_with_cancellation(
        env,
        None,
        "执行两个工具".into(),
        "test".into(),
        super::prompt::Delivery::Text,
        cancel,
    ));
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev);
    }
    assert_eq!(calls_a.load(Ordering::SeqCst), 1, "first tool must run");
    assert_eq!(
        calls_b.load(Ordering::SeqCst),
        0,
        "second tool in batch must not execute after cancellation"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::TurnEnd { status, .. } if status == "cancelled")));
}
