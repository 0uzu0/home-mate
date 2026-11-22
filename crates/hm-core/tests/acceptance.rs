//! Acceptance scenarios for the M1 brain loop, driven by a scripted fake
//! model. These encode the user's acceptance bar (docs/HANDOFF.md §0 /
//! memory of the 2026-11-07 handover):
//!
//! 1. Bedtime story → "换个短一点的" keeps context.
//! 2. "放热榜上的歌" chains chart → search → play and answers from the
//!    tool's ACTUAL receipt.
//! 3. Play failure reaches the model verbatim; it may re-pick.
//! 4. Unknown evidence is never reported as success.
//! 5. Mid-turn stop cancels; late results are recorded, not acted on.
//! 6. Novel phrasings flow through unchanged (no per-sentence routing).

use futures::StreamExt;
use hm_core::agent::{run_turn, AgentEnv, AgentEvent};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::store::Store;
use hm_core::tool::mocks::{mock_registry, MockInject};
use std::sync::Arc;

mod common;

/// A full app with a REAL music protocol partner (fake Subsonic/Personal
/// Services) and a fake device on the gateway socket. Music is a configured
/// service since R1e; the receipts these tests assert on are produced by the
/// same code that runs on the speaker.
async fn make_env(
    llm: FakeLlm,
    inject: MockInject,
) -> (
    Arc<AgentEnv>,
    Arc<hm_core::task::TaskRegistry>,
    Arc<common::FakeMusic>,
) {
    make_env_confirm(llm, inject, true).await
}

async fn make_env_confirm(
    llm: FakeLlm,
    inject: MockInject,
    confirm: bool,
) -> (
    Arc<AgentEnv>,
    Arc<hm_core::task::TaskRegistry>,
    Arc<common::FakeMusic>,
) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let state = hm_core::api::AppState::with_overrides(
        store.clone(),
        std::path::PathBuf::from("./data-test"),
        inject,
        Some(Arc::new(llm)),
        None,
        None,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gw_port).into());
    let fake = common::spawn_fake_music().await;
    common::configure_music(&store, &fake, true, false, true, true);
    common::spawn_fake_device_opts(gw_port, confirm).await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let env = state.agent_env().expect("agent env");
    (env, state.tasks.clone(), Arc::new(fake))
}

/// Like `make_env` but with a short first-event timeout, for the
/// 200-then-hang failure mode.
fn make_env_fast_timeout(
    llm: FakeLlm,
    inject: MockInject,
    timeout: std::time::Duration,
) -> Arc<AgentEnv> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let (registry, _sink, _weather) = mock_registry(inject);
    Arc::new(AgentEnv {
        prompt_preferences: Default::default(),
        llm: Arc::new(llm),
        model: "fake-model".into(),
        registry: Arc::new(registry),
        sessions: Arc::new(hm_core::session::SessionStore::new(store.clone())),
        tasks: Arc::new(hm_core::task::TaskRegistry::new(store.clone())),
        traces: Arc::new(hm_core::trace::TraceRecorder::new(store.clone())),
        memory: hm_core::memory::MemoryService::new(store.clone()),
        working_memory: hm_core::working_memory::WorkingMemoryStore::new(store.clone()),
        shared_memory: None,
        user_md: None,
        scope: Default::default(),
        assistant_name: "高达".into(),
        llm_fallback: None,
        llm_first_event_timeout: timeout,
    })
}

async fn drive(mut stream: impl futures::Stream<Item = AgentEvent> + Unpin) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    while let Some(ev) = stream.next().await {
        out.push(ev);
    }
    out
}

fn answer_of(events: &[AgentEvent]) -> String {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::Answer { text } => Some(text.clone()),
            _ => None,
        })
        .expect("turn produced an answer")
}

fn tool_results<'a>(events: &'a [AgentEvent]) -> Vec<&'a AgentEvent> {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .collect()
}

// --- 1. story revision keeps context -----------------------------------------

