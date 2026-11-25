//! The agent loop: one turn = user text → (model call ⇄ tool calls)* →
//! answer. Emits a stream of `AgentEvent`s consumed by the SSE endpoint.
//!
//! Design properties under test:
//! - Context is rebuilt from persisted history, so revisions ("换个短一点
//!   的") reach the model with the prior story intact.
//! - Tool results (including failures and `unknown`) enter the next model
//!   call verbatim; the loop never rewrites them into success.
//! - Cancellation is checked before each model call and each side effect;
//!   results arriving after cancellation are recorded as late, never acted
//!   on further.

pub mod facts;
pub mod prompt;

use crate::llm::{Finish, LlmClient, WireMessage};
use crate::session::SessionStore;
use crate::task::{CrossTurnState, TaskRegistry, TaskStatus};
use crate::tool::{Registry, ToolCtx};
use crate::trace::TraceRecorder;
use serde::Serialize;
use std::sync::Arc;

const MAX_MODEL_CALLS: usize = 8;
const MAX_TOOLS_PER_CALL: usize = 4;
const HISTORY_WINDOW: usize = 24;
/// Total attempts per model call: 1 try + 2 retries, backoff 1s then 3s.
/// Only transient errors (transport/408/429/5xx, first-event timeout) and
/// only BEFORE any token reached the client are retried.
const MAX_LLM_ATTEMPTS: usize = 3;
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const STREAM_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

fn backoff_for(attempt: usize) -> std::time::Duration {
    std::time::Duration::from_millis(if attempt == 1 { 1000 } else { 3000 })
}

