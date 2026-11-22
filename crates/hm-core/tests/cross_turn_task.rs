//! M6 Cross-Turn Task integration tests (core#20).
//!
//! Verifies:
//! 1. A two-step request completes step 1 (`AwaitingContinue`), then saying
//!    「继续」 injects the pending task context (`goal`, `next_socket`,
//!    `last_summary`) and completes step 2 without repeating the original prompt.
//! 2. Cancelling after step 1 (via 「取消」 / 「算了」 or explicit cancel)
//!    marks the task Cancelled so a subsequent 「继续」 never executes step 2.
//! 3. Normal short-conversation turns without a multi-step task behave
//!    identically to before (no cross-turn prompt injection).

use futures::StreamExt;
use hm_core::agent::{run_turn, AgentEnv, AgentEvent};
use hm_core::config::ConfigService;
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::memory::MemoryService;
use hm_core::session::SessionStore;
use hm_core::store::Store;
use hm_core::task::{CrossTurnState, TaskRegistry};
use hm_core::tool::Registry;
use hm_core::trace::TraceRecorder;
use std::sync::Arc;

fn make_env(llm: FakeLlm) -> Arc<AgentEnv> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let config = Arc::new(ConfigService::new(store.clone()));
    let sessions = Arc::new(SessionStore::new(store.clone()));
    let tasks = Arc::new(TaskRegistry::new(store.clone()));
    let traces = Arc::new(TraceRecorder::new(store.clone()));
    let memory = MemoryService::new(store.clone());
    let working_memory = hm_core::working_memory::WorkingMemoryStore::new(store);
    let registry = Arc::new(Registry::new());
    Arc::new(AgentEnv {
        prompt_preferences: config.prompt_preferences().unwrap_or_default(),
        llm: Arc::new(llm),
        model: "fake-model".into(),
        llm_fallback: None,
        registry,
        sessions,
        tasks,
        traces,
        memory,
        working_memory,
        shared_memory: None,
        user_md: None,
        scope: Default::default(),
        assistant_name: "高达".into(),
        llm_first_event_timeout: std::time::Duration::from_secs(5),
    })
}

async fn drive(mut stream: impl futures::Stream<Item = AgentEvent> + Unpin) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    while let Some(ev) = stream.next().await {
        out.push(ev);
    }
    out
}

fn session_of(events: &[AgentEvent]) -> String {
    match &events[0] {
        AgentEvent::Session { session_id } => session_id.clone(),
        other => panic!("expected Session first, got {other:?}"),
    }
}

fn answer_of(events: &[AgentEvent]) -> String {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::Answer { text } => Some(text.clone()),
            _ => None,
        })
        .expect("turn produced an Answer")
}

#[tokio::test]
async fn two_step_request_continues_on_continue_without_repeating_prompt() {
    let llm = FakeLlm::new(vec![
        // Turn 1: Step 1 finishes and schedules step 2 via [[NEXT_STEP: ...]]
        Scripted::Text {
            chunks: vec![
                "第一步结果：今天北京晴，22度。\n[[NEXT_STEP: 给出客厅空调温度建议]]".into(),
            ],
        },
        // Turn 2: User says only "继续"; model receives injected cross-turn context
        Scripted::Text {
            chunks: vec!["第二步结果：建议将客厅空调调到26度。".into()],
        },
    ]);
    let llm_ref = llm.clone();
    let env = make_env(llm);

    // Step 1
    let ev1 = drive(run_turn(
        env.clone(),
        None,
        "先帮我查北京天气，然后等我说继续再给出客厅空调温度建议".into(),
        "console".into(),
    ))
    .await;
    let sid = session_of(&ev1);
    let ans1 = answer_of(&ev1);
    assert_eq!(ans1, "第一步结果：今天北京晴，22度。");
    assert!(
        !ans1.contains("NEXT_STEP"),
        "marker must be stripped from answer"
    );

    let ct = env
        .tasks
        .get_cross_turn(&sid)
        .expect("cross-turn task recorded");
    assert_eq!(ct.state, CrossTurnState::AwaitingContinue);
    assert_eq!(ct.next_socket.as_deref(), Some("给出客厅空调温度建议"));
    assert!(!ct.cancelled);

    // Step 2: user only says "继续"
    let ev2 = drive(run_turn(
        env.clone(),
        Some(sid.clone()),
        "继续".into(),
        "console".into(),
    ))
    .await;
    let ans2 = answer_of(&ev2);
    assert_eq!(ans2, "第二步结果：建议将客厅空调调到26度。");

    let ct_done = env
        .tasks
        .get_cross_turn(&sid)
        .expect("cross-turn task still tracked");
    assert_eq!(ct_done.state, CrossTurnState::Completed);
    assert!(!env.tasks.has_pending_cross_turn(&sid));

    // Verify the model saw the injected cross-turn context in Turn 2
    let seen = llm_ref.seen_requests();
    assert_eq!(seen.len(), 2);
    let turn2_req = serde_json::to_string(&seen[1]).unwrap();
    assert!(turn2_req.contains("当前跨回合任务"));
    assert!(turn2_req.contains("给出客厅空调温度建议"));
}