#[tokio::test]
async fn story_revision_keeps_context() {
    let story1 = "从前有一只小狐狸，它住在很远的森林里……（这里本来有很长的三段）……月亮说：睡吧。";
    let story2 = "短版：小狐狸数了三颗星星，就睡着了。";
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec![story1.into()],
        },
        Scripted::Text {
            chunks: vec![story2.into()],
        },
    ]);
    let (env, _tasks, _music) = make_env(llm.clone(), MockInject::default()).await;

    let ev1 = drive(run_turn(
        env.clone(),
        None,
        "讲个睡前故事".into(),
        "text".into(),
    ))
    .await;
    let sid = match &ev1[0] {
        AgentEvent::Session { session_id } => session_id.clone(),
        _ => panic!("first event should be session"),
    };
    assert!(answer_of(&ev1).contains("小狐狸"));

    let ev2 = drive(run_turn(
        env,
        Some(sid),
        "换个短一点的".into(),
        "text".into(),
    ))
    .await;
    assert_eq!(answer_of(&ev2), story2);

    // The revision request reached the model WITH the original story and
    // the revision ask — that is the whole acceptance point.
    let seen = llm.seen_requests();
    assert_eq!(seen.len(), 2, "one model call per turn");
    let ctx2 = &seen[1];
    let joined: String = ctx2
        .iter()
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("小狐狸"),
        "second call still carries the first story"
    );
    assert!(
        joined.contains("换个短一点的"),
        "second call carries the revision request"
    );
}

// --- 2. hotlist chain, answer grounded in the receipt ------------------------