/// What the core says when a turn's tools ALL failed and no action fact took
/// over the answer.
///
/// A failure is a fact the core owns, exactly like an action receipt: the
/// model may explain, but it may not stretch "no data" into a paragraph.
/// The person's complaint (2026-11-13) was a long answer for a weather
/// lookup that could not connect. Order of preference:
/// 1. the tool's own authoritative line (`rendered_answer`, already used by
///    weather) — the most specific honest sentence;
/// 2. its `detail` when that is short, Chinese and free of code tokens;
/// 3. a line derived from the error CLASS (timeout/offline/unconfigured/…),
///    never from the tool's name, so a new tool needs no new branch here.
/// An EMPTY result is not a success for the delivery shape (M7 residual,
/// 2026-11-13). `music.search` returns `observed_ok{status:"no_matches",
/// tracks:[]}`, so the "all tools failed" short-circuit never fired and the
/// model could stretch "no match" into a six-sentence Detail answer — the
/// person asked for one thing and heard a paragraph about why it is missing.
///
/// Empty = the tool succeeded at asking and got nothing back:
/// - a payload `status` of no_matches|empty|no_data|unavailable;
/// - a key LIST that exists and is empty (`tracks`, `items`).
/// An action fact is never empty (its own sentence is spoken by the fact
/// path), and a genuine failure stays `failure_say`'s job.
fn is_empty_result(result: &crate::tool::ToolResult) -> bool {
    if result.fact.is_some() {
        return false;
    }
    let Some(v) = result.result.as_ref() else {
        return false;
    };
    const EMPTY_STATUS: [&str; 4] = ["no_matches", "empty", "no_data", "unavailable"];
    if v.get("status")
        .and_then(|s| s.as_str())
        .map(|s| EMPTY_STATUS.contains(&s))
        .unwrap_or(false)
    {
        return true;
    }
    ["tracks", "items"].iter().any(|key| {
        v.get(*key)
            .and_then(|a| a.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(false)
    })
}

/// The one sentence spoken for an empty result: the tool's own authoritative
/// line when it has one (music.search now does), else a short generic line.
fn empty_say(result: &crate::tool::ToolResult) -> Option<String> {
    if let Some(line) = result
        .result
        .as_ref()
        .and_then(|r| r.get("rendered_answer"))
        .and_then(|v| v.as_str())
    {
        let line = line.trim();
        if !line.is_empty() {
            return Some(line.to_string());
        }
    }
    Some("没有找到结果。".to_string())
}

fn failure_say(result: &crate::tool::ToolResult) -> Option<String> {
    use crate::tool::ToolStatus;
    if matches!(
        result.status,
        ToolStatus::Completed | ToolStatus::Partial | ToolStatus::Cancelled
    ) {
        return None;
    }
    if let Some(r) = result.result.as_ref() {
        if let Some(line) = r.get("rendered_answer").and_then(|v| v.as_str()) {
            let line = line.trim();
            if !line.is_empty() {
                return Some(line.to_string());
            }
        }
    }
    let (code, detail) = match result.error.as_ref() {
        Some(e) => (e.code.to_uppercase(), e.detail.trim().to_string()),
        None => (String::new(), String::new()),
    };
    if !detail.is_empty()
        && detail.chars().count() <= 40
        && detail.chars().any(|c| c as u32 > 0x2E80)
        && !detail.contains('_')
        && !detail.contains('/')
        && !detail.chars().any(|c| c.is_ascii_uppercase())
    {
        return Some(detail);
    }
    let class = if code.contains("TIMEOUT") {
        "现在连不上这个服务，稍后再试。"
    } else if code.contains("OFFLINE") || code.contains("UNREACHABLE") {
        "现在网络不通，查不到。"
    } else if code.contains("UNCONFIGURED")
        || code.contains("NOT_CONFIGURED")
        || code.contains("DISABLED")
    {
        "这个服务还没配置，现在用不了。"
    } else if code.contains("AUTH") || code.contains("TOKEN") {
        "服务认证失败，需要重新配置。"
    } else if code.contains("EMPTY") || code.contains("NO_MATCH") {
        "没有查到相关内容。"
    } else if code.contains("STALE") {
        "没有可用的数据。"
    } else if code.contains("BAD_ARGS") || code.contains("UNKNOWN_TOOL") {
        "这次请求我没处理对，没有可靠的结果。"
    } else {
        "这个我没查到，暂时没有可靠的结果。"
    };
    Some(class.to_string())
}

/// Result of one fully-consumed model call.
struct CallOutcome {
    finish: Finish,
    content: String,
    tool_calls: Vec<crate::llm::WireToolCall>,
    /// R2-BM5: what actually reached the speaker after the claim gate. For
    /// providers that only send terminal content this stays empty and the
    /// whole-answer path sanitizes `content` at the end.
    spoken: String,
    streamed_any: bool,
    suppressed_claims: usize,
    /// A refusal sentence was inserted because the model claimed an action
    /// that no receipt confirmed.
    refusal_sent: bool,
}

/// R2-BM5: the speaker-side gate.
///
/// Model deltas are grouped into the SAME sentence segments the voice layer
/// uses (`take_speak_segment`), so first-sound timing is unchanged: the voice
/// layer already waits for a sentence boundary before synthesizing. Each
/// segment is checked for an unsupported action claim:
/// - a turn WITH a core fact: the claim is dropped (the core already spoke
///   the fact; the model must not restate it);
/// - a turn WITHOUT a fact: the claim is replaced by one honest refusal.
///
/// Questions and negations are not claims (see `facts::asserts_action`).
struct SentenceGate {
    buf: String,
    spoken: String,
    suppressed: usize,
    refusal_sent: bool,
    has_fact: bool,
    allow_followup_prose: bool,
}

impl SentenceGate {
    fn new(has_fact: bool, allow_followup_prose: bool) -> Self {
        Self {
            buf: String::new(),
            spoken: String::new(),
            suppressed: 0,
            refusal_sent: false,
            has_fact,
            allow_followup_prose,
        }
    }

    fn feed(&mut self, text: &str, tx: &mut futures::channel::mpsc::UnboundedSender<AgentEvent>) {
        self.buf.push_str(text);
        if self.buf.contains("[[NEXT_STEP:") {
            return;
        }
        while let Some(seg) = crate::voice::VoicePipeline::take_speak_segment(&mut self.buf) {
            self.emit(seg, tx);
        }
    }

    fn flush(&mut self, tx: &mut futures::channel::mpsc::UnboundedSender<AgentEvent>) {
        let (clean, _) = extract_next_step(&self.buf);
        let residue = clean.trim().to_string();
        self.buf.clear();
        if !residue.is_empty() {
            self.emit(residue, tx);
        }
    }

    fn emit(&mut self, seg: String, tx: &mut futures::channel::mpsc::UnboundedSender<AgentEvent>) {
        if self.has_fact && !self.allow_followup_prose {
            // ENFORCED boundary (R2-BM5, audit Q1): once the core has a
            // confirmed/rejected fact for this turn, the core owns both the
            // action sentence AND the speaker. No model prose is appended —
            // a paraphrase detector can never be made complete, so the
            // boundary is receipt-owned, not lexical.
            self.suppressed += 1;
            return;
        }
        if facts::asserts_action(&seg) {
            self.suppressed += 1;
            if !self.has_fact && !self.refusal_sent {
                self.refusal_sent = true;
                let refusal = facts::UNSUPPORTED_ACTION_REFUSAL;
                let _ = tx.unbounded_send(AgentEvent::Fact {
                    text: refusal.to_string(),
                });
                self.spoken.push_str(refusal);
            }
            return;
        }
        // `take_speak_segment` trims the terminator when it is whitespace
        // (e.g. "你好\n"). Emitting the bare "你好" makes the VOICE layer's
        // own splitter wait for another boundary (audit Q6: double
        // segmentation delayed speech). Re-attach a newline: the voice trims
        // it and speaks the segment now, exactly as it did before the gate.
        let mut out = seg;
        if !out.ends_with(|c| "。！？!?；;\n".contains(c)) {
            out.push('\n');
        }
        let _ = tx.unbounded_send(AgentEvent::Token { text: out.clone() });
        self.spoken.push_str(&out);
    }
}

/// Consume one model-call stream. Emits Token events; applies the
/// first-event timeout. Errors carry whether any Delta already went out
/// (such failures must not be silently retried).
#[allow(clippy::too_many_arguments)]
async fn consume_stream(
    mut stream: crate::llm::LlmStream,
    first_event_timeout: std::time::Duration,
    idle_timeout: std::time::Duration,
    total_timeout: std::time::Duration,
    cancel: &crate::task::CancellationScope,
    tx: &mut futures::channel::mpsc::UnboundedSender<AgentEvent>,
    has_fact: bool,
    allow_followup_prose: bool,
    silent: bool,
) -> Result<CallOutcome, (crate::llm::LlmError, bool)> {
    let mut deltas: Vec<String> = Vec::new();
    let mut gate = SentenceGate::new(has_fact, allow_followup_prose);
    let mut end: Option<(Finish, String, Vec<crate::llm::WireToolCall>)> = None;

    if tx.is_closed() {
        cancel.cancel();
        return Err((
            crate::llm::LlmError::Protocol("client disconnected".into()),
            false,
        ));
    }
    let first = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            return Err((crate::llm::LlmError::Protocol("cancelled".into()), false));
        }
        res = tokio::time::timeout(first_event_timeout, stream.next()) => match res {
            Err(_) => {
                let err = crate::llm::LlmError::Http {
                    status: None,
                    detail: format!("no first SSE event within {first_event_timeout:?}"),
                };
                return Err((err, false));
            }
            Ok(None) => {
                return Err((
                    crate::llm::LlmError::Protocol("stream ended without any event".into()),
                    false,
                ));
            }
            Ok(Some(ev)) => ev,
        }
    };

    let deadline = tokio::time::Instant::now() + total_timeout;
    let mut events = futures::stream::iter(vec![first]).chain(stream);
    loop {
        if tx.is_closed() {
            cancel.cancel();
            return Err((
                crate::llm::LlmError::Protocol("client disconnected".into()),
                !deltas.is_empty() && !silent,
            ));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            if !silent {
                gate.flush(tx);
            }
            return Err((
                crate::llm::LlmError::Http {
                    status: None,
                    detail: format!("LLM stream total timeout exceeded ({total_timeout:?})"),
                },
                !deltas.is_empty() && !silent,
            ));
        }
        let wait = idle_timeout.min(remaining);
        let next_ev = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err((
                    crate::llm::LlmError::Protocol("cancelled".into()),
                    !deltas.is_empty() && !silent,
                ));
            }
            res = tokio::time::timeout(wait, events.next()) => match res {
                Err(_) => {
                    if !silent { gate.flush(tx); }
                    return Err((
                        crate::llm::LlmError::Http {
                            status: None,
                            detail: format!("LLM stream idle timeout after {wait:?}"),
                        },
                        !deltas.is_empty() && !silent,
                    ));
                }
                Ok(ev) => ev,
            }
        };
        let Some(ev) = next_ev else {
            break;
        };
        match ev {
            crate::llm::LlmEvent::Delta(t) => {
                deltas.push(t.clone());
                // A turn whose only tool results FAILED speaks one core line;
                // the model's prose is kept for the record, never streamed.
                if !silent {
                    gate.feed(&t, tx);
                }
            }
            crate::llm::LlmEvent::End {
                finish,
                content,
                tool_calls,
            } => {
                end = Some((finish, content, tool_calls));
                break;
            }
            crate::llm::LlmEvent::Failed(e) => {
                // The sentence gate may be holding a partial sentence the
                // model already produced. A provider failure must not swallow
                // what the person was meant to hear before the failure.
                if !silent {
                    gate.flush(tx);
                }
                let emitted = !deltas.is_empty();
                return Err((crate::llm::LlmError::Protocol(e), emitted));
            }
        }
    }
    match end {
        Some((finish, content, tool_calls)) => {
            if !silent {
                gate.flush(tx);
            }
            let streamed_any = !deltas.is_empty() && !silent;
            let content = if content.is_empty() {
                deltas.concat()
            } else {
                content
            };
            Ok(CallOutcome {
                finish,
                content,
                tool_calls,
                spoken: gate.spoken,
                streamed_any,
                suppressed_claims: gate.suppressed,
                refusal_sent: gate.refusal_sent,
            })
        }
        None => {
            if !silent {
                gate.flush(tx);
            }
            Err((
                crate::llm::LlmError::Protocol("stream ended without a terminal event".into()),
                !deltas.is_empty() && !silent,
            ))
        }
    }
}

/// Secondary LLM (user request 2026-11-10): tried ONCE after the primary
/// gives up (retries exhausted or a non-transient error), and only when no
/// tokens were emitted — a half-streamed answer must never be doubled.
#[derive(Clone)]
pub struct FallbackLlm {
    pub client: Arc<dyn LlmClient>,
    pub model: String,
    /// Provider/model label for traces and logs.
    pub label: String,
}