#[tokio::test]
async fn cancel_after_step_one_prevents_step_two_on_continue() {
    let llm = FakeLlm::new(vec![
        // Turn 1: Step 1 finishes with a pending next step
        Scripted::Text {
            chunks: vec!["第一步已完成：已列出待办事项。\n[[NEXT_STEP: 发送待办清单邮件]]".into()],
        },
        // Turn 3 (if user says "继续" after cancel): treated as ordinary turn, NO cross-turn context
        Scripted::Text {
            chunks: vec!["现在没有待继续的任务。".into()],
        },
    ]);
    let llm_ref = llm.clone();
    let env = make_env(llm);

    // Step 1
    let ev1 = drive(run_turn(
        env.clone(),
        None,
        "先列出待办事项，再发邮件".into(),
        "console".into(),
    ))
    .await;
    let sid = session_of(&ev1);
    assert!(env.tasks.has_pending_cross_turn(&sid));

    // User cancels with "算了" — handled without invoking LLM for step 2
    let ev_cancel = drive(run_turn(
        env.clone(),
        Some(sid.clone()),
        "算了".into(),
        "console".into(),
    ))
    .await;
    assert_eq!(answer_of(&ev_cancel), "好的，已取消后续步骤。");

    let ct = env.tasks.get_cross_turn(&sid).unwrap();
    assert_eq!(ct.state, CrossTurnState::Cancelled);
    assert!(ct.cancelled);
    assert!(!env.tasks.has_pending_cross_turn(&sid));

    // Subsequent "继续" must NOT receive the cancelled step 2 context
    let ev_after = drive(run_turn(
        env.clone(),
        Some(sid.clone()),
        "继续".into(),
        "console".into(),
    ))
    .await;
    assert_eq!(answer_of(&ev_after), "现在没有待继续的任务。");

    let seen = llm_ref.seen_requests();
    assert_eq!(seen.len(), 2, "cancel turn must not call LLM");
    let turn3_req = serde_json::to_string(&seen[1]).unwrap();
    assert!(!turn3_req.contains("当前跨回合任务"));
    assert!(!turn3_req.contains("发送待办清单邮件"));
}

#[tokio::test]
async fn short_conversation_without_task_has_no_cross_turn_injection() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["你好！".into()],
        },
        Scripted::Text {
            chunks: vec!["我是家庭伙伴。".into()],
        },
    ]);
    let llm_ref = llm.clone();
    let env = make_env(llm);

    let ev1 = drive(run_turn(env.clone(), None, "你好".into(), "console".into())).await;
    let sid = session_of(&ev1);
    assert!(!env.tasks.has_pending_cross_turn(&sid));
    assert!(env.tasks.get_cross_turn(&sid).is_none());

    let ev2 = drive(run_turn(
        env.clone(),
        Some(sid.clone()),
        "你是谁".into(),
        "console".into(),
    ))
    .await;
    assert_eq!(answer_of(&ev2), "我是家庭伙伴。");

    let seen = llm_ref.seen_requests();
    assert_eq!(seen.len(), 2);
    for req in &seen {
        let s = serde_json::to_string(req).unwrap();
        assert!(!s.contains("当前跨回合任务"));
    }
}