#[tokio::test]
async fn hotlist_song_chain_is_grounded_in_actual_result() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("hotlist.query".into(), "{}".into())],
        },
        Scripted::Calls {
            calls: vec![("music.search".into(), r#"{"query":"晚风"}"#.into())],
        },
        Scripted::Calls {
            calls: vec![(
                "music.play".into(),
                r#"{"source":"library","id":"t01"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["正在为你播放《晚风信笺》。".into()],
        },
    ]);
    let (env, _tasks, _music) = make_env(llm.clone(), MockInject::default()).await;

    let events = drive(run_turn(
        env,
        None,
        "放一首热榜上的歌".into(),
        "text".into(),
    ))
    .await;
    assert!(answer_of(&events).contains("晚风信笺"));

    // Tool order and the receipt the model was given:
    let starts: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolStart { tool, .. } => Some(tool.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec!["hotlist.query", "music.search", "music.play"]);

    let play = tool_results(&events).last().cloned();
    match play {
        Some(AgentEvent::ToolResult {
            status, evidence, ..
        }) => {
            assert_eq!(status, "completed");
            // R2-BM5: a streamed play is "started" only when the device
            // reports `streaming` (the first block was consumed). `accepted`
            // is begin validation, not playback; the terminal
            // device_confirmed comes when the song drains.
            assert_eq!(evidence, "sent");
        }
        other => panic!("expected play result, got {other:?}"),
    }

    // The final model call saw the play receipt (played: true) verbatim.
    let mut seen = llm.seen_requests();
    let last_ctx = seen.pop().unwrap();
    let joined = serde_json::to_string(&last_ctx).unwrap();
    assert!(
        joined.contains("playing"),
        "receipt entered the model context"
    );
    assert!(
        joined.contains("device streaming"),
        "the device's own start stage is in the receipt: {joined}"
    );
}

// --- 3. play failure reaches the model verbatim; re-pick allowed -------------

#[tokio::test]
async fn play_failure_is_passed_through_and_repick_happens() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("hotlist.query".into(), "{}".into())],
        },
        Scripted::Calls {
            calls: vec![("music.search".into(), r#"{"query":"晚风"}"#.into())],
        },
        Scripted::Calls {
            calls: vec![(
                "music.play".into(),
                r#"{"source":"library","id":"t01"}"#.into(),
            )],
        },
        // Model sees the fetch failure and re-picks the runner-up.
        Scripted::Calls {
            calls: vec![("music.search".into(), r#"{"query":"光年之外"}"#.into())],
        },
        Scripted::Calls {
            calls: vec![(
                "music.play".into(),
                r#"{"source":"library","id":"t02"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["第一首放不出来，帮你换成《光年之外的光》。".into()],
        },
    ]);
    let (env, _tasks, music) = make_env(llm.clone(), MockInject::default()).await;
    // The service accepts the search but fails t01's stream (a real 500).
    *music.fail_stream.lock().unwrap() = Some("t01".into());

    let events = drive(run_turn(env, None, "来首榜上最火的".into(), "text".into())).await;
    // R2-BM5: action sentences are the CORE's. The failed first play and the
    // successful re-pick are both rendered from their receipts; the model's
    // prose is not the authority (and is dropped on a fact turn).
    assert!(
        answer_of(&events).contains("没有完成《晚风信笺》的播放"),
        "final answer reflects the failure: {}",
        answer_of(&events)
    );
    assert!(
        answer_of(&events).contains("已开始播放《光年之外的光》"),
        "{}",
        answer_of(&events)
    );

    // The model call that decided the re-pick had seen the failure code.
    let seen = llm.seen_requests();
    let joined = serde_json::to_string(&seen[3]).unwrap();
    assert!(
        joined.contains("MUSIC_HTTP"),
        "failure code reached the model"
    );
    assert!(
        joined.contains("observed"),
        "nothing reached the device, and the receipt says so"
    );
}

// --- 4. unknown evidence is never success -------------------------------------

#[tokio::test]
async fn unknown_receipt_is_exposed_not_hidden() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("music.search".into(), r#"{"query":"晚风"}"#.into())],
        },
        Scripted::Calls {
            calls: vec![(
                "music.play".into(),
                r#"{"source":"library","id":"t01"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["我提交了播放，但还没收到设备确认；如果没声音我再试。".into()],
        },
    ]);
    // A device that accepts and never confirms: unconfirmed must stay unknown.
    let (env, _tasks, _music) = make_env_confirm(llm.clone(), MockInject::default(), false).await;

    let events = drive(run_turn(env, None, "放首晚风信笺".into(), "text".into())).await;
    let play = tool_results(&events).last().cloned();
    match play {
        Some(AgentEvent::ToolResult {
            status, evidence, ..
        }) => {
            assert_eq!(status, "unknown", "an unanswered play stays unknown");
            assert_eq!(evidence, "unknown");
        }
        other => panic!("expected unknown play result, got {other:?}"),
    }
    // The final context carries CONFIRM_TIMEOUT — no rewrite into success.
    let seen = llm.seen_requests();
    let joined = serde_json::to_string(seen.last().unwrap()).unwrap();
    assert!(joined.contains("CONFIRM_TIMEOUT"));
    // The core renders the unconfirmed play fact; the model's prose is not
    // the answer (BM5-A's enforced boundary).
    assert!(
        answer_of(&events).contains("没能确认《晚风信笺》是否播放"),
        "{}",
        answer_of(&events)
    );
}

// --- 5. mid-turn cancel: late result recorded, no further model call ----------

#[tokio::test]
async fn mid_turn_cancel_stops_and_records_late_result() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("hotlist.query".into(), "{}".into())],
        }, // delayed 300ms
        Scripted::Text {
            chunks: vec!["榜单一".into()],
        }, // must NEVER run
    ]);
    let (env, tasks, _music) = make_env(
        llm.clone(),
        MockInject {
            hotlist_delay_ms: 300,
            ..Default::default()
        },
    )
    .await;

    let stream = run_turn(env, None, "放热榜上的歌".into(), "text".into());
    let handle = tokio::task::spawn(async move { drive(stream).await });

    // Wait for the task id, then cancel while hotlist.query is sleeping.
    let mut task_id = None;
    for _ in 0..100 {
        let list = tasks.list(5).unwrap();
        if let Some(t) = list.first() {
            if t["status"].as_str() == Some("running") {
                task_id = Some(t["id"].as_str().unwrap().to_string());
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let task_id = task_id.expect("task appeared");
    assert!(
        tasks.request_cancel(&task_id).unwrap(),
        "cancel accepted while running"
    );

    let events = handle.await.unwrap();
    let late_results: Vec<&AgentEvent> = tool_results(&events)
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ToolResult {
                    late: Some(true),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        late_results.len(),
        1,
        "delayed hotlist result recorded as late"
    );

    let cancelled = events
        .iter()
        .any(|e| matches!(e, AgentEvent::Cancelled { .. }));
    assert!(cancelled, "turn reported cancellation");
    let end = events.last().unwrap();
    match end {
        AgentEvent::TurnEnd { status, .. } => assert_eq!(status, "cancelled"),
        other => panic!("expected turn_end last, got {other:?}"),
    }
    assert_eq!(
        llm.seen_requests().len(),
        1,
        "no model call after cancellation"
    );
    let task_state = tasks.get(&task_id).unwrap();
    assert_eq!(task_state.status(), hm_core::task::TaskStatus::Cancelled);
}

// --- 6. novel phrasing passes through verbatim (no routing layer) -------------

#[tokio::test]
async fn novel_phrasings_flow_through_unchanged() {
    // Any phrasing the user invents must reach the model exactly as said;
    // there is no keyword pre-router anywhere in the loop.
    for novel in [
        "榜单最火那首来听听",
        "现在流行的歌放一个嘛",
        "整点网上都听的歌",
    ] {
        let llm = FakeLlm::new(vec![Scripted::Text {
            chunks: vec!["好的。".into()],
        }]);
        let (env, _t, _music) = make_env(llm.clone(), MockInject::default()).await;
        let events = drive(run_turn(env, None, novel.into(), "text".into())).await;
        assert_eq!(answer_of(&events), "好的。");
        let ctx = &llm.seen_requests()[0];
        // The user message must be present verbatim, untouched.
        let verbatim = ctx
            .iter()
            .any(|m| m.role == "user" && m.content.as_deref() == Some(novel));
        assert!(verbatim, "phrasing '{novel}' must reach the model verbatim");
    }
}

// --- 7. transient provider failure → bounded retry recovers -------------------

#[tokio::test]
async fn transient_request_error_is_retried() {
    let llm = FakeLlm::new(vec![
        Scripted::RequestError {
            message: "upstream 500 (injected)".into(),
        },
        Scripted::Text {
            chunks: vec!["重试后正常。".into()],
        },
    ]);
    let (env, _tasks, _music) = make_env(llm.clone(), MockInject::default()).await;
    let events = drive(run_turn(env, None, "你好".into(), "text".into())).await;
    assert_eq!(answer_of(&events), "重试后正常。");
    assert_eq!(
        llm.seen_requests().len(),
        2,
        "second attempt after transient failure"
    );
}

// --- 7b. primary gives up → secondary provider serves the turn ------------------

#[tokio::test]
async fn llm_fallback_serves_after_primary_gives_up() {
    let primary = FakeLlm::new(vec![
        Scripted::RequestError {
            message: "deepseek down 1".into(),
        },
        Scripted::RequestError {
            message: "deepseek down 2".into(),
        },
        Scripted::RequestError {
            message: "deepseek down 3".into(),
        },
    ]);
    let secondary = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["GLM 兜底回答。".into()],
    }]);
    let (mut env, _tasks, _music) = make_env(primary.clone(), MockInject::default()).await;
    Arc::get_mut(&mut env).unwrap().llm_fallback = Some(hm_core::agent::FallbackLlm {
        client: Arc::new(secondary.clone()),
        model: "glm-4-flash".into(),
        label: "glm/glm-4-flash".into(),
    });
    let events = drive(run_turn(env, None, "你好".into(), "text".into())).await;
    assert_eq!(
        answer_of(&events),
        "GLM 兜底回答。",
        "fallback answer must serve the turn"
    );
    assert_eq!(
        primary.seen_requests().len(),
        3,
        "primary retried its full budget"
    );
    assert_eq!(
        secondary.seen_requests().len(),
        1,
        "fallback tried exactly once"
    );
}