#[derive(Clone)]
pub struct AgentEnv {
    pub prompt_preferences: prompt::PromptPreferences,
    pub llm: Arc<dyn LlmClient>,
    pub model: String,
    pub llm_fallback: Option<FallbackLlm>,
    pub registry: Arc<Registry>,
    pub sessions: std::sync::Arc<SessionStore>,
    pub tasks: std::sync::Arc<TaskRegistry>,
    pub traces: std::sync::Arc<TraceRecorder>,
    /// 2026-11-13: short household notes. Rendered as one bounded block in the
    /// system message; `None`-empty memory adds nothing to the prompt.
    pub memory: std::sync::Arc<crate::memory::MemoryService>,
    /// M6 (#23): long-term working memory (project paths, progress, active
    /// work context). Separate from household notes and injected across sessions.
    pub working_memory: std::sync::Arc<crate::working_memory::WorkingMemoryStore>,
    /// W3 (#80): optional offline-resilient shared memory cache (0 ms local read).
    pub shared_memory: Option<std::sync::Arc<crate::shared_memory::SharedMemoryService>>,
    /// W4b (#83): optional auditable USER.md profile store.
    pub user_md: Option<std::sync::Arc<crate::user_md::UserMdService>>,
    /// W3 (#78): explicit default scope context when callers do not pass one per-turn.
    pub scope: crate::working_memory::ScopeContext,
    pub assistant_name: String,
    /// Give up on a stream that produced no event within this budget —
    /// guards against the observed "200 then silence" provider failure.
    pub llm_first_event_timeout: std::time::Duration,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Session {
        session_id: String,
    },
    Task {
        task_id: String,
    },
    Token {
        text: String,
    },
    /// R2 (2026-11-13): a sentence the CORE owns — a rendered action fact or
    /// a fail-closed refusal. It travels on its own event so the voice layer
    /// can exempt it from the spoken budget: a length cap must never be the
    /// reason a person does not hear what actually happened.
    Fact {
        text: String,
    },
    ToolStart {
        call_id: String,
        tool: String,
        args: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        tool: String,
        status: String,
        evidence: String,
        summary: String,
        /// Honest execution detail (e.g. the simulated-receipt marker when
        /// no device is connected) — the same string the model sees.
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        late: Option<bool>,
    },
    ConfirmationRequired {
        action: serde_json::Value,
    },
    Answer {
        text: String,
    },
    Cancelled {
        stage: String,
    },
    TurnEnd {
        trace_id: String,
        status: String,
        duration_ms: u64,
    },
    Error {
        message: String,
    },
}

/// Run one conversational turn on the TEXT channel (console, API, tests).
/// A console answer is read, so it is not spoken-budgeted; the voice layer
/// calls [`run_turn_with_delivery`] with the channel it will actually use.
pub fn run_turn(
    env: Arc<AgentEnv>,
    session_id: Option<String>,
    text: String,
    source: String,
) -> impl futures::Stream<Item = AgentEvent> {
    let scope_ctx = env.scope.clone();
    run_turn_with_scope_and_cancellation(
        env,
        session_id,
        text,
        source,
        prompt::Delivery::Text,
        Arc::new(crate::task::CancellationScope::default()),
        scope_ctx,
    )
}

pub fn run_turn_with_scope(
    env: Arc<AgentEnv>,
    session_id: Option<String>,
    text: String,
    source: String,
    scope_ctx: crate::working_memory::ScopeContext,
) -> impl futures::Stream<Item = AgentEvent> {
    run_turn_with_scope_and_cancellation(
        env,
        session_id,
        text,
        source,
        prompt::Delivery::Text,
        Arc::new(crate::task::CancellationScope::default()),
        scope_ctx,
    )
}

/// Run one conversational turn. Returns the event stream; the first events
/// identify the session and task so callers can cancel by id.
///
/// `delivery` is the CHANNEL the answer will be delivered on. It shapes the
/// system prompt (length rule) and nothing else here — the runtime cap lives
/// in the voice layer, and both read the same budget.
pub fn run_turn_with_delivery(
    env: Arc<AgentEnv>,
    session_id: Option<String>,
    text: String,
    source: String,
    delivery: prompt::Delivery,
) -> impl futures::Stream<Item = AgentEvent> {
    let scope_ctx = env.scope.clone();
    run_turn_with_scope_and_cancellation(
        env,
        session_id,
        text,
        source,
        delivery,
        Arc::new(crate::task::CancellationScope::default()),
        scope_ctx,
    )
}

#[cfg(test)]
pub fn run_turn_with_cancellation(
    env: Arc<AgentEnv>,
    session_id: Option<String>,
    text: String,
    source: String,
    delivery: prompt::Delivery,
    cancel: Arc<crate::task::CancellationScope>,
) -> impl futures::Stream<Item = AgentEvent> {
    let scope_ctx = env.scope.clone();
    run_turn_with_scope_and_cancellation(env, session_id, text, source, delivery, cancel, scope_ctx)
}

