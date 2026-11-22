//! M3 acceptance scenario set (deterministic, scripted model).
//!
//! This file PINS the user's acceptance bar as repeatable contract tests.
//! Real-model runs of the same bar live in scripts/verify_m3.py — the two
//! are deliberately separate: deterministic tests assert the CONTRACT (what
//! reaches the model, what the tools did), the script asserts the real
//! model's end-to-end behavior.
//!
//! Scenario index (category → test → where pinned):
//!
//! 新说法变体 (novel phrasings)
//!   · chart phrasings reach the model verbatim ....... acceptance.rs #6
//!   · device-entry utterances persist verbatim ...... scenarios.rs S1
//! 纠正 (corrections)
//!   · story revision keeps context .................. acceptance.rs #1
//!   · cross-turn song correction re-picks ........... scenarios.rs S2
//!   · "what just played" grounded in receipts ....... scenarios.rs S5
//! 失败换路 (failure fallback)
//!   · play failure → re-pick runner-up .............. acceptance.rs #3
//!   · unknown evidence never reported as success .... acceptance.rs #4
//!   · empty chart reported honestly ................. scenarios.rs S3
//!   · no library match → no substitution ............ scenarios.rs S4
//! 迟到/取消边界 (late/cancel boundaries)
//!   · mid-turn cancel, late result recorded ......... acceptance.rs #5
//!   · stale device events recorded not acted on ..... gateway_voice.rs
//!   · stop during synthesis (button/utterance/window) gateway_voice.rs

use futures::StreamExt;
use hm_core::agent::{run_turn, AgentEnv, AgentEvent};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::store::Store;
use hm_core::tool::mocks::MockInject;
use std::sync::Arc;

mod common;

/// A full app (real registry, real gateway socket) with a fake music service
/// and a fake device that confirms streamed plays. Music is a CONFIGURED
/// SERVICE now (R1e) — these scenarios exercise the real Subsonic/Personal
/// Services protocol, not a mock library.
async fn make_env(llm: FakeLlm, inject: MockInject) -> (Arc<AgentEnv>, Arc<common::FakeMusic>) {
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
    common::configure_music(&store, &fake, true, true, true, true);
    common::spawn_fake_device(gw_port).await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let env = state.agent_env().expect("agent env");
    (env, Arc::new(fake))
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

fn session_of(events: &[AgentEvent]) -> String {
    match &events[0] {
        AgentEvent::Session { session_id } => session_id.clone(),
        _ => panic!("first event should be session"),
    }
}

/// Join every request the model saw, for context assertions.
fn context_of(llm: &FakeLlm, call: usize) -> String {
    let seen = llm.seen_requests();
    serde_json::to_string(&seen[call]).unwrap()
}

// --- S1: device-entry novel phrasings persist verbatim -------------------------

#[tokio::test]
async fn s1_device_utterances_persist_verbatim() {
    // The voice entry (device utterance) is the user's everyday path: any
    // invented phrasing must land in the session exactly as spoken — there
    // is no keyword router in front of the model on this path either.
    let phrasings = [
        "榜单最火那首来听听",
        "讲个故事听听嘛",
        "帮我看看今天流行啥歌",
    ];
    for text in phrasings {
        let llm = FakeLlm::new(vec![Scripted::Text {
            chunks: vec!["好的。".into()],
        }]);
        let (env, _music) = make_env(llm.clone(), MockInject::default()).await;
        let events = drive(run_turn(
            env,
            Some("dev:dot2-d".into()),
            text.into(),
            "device:text".into(),
        ))
        .await;
        assert_eq!(answer_of(&events), "好的。");
        let ctx = context_of(&llm, 0);
        assert!(
            ctx.contains(text),
            "device phrasing '{text}' must reach the model verbatim"
        );
    }
}

// --- S2: cross-turn correction re-picks the runner-up ---------------------------

#[tokio::test]
async fn s2_song_correction_repicks_with_receipt_grounding() {
    // Turn 1 plays chart #1 (grounded in the receipt). Turn 2 is a bare
    // correction ("不是这首，换第二首"): the loop must resolve it against the
    // PRIOR turn's tool evidence, search #2, and play the new track — the
    // play sink is the sole source of "what actually played".
    let llm = FakeLlm::new(vec![
        // Turn 1: chart → search → play #1.
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
            chunks: vec!["正在播放《晚风信笺》。".into()],
        },
        // Turn 2: correction — search #2, play it.
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
            chunks: vec!["换成《光年之外的光》了。".into()],
        },
    ]);
    let (env, music) = make_env(llm.clone(), MockInject::default()).await;

    let ev1 = drive(run_turn(
        env.clone(),
        None,
        "放第一名的歌".into(),
        "text".into(),
    ))
    .await;
    let sid = session_of(&ev1);
    assert!(answer_of(&ev1).contains("晚风信笺"));

    let ev2 = drive(run_turn(
        env,
        Some(sid),
        "不是这首，换第二首".into(),
        "text".into(),
    ))
    .await;
    assert!(answer_of(&ev2).contains("光年之外的光"));

    // What actually played — the service's own stream log, in order.
    let played: Vec<String> = music
        .hits()
        .into_iter()
        .filter(|h| h.starts_with("stream:"))
        .collect();
    assert_eq!(
        played,
        vec!["stream:t01", "stream:t02"],
        "correction must re-pick, not replay"
    );

    // The correction turn's model context carries the FIRST play's receipt
    // (that is how "第二首" resolves) and the play receipt for t02.
    let ctx2 = context_of(&llm, 4);
    assert!(
        ctx2.contains("晚风信笺"),
        "first play's receipt must be in context"
    );
    let ctx_last = context_of(&llm, 6);
    eprintln!("CTX_LAST={ctx_last}");
    assert!(
        ctx_last.contains("playing"),
        "second play receipt must reach the answer call"
    );
    assert!(
        ctx_last.contains("device streaming"),
        "the device's own start stage is in the receipt"
    );
}