// --- 7c. fallback must NOT double a half-streamed answer -----------------------

#[tokio::test]
async fn llm_fallback_skipped_when_primary_emitted_tokens() {
    let primary = FakeLlm::new(vec![Scripted::DeltaThenFail {
        chunk: "半句".into(),
        message: "mid-stream death".into(),
    }]);
    let secondary = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["不该出现的第二答案".into()],
    }]);
    let (mut env, _tasks, _music) = make_env(primary.clone(), MockInject::default()).await;
    Arc::get_mut(&mut env).unwrap().llm_fallback = Some(hm_core::agent::FallbackLlm {
        client: Arc::new(secondary.clone()),
        model: "glm-4-flash".into(),
        label: "glm/glm-4-flash".into(),
    });
    let events = drive(run_turn(env, None, "你好".into(), "text".into())).await;
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "turn fails honestly"
    );
    assert_eq!(
        secondary.seen_requests().len(),
        0,
        "fallback must not double a half-streamed answer"
    );
}

// --- 8. retries exhausted → honest failure, no fake budget label ---------------

#[tokio::test]
async fn retry_exhaustion_fails_honestly() {
    let llm = FakeLlm::new(vec![
        Scripted::RequestError {
            message: "boom-1".into(),
        },
        Scripted::RequestError {
            message: "boom-2".into(),
        },
        Scripted::RequestError {
            message: "boom-3".into(),
        },
    ]);
    let (env, tasks, _music) = make_env(llm.clone(), MockInject::default()).await;
    let events = drive(run_turn(env, None, "你好".into(), "text".into())).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    match events.last().unwrap() {
        AgentEvent::TurnEnd { status, .. } => assert_eq!(status, "failed"),
        other => panic!("expected turn_end, got {other:?}"),
    }
    assert_eq!(llm.seen_requests().len(), 3, "gave up after 3 attempts");
    let list = tasks.list(5).unwrap();
    assert_eq!(list[0]["status"], "failed");
}