pub fn run_turn_with_scope_and_cancellation(
    env: Arc<AgentEnv>,
    session_id: Option<String>,
    text: String,
    source: String,
    delivery: prompt::Delivery,
    cancel: Arc<crate::task::CancellationScope>,
    scope_ctx: crate::working_memory::ScopeContext,
) -> impl futures::Stream<Item = AgentEvent> {
    let authority = crate::execution::Authority::internal(env.sessions.store(), scope_ctx);
    run_turn_authorized_with_wake(
        env, session_id, text, source, delivery, cancel, authority, None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn run_turn_with_scope_and_wake(
    env: Arc<AgentEnv>,
    session_id: Option<String>,
    text: String,
    source: String,
    delivery: prompt::Delivery,
    cancel: Arc<crate::task::CancellationScope>,
    scope_ctx: crate::working_memory::ScopeContext,
    wake: Option<(String, String)>,
    lan_exempt: bool,
) -> impl futures::Stream<Item = AgentEvent> {
    let authority = crate::execution::Authority::internal_with_lan_exempt(
        env.sessions.store(),
        scope_ctx,
        lan_exempt,
    );
    run_turn_authorized_with_wake(
        env, session_id, text, source, delivery, cancel, authority, wake,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn run_turn_authorized(
    env: Arc<AgentEnv>,
    session_id: Option<String>,
    text: String,
    source: String,
    delivery: prompt::Delivery,
    cancel: Arc<crate::task::CancellationScope>,
    authority: Result<crate::execution::Authority, String>,
) -> impl futures::Stream<Item = AgentEvent> {
    run_turn_authorized_with_wake(
        env, session_id, text, source, delivery, cancel, authority, None,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_turn_authorized_with_wake(
    env: Arc<AgentEnv>,
    session_id: Option<String>,
    text: String,
    source: String,
    delivery: prompt::Delivery,
    cancel: Arc<crate::task::CancellationScope>,
    authority: Result<crate::execution::Authority, String>,
    wake: Option<(String, String)>,
) -> impl futures::Stream<Item = AgentEvent> {
    let delivery_authority = authority.clone().ok();
    let (tx, rx) = futures::channel::mpsc::unbounded();
    tokio::spawn(async move {
        let mut tx = tx;
        let outcome = match authority {
            Ok(authority) => {
                turn_body(
                    env, session_id, text, source, delivery, cancel, authority, wake, &mut tx,
                )
                .await
            }
            Err(error) => Err(error),
        };
        if let Err(msg) = outcome {
            let _ = tx.unbounded_send(AgentEvent::Error { message: msg });
        }
    });
    use futures::StreamExt;
    rx.filter_map(move |event| {
        let valid = delivery_authority
            .as_ref()
            .is_none_or(|a| a.check().is_ok());
        futures::future::ready(if valid || matches!(event, AgentEvent::Error { .. }) {
            Some(event)
        } else {
            None
        })
    })
}

#[allow(clippy::too_many_arguments)]
async fn turn_body(
    env: Arc<AgentEnv>,
    session_id: Option<String>,
    text: String,
    source: String,
    delivery: prompt::Delivery,
    cancel: Arc<crate::task::CancellationScope>,
    authority: crate::execution::Authority,
    wake: Option<(String, String)>,
    tx: &mut futures::channel::mpsc::UnboundedSender<AgentEvent>,
) -> Result<(), String> {
    authority.check()?;
    let scope_ctx = authority.scope().clone();
    // Session: reuse if it exists, else create.
    let sid = match session_id {
        Some(id) if env.sessions.exists(&id).unwrap_or(false) => id,
        _ => env.sessions.create().map_err(|e| e.to_string())?,
    };
    authority.bind_session(&sid)?;
    let context_ticket = env
        .sessions
        .context_ticket(&sid, authority.epoch())
        .map_err(|e| e.to_string())?;
    let _ = tx.unbounded_send(AgentEvent::Session {
        session_id: sid.clone(),
    });
    let task = env
        .tasks
        .create_with_scope(&sid, cancel)
        .map_err(|e| e.to_string())?;
    let task_id = task.id.clone();
    let _ = tx.unbounded_send(AgentEvent::Task {
        task_id: task_id.clone(),
    });

    let trace = env
        .traces
        .start_with_wake(
            &sid,
            &task_id,
            &source,
            &text,
            wake.as_ref().map(|(ch, _)| ch.as_str()),
            wake.as_ref().map(|(_, w)| w.as_str()),
        )
        .map_err(|e| e.to_string())?;

    let check_authority = || {
        authority.check().inspect_err(|_| {
            env.tasks.request_cancel(&task_id);
            let _ = env.tasks.finish(&task, TaskStatus::Cancelled);
            env.traces.finish(&trace, "cancelled", &text);
        })
    };

    task.commit(|| env.sessions.append(&sid, "user", &text))
        .transpose()
        .map_err(|e| e.to_string())?;

    if env.tasks.has_pending_cross_turn(&sid) && is_cancel_utterance(&text) {
        env.tasks.cancel_cross_turn(&sid);
        env.traces.event(
            &trace,
            "cross_turn_cancelled",
            serde_json::json!({ "session_id": sid, "text": text }),
        );
        let cancel_reply = "好的，已取消后续步骤。".to_string();
        let _ = task
            .commit(|| env.sessions.append(&sid, "assistant", &cancel_reply))
            .transpose()
            .map_err(|e| e.to_string())?;
        let _ = tx.unbounded_send(AgentEvent::Answer { text: cancel_reply });
        env.tasks
            .finish(&task, TaskStatus::Completed)
            .map_err(|e| e.to_string())?;
        env.traces.finish(&trace, "completed", &text);
        let _ = tx.unbounded_send(AgentEvent::TurnEnd {
            trace_id: trace.trace_id.clone(),
            status: "completed".into(),
            duration_ms: id_now_minus(trace.started_ms),
        });
        return Ok(());
    }
    let continuing_cross_turn =
        env.tasks.has_pending_cross_turn(&sid) && is_continue_utterance(&text);

    // Context: system + persisted structured history + this turn's live messages.
    // The capability block is rendered from the registry (R1h/RH-3): adding or
    // removing a service changes this line's output, never prompt.rs's text.
    let mut messages: Vec<WireMessage> = Vec::new();
    let mut system = prompt::system_prompt_with_preferences(
        &env.assistant_name,
        &env.registry.ready_capabilities(),
        &env.registry.unavailable_services(),
        delivery,
        &env.prompt_preferences,
    );
    // Household memory: background the user chose to keep (names, aliases,
    // preferences, important events). Bounded; operational state is never
    // stored here, so it cannot go stale against the capability list.
    if let Some(block) = env.memory.render_for_prompt_scoped(&scope_ctx) {
        system.push_str("\n\n");
        system.push_str(&block);
    }
    if let Some(block) = env.tasks.render_cross_turn_for_prompt(&sid) {
        system.push_str("\n\n");
        system.push_str(&block);
    }
    if continuing_cross_turn {
        if let Some(mut ct) = env.tasks.get_cross_turn(&sid) {
            ct.state = CrossTurnState::Active;
            ct.updated_ms = crate::id::now_ms();
            env.tasks.set_cross_turn(ct);
        }
        env.traces.event(
            &trace,
            "cross_turn_resumed",
            serde_json::json!({ "session_id": sid }),
        );
    }
    // Long-term working memory: per-person / per-device work context that
    // persists across sessions without touching the 200-item household notes.
    if let Some(summary) = env.working_memory.render_for_prompt_scoped(&scope_ctx) {
        system.push_str("\n\n");
        system.push_str(&prompt::working_memory_section(&summary));
    }
    // Cached shared profile summaries from NAS `memory-svc` (0 ms local read).
    if let Some(ref shared) = env.shared_memory {
        if let Some(summary) = shared.render_for_prompt_scoped(&scope_ctx) {
            system.push_str("\n\n");
            system.push_str(&prompt::shared_memory_section(&summary));
        }
    }
    // Auditable USER.md profile & preferences (scoped by ScopeContext).
    if let Some(ref user_md) = env.user_md {
        if let Some(summary) = user_md.render_for_prompt_scoped(&scope_ctx) {
            system.push_str("\n\n");
            system.push_str(&prompt::user_md_section(&summary));
        }
    }
    messages.push(WireMessage::system(system));
    if let Ok(hist) = env.sessions.history(&sid, HISTORY_WINDOW) {
        // The history already includes the user message we just appended.
        messages.extend(hist);
    } else {
        messages.push(WireMessage::user(text.clone()));
    }

    env.traces.event(
        &trace,
        "llm_start",
        serde_json::json!({ "context_messages": messages.len() }),
    );

    let mut final_status = "";
    let mut answer_text;
    // Last tool rendering that could stand in for the answer: (text,
    // authoritative). Authoritative ones are spoken verbatim (A's
    // `replace_answer`), so the device never reads out "let me call a tool".
    let mut rendered_answer: Option<(String, bool)> = None;
    // R2-BM5: facts the CORE has already spoken this turn (structured tool
    // facts rendered by `facts::render_fact`), plus the gate's bookkeeping.
    let mut facts: Vec<crate::tool::ToolFact> = Vec::new();
    let mut facts_confirmed_all = true;
    let mut allow_followup_prose = false;
    let mut saw_strict_fact = false;
    let mut core_said = String::new();
    let mut suppressed_total = 0usize;
    // 2026-11-13 (M2): a turn whose tools ALL failed says ONE core line. The
    // line is remembered here, the model's prose for that round is not
    // streamed (silent), and a later successful retry clears both.
    let mut failure_line: Option<String> = None;
    let mut turn_had_success = false;
    let mut silent_round = false;
    // 2026-11-14: a tool spoke its own authoritative line this turn
    // (`replace_answer`). On a STREAMED turn the final replacement below only
    // fixes the record — the model's tokens would already be in the speaker
    // (reminder.set's computed time was spoken over by the model's 「好的」).
    // So the line is said at receipt time, like an action fact, and the
    // model's later prose stays out of the speaker.
    let mut authoritative_said = false;
    let mut saw_home_action = false;
    let mut saw_non_home_tool = false;
    let mut finished = false;
    let mut turn_error: Option<String> = None;
    let mut tool_next_socket: Option<String> = None;

    let cancel_scope = task.cancellation_scope();
    for call_no in 1..=MAX_MODEL_CALLS {
        check_authority()?;
        if tx.is_closed() && !task.is_cancelled() {
            env.tasks.request_cancel(&task_id);
        }
        if task.is_cancelled() {
            task.note(format!("cancel observed before model call {call_no}"));
            let _ = tx.unbounded_send(AgentEvent::Cancelled {
                stage: format!("before model call {call_no}"),
            });
            final_status = "cancelled";
            finished = true;
            break;
        }

        // One model call with bounded retry on transient provider failures.
        let catalog = env.registry.catalog().map_err(|e| e.to_string())?;
        let tool_specs = catalog.specs();
        let mut call_outcome: Option<CallOutcome> = None;
        let mut llm_gave_up: Option<String> = None;
        let mut gave_up_with_deltas = false;
        for attempt in 1..=MAX_LLM_ATTEMPTS {
            check_authority()?;
            if tx.is_closed() && !task.is_cancelled() {
                env.tasks.request_cancel(&task_id);
            }
            if task.is_cancelled() {
                break;
            }
            let result = match env
                .llm
                .chat_stream(&env.model, messages.clone(), &tool_specs)
                .await
            {
                Ok(stream) => {
                    consume_stream(
                        stream,
                        env.llm_first_event_timeout,
                        STREAM_IDLE_TIMEOUT,
                        STREAM_TOTAL_TIMEOUT,
                        &cancel_scope,
                        tx,
                        !facts.is_empty(),
                        allow_followup_prose && !saw_strict_fact,
                        silent_round || authoritative_said,
                    )
                    .await
                }
                Err(e) => Err((e, false)),
            };
            match result {
                Ok(o) => {
                    call_outcome = Some(o);
                    break;
                }
                Err((err, deltas_emitted)) => {
                    if task.is_cancelled() {
                        break;
                    }
                    let transient = !deltas_emitted && err.is_transient();
                    let retrying = transient && attempt < MAX_LLM_ATTEMPTS;
                    env.traces.event(
                        &trace,
                        if retrying { "llm_retry" } else { "llm_failed" },
                        serde_json::json!({
                            "attempt": attempt, "error": err.to_string(),
                            "transient": transient, "deltas_emitted": deltas_emitted,
                        }),
                    );
                    tracing::warn!(task = %task_id, attempt, error = %err, "llm call failed");
                    if retrying {
                        tokio::select! {
                            biased;
                            _ = cancel_scope.cancelled() => break,
                            _ = tokio::time::sleep(backoff_for(attempt)) => continue,
                        }
                    }
                    llm_gave_up = Some(err.to_string());
                    gave_up_with_deltas = deltas_emitted;
                    break;
                }
            }
        }

        // Secondary provider: one attempt, only when the primary gave up
        // WITHOUT emitting anything (a half-streamed answer must never be
        // doubled by a second provider). One attempt keeps the added
        // worst-case latency bounded; its own transient class is rare
        // enough at this point (the primary already waited out backoffs).
        if call_outcome.is_none() && !gave_up_with_deltas && !task.is_cancelled() {
            if let Some(fb) = env.llm_fallback.as_ref() {
                check_authority()?;
                env.traces.event(
                    &trace,
                    "llm_fallback",
                    serde_json::json!({
                        "provider": fb.label,
                        "primary_error": llm_gave_up,
                    }),
                );
                let result = match fb
                    .client
                    .chat_stream(&fb.model, messages.clone(), &tool_specs)
                    .await
                {
                    Ok(stream) => {
                        consume_stream(
                            stream,
                            env.llm_first_event_timeout,
                            STREAM_IDLE_TIMEOUT,
                            STREAM_TOTAL_TIMEOUT,
                            &cancel_scope,
                            tx,
                            !facts.is_empty(),
                            allow_followup_prose && !saw_strict_fact,
                            silent_round || authoritative_said,
                        )
                        .await
                    }
                    Err(e) => Err((e, false)),
                };
                match result {
                    Ok(o) => call_outcome = Some(o),
                    Err((err, deltas)) => {
                        env.traces.event(
                            &trace,
                            "llm_fallback_failed",
                            serde_json::json!({
                                "provider": fb.label, "error": err.to_string(),
                                "deltas_emitted": deltas,
                            }),
                        );
                        llm_gave_up = Some(format!(
                            "primary: {} / fallback {label}: {err}",
                            llm_gave_up.unwrap_or_default(),
                            label = fb.label
                        ));
                    }
                }
            }
        }

        check_authority()?;
        let Some(outcome) = call_outcome else {
            if task.is_cancelled() {
                let _ = tx.unbounded_send(AgentEvent::Cancelled {
                    stage: format!("during model call {call_no}"),
                });
                final_status = "cancelled";
            } else {
                turn_error = Some(llm_gave_up.unwrap_or_else(|| "model call failed".into()));
            }
            finished = true;
            break;
        };
        let CallOutcome {
            finish,
            content,
            tool_calls,
            spoken,
            streamed_any,
            suppressed_claims,
            refusal_sent,
        } = outcome;
        suppressed_total += suppressed_claims;
        if suppressed_claims > 0 {
            env.traces.event(
                &trace,
                "claim_suppressed",
                serde_json::json!({
                    "call": call_no,
                    "sentences": suppressed_claims,
                    "had_fact": !facts.is_empty(),
                    "action": if refusal_sent { "refused" } else { "dropped" },
                }),
            );
        }

        // Late-output boundary: if cancellation landed while the stream was
        // being consumed, whatever the model produced is a late result —
        // recorded, never finalized into an answer (which would also be
        // spoken on devices). Tokens may already have streamed; that part
        // cannot be unsent and stays visible to whoever was listening.
        if task.is_cancelled() {
            task.note("model output arrived after cancel; answer suppressed");
            env.traces.event(
                &trace,
                "late_model_output",
                serde_json::json!({ "call": call_no, "content_chars": content.chars().count() }),
            );
            let _ = tx.unbounded_send(AgentEvent::Cancelled {
                stage: format!("after model call {call_no}"),
            });
            final_status = "cancelled";
            finished = true;
            break;
        }

        env.traces.event(
            &trace,
            "llm_end",
            serde_json::json!({
                "call": call_no, "finish": format!("{finish:?}"),
                "content_chars": content.chars().count(),
                "tool_calls": tool_calls.len(),
            }),
        );

        if finish == Finish::Stop || tool_calls.is_empty() {
            // R2-BM5: what the model SAID (post-gate). A provider that only
            // sent terminal content never streamed deltas, so sanitize the
            // whole answer here — the claim gate must not depend on transport.
            let model_text = if authoritative_said {
                // Core already emitted the tool's authoritative `rendered_answer`
                // (`reminder.set`, `brain.delegate`, `brain.steer`, `brain.cancel`).
                // Never run `sanitize_whole` over the discarded post-tool model
                // prose — doing so would otherwise trigger `UNSUPPORTED_ACTION_REFUSAL`.
                String::new()
            } else if streamed_any {
                spoken.clone()
            } else {
                let (text, suppressed) = facts::sanitize_whole_with_prose(
                    content.trim(),
                    !facts.is_empty(),
                    allow_followup_prose && !saw_strict_fact,
                );
                suppressed_total += suppressed;
                if suppressed > 0 {
                    env.traces.event(
                        &trace,
                        "claim_suppressed",
                        serde_json::json!({
                            "call": call_no,
                            "sentences": suppressed,
                            "had_fact": !facts.is_empty(),
                            "action": "whole_answer",
                        }),
                    );
                }
                text
            };
            // The gate re-attaches a newline so the VOICE layer speaks a
            // newline-terminated sentence immediately; the persisted Answer
            // and the whole-answer TTS path keep clean text.
            let model_text = model_text.trim().to_string();
            if let Some(line) = failure_line
                .clone()
                .filter(|_| facts.is_empty() && !turn_had_success)
            {
                // M2 (2026-11-13): every tool in this turn failed. The person
                // hears ONE core-owned sentence, not the model's explanation
                // of a missing datum. Speaking it as a Fact also keeps the
                // spoken budget out of it.
                answer_text = line.clone();
                let _ = tx.unbounded_send(AgentEvent::Fact { text: line.clone() });
                env.traces.event(
                    &trace,
                    "answer_rendered",
                    serde_json::json!({
                        "source": "tool_failure",
                        "chars": answer_text.chars().count(),
                        "model_chars_dropped": model_text.chars().count(),
                    }),
                );
                task.note("core-rendered failure line (one sentence)");
            } else if !facts.is_empty() {
                // The action sentence(s) are the CORE's. The model may only
                // add non-action context; its own action claims were dropped.
                // For a pure Home Assistant control turn (`home.set`), the
                // core's rendered fact is the complete answer so trailing
                // model questions ("还需要帮您做什么吗？") do not hold the
                // user in a follow-up exchange.
                answer_text = core_said.clone();
                // A combined request ("关灯并告诉我天气") has a trusted query
                // rendering that is NOT replace_answer (weather). Keeping it
                // here is what stops an unrelated answer from disappearing
                // when model prose is dropped (audit Q6).
                if let Some((text, _)) = rendered_answer.as_ref() {
                    answer_text.push_str(text);
                }
                if !(saw_home_action && !saw_non_home_tool) {
                    answer_text.push_str(&model_text);
                }
                env.traces.event(
                    &trace,
                    "answer_rendered",
                    serde_json::json!({
                        "source": "tool",
                        "facts": facts.len(),
                        "confirmed": facts_confirmed_all,
                        "chars": answer_text.chars().count(),
                        "suppressed_claims": suppressed_total,
                    }),
                );
                task.note(format!("core-rendered facts: {}", facts.len()));
            } else if let Some((text, true)) = rendered_answer.as_ref() {
                env.traces.event(
                    &trace,
                    "answer_rendered",
                    serde_json::json!({ "source": "tool", "chars": text.chars().count() }),
                );
                answer_text = text.clone();
            } else if suppressed_total > 0 {
                // Fail-closed: an action claim without a receipt is not
                // adopted. The refusal is the answer (never the claim).
                answer_text = model_text;
                env.traces.event(
                    &trace,
                    "answer_rendered",
                    serde_json::json!({
                        "source": "refusal",
                        "reason": "unsupported_action_claim",
                        "chars": answer_text.chars().count(),
                        "suppressed_claims": suppressed_total,
                    }),
                );
            } else {
                // Existing precedence: an empty model answer with a rendering
                // to hand, then the model's own words.
                answer_text = model_text;
                if let Some((text, false)) = rendered_answer.as_ref() {
                    if answer_text.is_empty() {
                        env.traces.event(
                            &trace,
                            "answer_rendered",
                            serde_json::json!({ "source": "tool_fallback", "chars": text.chars().count() }),
                        );
                        answer_text = text.clone();
                    }
                }
            }
            let (_, raw_next_socket) = extract_next_step(&content);
            let (clean_answer, answer_next_socket) = extract_next_step(&answer_text);
            answer_text = clean_answer;
            let pending_next = raw_next_socket
                .or(answer_next_socket)
                .or_else(|| tool_next_socket.clone());

            // Last cancellation check (audit Q5): the model output arrived,
            // but a stop can land while the answer is being assembled. A
            // stopped turn must not persist or speak an answer.
            if task.is_cancelled() {
                task.note("cancel landed before the answer was persisted; answer suppressed");
                env.traces.event(
                    &trace,
                    "late_answer_suppressed",
                    serde_json::json!({ "chars": answer_text.chars().count() }),
                );
                let _ = tx.unbounded_send(AgentEvent::Cancelled {
                    stage: "before answer".into(),
                });
                final_status = "cancelled";
                finished = true;
                break;
            }
            if task
                .commit(|| env.sessions.append(&sid, "assistant", &answer_text))
                .transpose()
                .map_err(|e| e.to_string())?
                .is_none()
            {
                final_status = "cancelled";
                finished = true;
                break;
            }
            if let Some(next_step) = pending_next {
                let goal = env
                    .tasks
                    .get_cross_turn(&sid)
                    .filter(|t| continuing_cross_turn && !t.cancelled)
                    .map(|t| t.goal)
                    .unwrap_or_else(|| text.clone());
                env.tasks.begin_cross_turn(
                    &sid,
                    goal.clone(),
                    Some(next_step.clone()),
                    Some(answer_text.clone()),
                    CrossTurnState::AwaitingContinue,
                );
                env.traces.event(
                    &trace,
                    "cross_turn_awaiting_continue",
                    serde_json::json!({ "session_id": sid, "goal": goal, "next_socket": next_step }),
                );
            } else if continuing_cross_turn {
                env.tasks
                    .complete_cross_turn(&sid, Some(answer_text.clone()));
                env.traces.event(
                    &trace,
                    "cross_turn_completed",
                    serde_json::json!({ "session_id": sid }),
                );
            }
            let _ = tx.unbounded_send(AgentEvent::Answer {
                text: answer_text.clone(),
            });
            final_status = "completed";
            finished = true;
            break;
        }

        // Tool-call turn: execute at most MAX_TOOLS_PER_CALL and persist
        // exactly those calls. A call that never ran must not leave a
        // dangling tool_call_id — providers reject the next request, and a
        // truncated turn used to keep every id (R1f).
        let calls: Vec<crate::llm::WireToolCall> = tool_calls
            .iter()
            .take(MAX_TOOLS_PER_CALL)
            .cloned()
            .collect();
        // `content` stays readable for the console; `wire` is what the model
        // sees again (structured tool_calls, never `[调用 …]` prose).
        let public_calls: Vec<_> = calls
            .iter()
            .cloned()
            .map(|mut call| {
                if let Some(tool) = catalog
                    .canonical_name(&call.name)
                    .and_then(|name| catalog.tool(name))
                {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
                        call.arguments = tool.redact_arguments(&args).to_string();
                    }
                }
                call
            })
            .collect();
        let summary: Vec<String> = public_calls
            .iter()
            .map(|c| {
                format!(
                    "[调用 {} {}]",
                    catalog.canonical_name(&c.name).unwrap_or(&c.name),
                    c.arguments
                )
            })
            .collect();
        let calls_wire = WireMessage::assistant_calls(public_calls);
        task.commit(|| {
            env.sessions
                .append_wire(&sid, "assistant", &summary.join(" "), &calls_wire)
        })
        .transpose()
        .map_err(|e| e.to_string())?;
        messages.push(calls_wire);

        // Receipts (incl. failures/unknown/late) persist right after each
        // call, so FOLLOW-UP turns ("刚才放的什么"/"第一首放不出来") are
        // grounded in what actually happened, not in the intent line above.
        let mut answered: Vec<String> = Vec::new();
        for call in &calls {
            if tx.is_closed() && !task.is_cancelled() {
                env.tasks.request_cancel(&task_id);
            }
            if task.is_cancelled() {
                break;
            }
            let call_id = crate::id::next("C");
            // Canonical labels are for UI/facts; dispatch uses the exact
            // advertised wire name and its captured contract revision.
            let tool_name = catalog
                .canonical_name(&call.name)
                .unwrap_or(&call.name)
                .to_string();
            if tool_name == "home.set" || tool_name.starts_with("home.") {
                saw_home_action = true;
            } else {
                saw_non_home_tool = true;
            }
            let args: serde_json::Value =
                serde_json::from_str(&call.arguments).unwrap_or(serde_json::json!({
                    "_parse_error": call.arguments
                }));

            let public_args = catalog
                .tool(&tool_name)
                .map(|tool| tool.redact_arguments(&args))
                .unwrap_or_else(|| args.clone());
            env.traces.event(
                &trace,
                "tool_start",
                serde_json::json!({ "call_id": call_id, "tool": tool_name, "args": public_args }),
            );
            let _ = tx.unbounded_send(AgentEvent::ToolStart {
                call_id: call_id.clone(),
                tool: tool_name.clone(),
                args: public_args,
            });

            let ctx = ToolCtx::new(task_id.clone(), call_id.clone(), task.cancel_flag())
                .with_model_exchange()
                .with_provenance(
                    authority
                        .physical_device_id()
                        .or_else(|| authority.scope().device_id.as_deref())
                        .map(str::to_string),
                    Some(trace.trace_id.clone()),
                )
                .with_task_authority(authority.clone(), task.cancellation_scope());
            let result = env
                .registry
                .call_advertised(&catalog, &call.name, args, &ctx)
                .await;
            check_authority()?;
            #[cfg(test)]
            if result.status == crate::tool::ToolStatus::AwaitingConfirmation {
                ctx.before_confirmation_receipt().await;
                check_authority()?;
            }
            // A tool that owns the presentation of its own fact (A's
            // `replace_answer`): the model must not paraphrase it. Without
            // this the device read out "我来调用工具" around a clock answer
            // (user report 2026-11-12).
            let mut authoritative_line: Option<String> = None;
            if let Some(r) = result.result.as_ref() {
                if let Some(ns) = r
                    .get("next_socket")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    tool_next_socket = Some(ns.to_string());
                }
                if let Some(rendered) = r.get("rendered_answer").and_then(|v| v.as_str()) {
                    let authoritative = r
                        .get("replace_answer")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    rendered_answer = Some((rendered.to_string(), authoritative));
                    if authoritative {
                        authoritative_line = Some(rendered.to_string());
                    }
                }
            }

            let late = task.is_cancelled();
            if late {
                task.note(format!("late result from {tool_name} recorded only"));
            }
            if let (Some(line), false) = (authoritative_line, late) {
                // Said NOW, on the core's own event; the rest of this turn's
                // model prose is not streamed (see `authoritative_said`).
                env.traces.event(
                    &trace,
                    "authoritative_line",
                    serde_json::json!({ "call_id": call_id, "tool": tool_name, "chars": line.chars().count() }),
                );
                let _ = tx.unbounded_send(AgentEvent::Fact { text: line });
                authoritative_said = true;
            }
            // M2 (2026-11-13): success/failure bookkeeping for the one-line
            // failure answer. A result that carries an ACTION FACT is not a
            // query failure — its own sentence is spoken by the fact path.
            if !late {
                match result.status {
                    crate::tool::ToolStatus::Completed | crate::tool::ToolStatus::Partial
                        if !is_empty_result(&result) =>
                    {
                        turn_had_success = true;
                        failure_line = None;
                    }
                    // A completed-but-EMPTY result: the question was asked
                    // and answered with nothing. Not a success — the core
                    // owns the one short line (M7 residual).
                    crate::tool::ToolStatus::Completed | crate::tool::ToolStatus::Partial => {
                        if !turn_had_success {
                            if let Some(line) = empty_say(&result) {
                                failure_line = Some(line);
                            }
                        }
                    }
                    _ if result.fact.is_some() => {}
                    _ if !turn_had_success => {
                        if let Some(line) = failure_say(&result) {
                            failure_line = Some(line);
                        }
                    }
                    _ => {}
                }
            }
            env.traces.event(
                &trace,
                "tool_result",
                serde_json::json!({
                    "call_id": call_id, "tool": tool_name,
                    "status": format!("{:?}", result.status),
                    "evidence": format!("{:?}", result.evidence),
                    "summary": result.summary(),
                    "result": result.result, "error": result.error, "late": late,
                }),
            );
            let _ = tx.unbounded_send(AgentEvent::ToolResult {
                call_id: call_id.clone(),
                tool: tool_name.clone(),
                status: result.status.as_str().to_string(),
                evidence: format!("{:?}", result.evidence).to_lowercase(),
                summary: result.summary(),
                detail: result
                    .result
                    .as_ref()
                    .and_then(|r| r.get("detail"))
                    .and_then(|d| d.as_str())
                    .map(|d| d.to_string()),
                late: late.then_some(true),
            });

            // R2-BM5: the CORE speaks the action fact, immediately. This is
            // earlier than the next model call, so the first thing a person
            // hears in an action turn is the observed result — and the model
            // never gets a chance to phrase it first. Only a confirmed
            // receipt may use「已…」(`facts::render_fact` enforces it).
            if let Some(f) = result.fact.clone() {
                let confirmed = facts::is_confirmed(&result, &f);
                let text = facts::render_fact(&result, &f);
                env.traces.event(
                    &trace,
                    "fact_rendered",
                    serde_json::json!({
                        "call_id": call_id, "tool": tool_name,
                        "subject": f.subject, "action": f.action,
                        "confirmed": confirmed, "text": text, "late": late,
                    }),
                );
                if !late {
                    let tool_allows_prose = result
                        .result
                        .as_ref()
                        .and_then(|r| r.get("allow_followup_prose"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if tool_allows_prose {
                        allow_followup_prose = true;
                    } else {
                        saw_strict_fact = true;
                    }
                    facts.push(f);
                    facts_confirmed_all &= confirmed;
                    core_said.push_str(&text);
                    // The CORE's sentence, on its own event: the voice layer
                    // speaks it outside the spoken budget.
                    let _ = tx.unbounded_send(AgentEvent::Fact { text });
                }
            }

            // The model sees the tool's truth, verbatim.
            let payload = serde_json::to_string(&serde_json::json!({
                "status": result.status.as_str().to_string(),
                "evidence": format!("{:?}", result.evidence).to_lowercase(),
                "summary": result.summary(),
                "result": result.result,
                "error": result.error,
                "fact": result.fact,
            }))
            .unwrap_or_else(|_| "{}".into());
            let tool_wire = WireMessage::tool_result(call.id.clone(), payload);
            messages.push(tool_wire.clone());

            // Cross-turn receipt row: compact, bounded, same truth. The
            // console reads `content`; the model replays `wire`.
            let mut result_json = result
                .result
                .as_ref()
                .map(|r| serde_json::to_string(r).unwrap_or_default())
                .unwrap_or_default();
            truncate_at_char_boundary(&mut result_json, 600);
            let receipt = format!(
                "[回执 {} {}{}]",
                tool_name,
                result.summary(),
                if result_json.is_empty() {
                    String::new()
                } else {
                    format!(" {result_json}")
                },
            );
            task.commit(|| env.sessions.append_wire(&sid, "tool", &receipt, &tool_wire))
                .transpose()
                .map_err(|e| e.to_string())?;
            answered.push(call.id.clone());
            if result.status == crate::tool::ToolStatus::AwaitingConfirmation {
                if let Some(action) = result.result.as_ref().and_then(|r| r.get("action")) {
                    let _ = tx.unbounded_send(AgentEvent::ConfirmationRequired {
                        action: action.clone(),
                    });
                }
                // The pending boundary ends this model/tool batch. Complete
                // protocol history for the unexecuted siblings without effects.
                for skipped in calls.iter().filter(|c| !answered.contains(&c.id)) {
                    task.commit(|| {
                        env.sessions.append_wire(
                            &sid,
                            "tool",
                            "[pending confirmation: not executed]",
                            &WireMessage::tool_result(
                                skipped.id.clone(),
                                r#"{"status":"not_executed","reason":"confirmation_pending"}"#,
                            ),
                        )
                    })
                    .transpose()
                    .map_err(|e| e.to_string())?;
                }
                ctx.complete_confirmation_exchange();
                final_status = "confirmation_required";
                finished = true;
                break;
            }
        }
        if finished && final_status == "confirmation_required" {
            break;
        }

        // The next model call is the one that would ramble about a missing
        // datum: keep its prose out of the speaker while the turn has no
        // success at all. A successful retry clears this again.
        silent_round = !turn_had_success && failure_line.is_some();

        if task.is_cancelled() {
            // A cancelled batch may have executed only part of its calls.
            // Fill the missing tool rows so the assistant's tool_calls never
            // dangle on the next request.
            for call in calls.iter().filter(|c| !answered.contains(&c.id)) {
                let payload = r#"{"status":"cancelled"}"#;
                task.commit(|| {
                    env.sessions.append_wire(
                        &sid,
                        "tool",
                        "[回执 已取消]",
                        &WireMessage::tool_result(call.id.clone(), payload),
                    )
                })
                .transpose()
                .map_err(|e| e.to_string())?;
            }
            task.note("cancel observed after tool batch; no further model call");
            let _ = tx.unbounded_send(AgentEvent::Cancelled {
                stage: "after tool batch".into(),
            });
            final_status = "cancelled";
            finished = true;
            break;
        }
    }
    if !finished && turn_error.is_none() && final_status.is_empty() {
        // Loop exhausted without a final answer: emit an explicit Error event
        // so Web Console and Android clients never display a silent blank turn.
        env.traces
            .event(&trace, "loop_budget_exhausted", serde_json::json!({}));
        turn_error = Some("达到最大工具调用轮数，未能完成回答".into());
        final_status = "failed";
    }
    if let Some(msg) = turn_error.clone() {
        // Runtime failures still finalize the task and trace (never leave
        // them "running") and reach the client as an explicit error event.
        final_status = "failed";
        let _ = tx.unbounded_send(AgentEvent::Error { message: msg });
    }

    let status_for_task = match final_status {
        "completed" => TaskStatus::Completed,
        "confirmation_required" => TaskStatus::AwaitingConfirmation,
        "cancelled" => {
            if continuing_cross_turn {
                env.tasks.cancel_cross_turn(&sid);
            }
            TaskStatus::Cancelled
        }
        _ => TaskStatus::Failed,
    };
    if final_status != "confirmation_required" {
        env.tasks
            .finish_at_epoch(&task, status_for_task, authority.epoch())
            .map_err(|e| e.to_string())?;
    }
    if final_status == "completed" {
        if let Some(ticket) = context_ticket {
            let _ = env
                .sessions
                .complete_context(&sid, ticket, crate::id::now_ms());
        }
    }
    env.traces.finish(&trace, final_status, &text);
    let _ = tx.unbounded_send(AgentEvent::TurnEnd {
        trace_id: trace.trace_id.clone(),
        status: final_status.into(),
        duration_ms: id_now_minus(trace.started_ms),
    });
    Ok(())
}

fn id_now_minus(started: u64) -> u64 {
    crate::id::now_ms().saturating_sub(started)
}

fn normalize_short_utterance(text: &str) -> String {
    text.trim()
        .trim_matches(|c: char| {
            c.is_whitespace()
                || matches!(c, '。' | '！' | '？' | '，' | '、' | '.' | '!' | '?' | ',')
        })
        .to_ascii_lowercase()
}

pub(crate) fn is_continue_utterance(text: &str) -> bool {
    matches!(
        normalize_short_utterance(text).as_str(),
        "继续"
            | "继续吧"
            | "接着"
            | "接着做"
            | "下一步"
            | "继续下一步"
            | "continue"
            | "go on"
            | "next step"
    )
}

pub(crate) fn is_cancel_utterance(text: &str) -> bool {
    matches!(
        normalize_short_utterance(text).as_str(),
        "取消"
            | "算了"
            | "不要了"
            | "不用了"
            | "别做了"
            | "停止"
            | "停"
            | "cancel"
            | "stop"
            | "abort"
    )
}

pub(crate) fn extract_next_step(text: &str) -> (String, Option<String>) {
    const PREFIX: &str = "[[NEXT_STEP:";
    const SUFFIX: &str = "]]";
    let Some(start) = text.find(PREFIX) else {
        return (text.to_string(), None);
    };
    let rest = &text[start + PREFIX.len()..];
    let Some(end_rel) = rest.find(SUFFIX) else {
        return (text.to_string(), None);
    };
    let step = rest[..end_rel].trim().to_string();
    let end = start + PREFIX.len() + end_rel + SUFFIX.len();
    let mut cleaned = String::with_capacity(text.len());
    cleaned.push_str(&text[..start]);
    cleaned.push_str(&text[end..]);
    let cleaned = cleaned.trim().to_string();
    let next = (!step.is_empty()).then_some(step);
    (cleaned, next)
}

/// Truncate to at most `max` BYTES without splitting a UTF-8 character
/// (`String::truncate` panics on a non-boundary — CJK tool results make
/// that a real possibility, and a panic here would kill the turn task
/// silently). Appends an ellipsis when truncation happened.
fn truncate_at_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push('…');
}

// Small helper so tests can wait for a cancel to land.
#[cfg(test)]
pub async fn wait_for(flag: &std::sync::atomic::AtomicBool, ms: u64) {
    for _ in 0..ms {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[cfg(test)]
mod tests;

use futures::StreamExt;