// --- S3: empty chart reported honestly ------------------------------------------

#[tokio::test]
async fn s3_empty_chart_is_reported_not_faked() {
    // No chart data: the tool says EMPTY_CHART, the model relays it, and no
    // play is submitted — an empty result must not be dressed up.
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("hotlist.query".into(), "{}".into())],
        },
        Scripted::Text {
            chunks: vec!["今天榜单暂时没有数据。".into()],
        },
    ]);
    let (env, music) = make_env(
        llm.clone(),
        MockInject {
            hotlist_empty: true,
            ..Default::default()
        },
    )
    .await;

    let events = drive(run_turn(env, None, "来首榜上最火的".into(), "text".into())).await;
    // M2 (2026-11-13): an all-failure turn speaks the tool's OWN short line
    // ("榜单暂无数据") instead of model prose — the empty result is stated,
    // never dressed up.
    assert!(
        answer_of(&events).contains("暂无数据"),
        "{}",
        answer_of(&events)
    );
    assert_eq!(
        music.count("stream:"),
        0,
        "empty chart must not trigger a play"
    );
    assert!(
        context_of(&llm, 1).contains("EMPTY_CHART"),
        "the honest failure code reached the model"
    );
}

// --- S4: no library match → no substitution -------------------------------------

#[tokio::test]
async fn s4_no_match_reports_empty_without_substitute() {
    // Chart #8 exists but the library does not carry it. The contract: the
    // empty candidate list reaches the model verbatim, the user constraint
    // is not silently widened, and nothing unrelated is played.
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "music.search".into(),
                r#"{"query":"午夜电台不播的歌"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["这首曲库里没有，没法播。".into()],
        },
    ]);
    let (env, music) = make_env(llm.clone(), MockInject::default()).await;

    let events = drive(run_turn(env, None, "放榜单第八名".into(), "text".into())).await;
    // M7 residual (2026-11-13): an empty result is delivered like a failure —
    // the core owns the ONE line and the model's prose is dropped. The
    // honesty this test guards is unchanged (nothing is substituted and the
    // answer says there is no match); the sentence is now the core's.
    assert!(
        answer_of(&events).contains("没找到"),
        "an empty result is stated in one core line: {}",
        answer_of(&events)
    );
    assert_eq!(
        music.count("stream:"),
        0,
        "no track may be substituted for the missing one"
    );
    let ctx = context_of(&llm, 1);
    assert!(
        ctx.contains("no_matches"),
        "the honest search result reached the model"
    );
    assert!(
        !ctx.contains("晚风信笺"),
        "unrelated candidates must not appear"
    );
}