// --- 9. 200-then-hang → first-event timeout triggers retry ---------------------

#[tokio::test]
async fn first_event_timeout_is_transient_and_retried() {
    let llm = FakeLlm::new(vec![
        // Hangs longer than the 120ms first-event budget we inject.
        Scripted::Delayed {
            delay_ms: 800,
            chunks: vec!["迟到".into()],
        },
        Scripted::Text {
            chunks: vec!["这次正常。".into()],
        },
    ]);
    let env = make_env_fast_timeout(
        llm.clone(),
        MockInject::default(),
        std::time::Duration::from_millis(120),
    );
    let events = drive(run_turn(env, None, "你好".into(), "text".into())).await;
    assert_eq!(answer_of(&events), "这次正常。");
    assert_eq!(
        llm.seen_requests().len(),
        2,
        "hang counted as transient, retried once"
    );
}

// --- 10. deltas already streamed → never retried silently ----------------------

#[tokio::test]
async fn no_retry_after_partial_output() {
    let llm = FakeLlm::new(vec![
        Scripted::DeltaThenFail {
            chunk: "说了一半".into(),
            message: "mid-stream cut".into(),
        },
        Scripted::Text {
            chunks: vec!["不应到达".into()],
        },
    ]);
    let (env, _tasks, _music) = make_env(llm.clone(), MockInject::default()).await;
    let events = drive(run_turn(env, None, "你好".into(), "text".into())).await;
    assert_eq!(
        llm.seen_requests().len(),
        1,
        "client already saw text; no silent retry"
    );
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Token { text } if text.trim() == "说了一半")));
}