// --- S5: "what just played" grounded in the room's play evidence ------------------

#[tokio::test]
async fn s5_prior_play_query_is_grounded_in_session_evidence() {
    // After a confirmed play, asking "刚才放的什么" must be answerable from
    // the SAME session's tool receipts — not from invented state. The fake
    // model's answer is scripted; the assertion is that the receipt is in
    // its context (grounding), which is the system's part of the contract.
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("music.search".into(), r#"{"query":"晚风信笺"}"#.into())],
        },
        Scripted::Calls {
            calls: vec![(
                "music.play".into(),
                r#"{"source":"library","id":"t01"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["正在播放《晚风信笺》。".into()],
        },
        Scripted::Text {
            chunks: vec!["刚才放的是《晚风信笺》。".into()],
        },
    ]);
    let (env, _music) = make_env(llm.clone(), MockInject::default()).await;

    let ev1 = drive(run_turn(
        env.clone(),
        None,
        "放首晚风信笺".into(),
        "text".into(),
    ))
    .await;
    let sid = session_of(&ev1);
    let ev2 = drive(run_turn(
        env,
        Some(sid),
        "刚才放的什么歌".into(),
        "text".into(),
    ))
    .await;
    assert!(answer_of(&ev2).contains("晚风信笺"));

    // The follow-up turn's model call carried the play receipt.
    let ctx = context_of(&llm, 3);
    assert!(
        ctx.contains("playing"),
        "play receipt must be in the follow-up context"
    );
    assert!(
        ctx.contains("晚风信笺"),
        "the played track name must be in the follow-up context"
    );
}

// --- S6: play failure over the gateway falls back honestly ------------------------
// (gateway-level failure paths are pinned in gateway_voice.rs: unconfirmed
// play stays unknown; simulated sink discloses the simulated receipt. Here
// we pin the brain-level contract that a failed play is NOT retried
// implicitly — the model decides the re-pick from the failure code.)

#[tokio::test]
async fn s6_failed_play_not_implicitly_resubmitted() {
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![("music.search".into(), r#"{"query":"小城路灯"}"#.into())],
        },
        Scripted::Calls {
            calls: vec![(
                "music.play".into(),
                r#"{"source":"library","id":"t03"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["这首放不了。".into()],
        },
    ]);
    let (env, music) = make_env(llm.clone(), MockInject::default()).await;
    // The service accepts the search but fails the stream (a real 500).
    *music.fail_stream.lock().unwrap() = Some("t03".into());
    let events = drive(run_turn(env, None, "放小城路灯".into(), "text".into())).await;
    // R2-BM5: the failure is spoken by the CORE (the model's prose is not the
    // authority for an action outcome).
    assert!(
        answer_of(&events).contains("没有完成《小城路灯》的播放"),
        "{}",
        answer_of(&events)
    );
    assert_eq!(
        music.count("stream:t03"),
        1,
        "exactly one fetch — no hidden retry"
    );
    let tool = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                tool,
                status,
                evidence,
                ..
            } if tool == "music.play" => Some((status.clone(), evidence.clone())),
            _ => None,
        })
        .expect("play tool result");
    // The stream failed BEFORE submission: no side effect happened, so the
    // evidence is "observed" (submitted would claim it reached the device).
    assert_eq!(tool, ("failed".into(), "observed".into()));
}
