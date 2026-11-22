//! Scripted LLM client for deterministic tests.
//!
//! `chat_stream` pops the next scripted response in order and records every
//! request it saw, so tests can assert on the exact context the loop built
//! (e.g. "the story revision request still contains the first story").

use super::{Finish, LlmError, LlmEvent, LlmStream, ToolSpec, WireMessage, WireToolCall};
use futures::StreamExt;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub enum Scripted {
    /// Stream text in the given chunks, then end with `stop`.
    Text { chunks: Vec<String> },
    /// Propose tool calls (name, JSON args), then end with `tool_calls`.
    Calls { calls: Vec<(String, String)> },
    /// Stream some text and propose calls in the same turn.
    TextThenCalls {
        chunks: Vec<String>,
        calls: Vec<(String, String)>,
    },
    /// `chat_stream` itself returns a transient (500) request error.
    RequestError { message: String },
    /// The stream opens, then fails with `Failed` (e.g. mid-protocol error).
    Fail { message: String },
    /// Emit one Delta, then fail — must NOT be retried (client saw text).
    DeltaThenFail { chunk: String, message: String },
    /// Stay silent for `delay_ms`, then stream text normally — used with a
    /// short first-event timeout to test the 200-then-hang failure mode.
    Delayed { delay_ms: u64, chunks: Vec<String> },
    /// Stay silent for `delay_ms`, then propose tool calls. Needed by the
    /// waiting-cue mode tests (R1g/P4): the cue must still be live when a
    /// media play starts, which requires the model to be slower than the cue.
    DelayedThenCalls {
        delay_ms: u64,
        calls: Vec<(String, String)>,
    },
    /// Stream one chunk every `delay_ms` — models a real LLM still
    /// producing sentences while the device is already speaking (the
    /// D/T5 stutter window: a barge-in between two sentences).
    TextPaced { chunks: Vec<String>, delay_ms: u64 },
}

#[derive(Default)]
pub struct FakeLlmInner {
    pub script: Vec<Scripted>,
    pub cursor: usize,
    /// Every request's messages, in call order (for assertions).
    pub seen_requests: Vec<Vec<WireMessage>>,
    /// Model names seen.
    pub seen_models: Vec<String>,
}

#[derive(Clone, Default)]
pub struct FakeLlm {
    pub inner: Arc<Mutex<FakeLlmInner>>,
}

impl FakeLlm {
    pub fn new(script: Vec<Scripted>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(FakeLlmInner {
                script,
                cursor: 0,
                ..Default::default()
            })),
        }
    }

    pub fn seen_requests(&self) -> Vec<Vec<WireMessage>> {
        crate::lock_or_recover(&self.inner, "fake_llm.inner")
            .seen_requests
            .clone()
    }
}

fn to_wire_calls(calls: Vec<(String, String)>, tools: &[ToolSpec]) -> Vec<WireToolCall> {
    calls
        .into_iter()
        .enumerate()
        .map(|(i, (name, args))| WireToolCall {
            id: format!("fake_{i}"),
            // Scripts use readable canonical labels; act like a provider that
            // selects the exact advertised name. Unknown labels stay unknown.
            name: tools
                .iter()
                .find(|t| t.canonical_name == name)
                .map(|t| t.name.clone())
                .unwrap_or(name),
            arguments: args,
        })
        .collect()
}

#[async_trait::async_trait]
impl super::LlmClient for FakeLlm {
    async fn chat_stream(
        &self,
        model: &str,
        messages: Vec<WireMessage>,
        tools: &[ToolSpec],
    ) -> Result<LlmStream, LlmError> {
        let scripted = {
            let mut g = crate::lock_or_recover(&self.inner, "fake_llm.inner");
            g.seen_requests.push(messages);
            g.seen_models.push(model.to_string());
            let s = g.script.get(g.cursor).cloned();
            g.cursor += 1;
            s
        };
        if let Some(Scripted::RequestError { message }) = &scripted {
            return Err(super::LlmError::Http {
                status: Some(500),
                detail: message.clone(),
            });
        }
        let events: Vec<LlmEvent> = match scripted {
            // RequestError returns early above; keep the pattern total anyway.
            Some(Scripted::RequestError { .. }) => vec![],
            Some(Scripted::Text { chunks }) => {
                let mut evs: Vec<LlmEvent> = chunks.iter().cloned().map(LlmEvent::Delta).collect();
                evs.push(LlmEvent::End {
                    finish: Finish::Stop,
                    content: chunks.concat(),
                    tool_calls: vec![],
                });
                evs
            }
            Some(Scripted::Calls { calls }) => {
                vec![LlmEvent::End {
                    finish: Finish::ToolCalls,
                    content: String::new(),
                    tool_calls: to_wire_calls(calls, tools),
                }]
            }
            Some(Scripted::DelayedThenCalls { delay_ms, calls }) => {
                let calls = to_wire_calls(calls, tools);
                let s = futures::stream::once(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    LlmEvent::End {
                        finish: Finish::ToolCalls,
                        content: String::new(),
                        tool_calls: calls,
                    }
                });
                return Ok(Box::pin(s));
            }
            Some(Scripted::TextThenCalls { chunks, calls }) => {
                let mut evs: Vec<LlmEvent> = chunks.iter().cloned().map(LlmEvent::Delta).collect();
                evs.push(LlmEvent::End {
                    finish: Finish::ToolCalls,
                    content: chunks.concat(),
                    tool_calls: to_wire_calls(calls, tools),
                });
                evs
            }
            Some(Scripted::Fail { message }) => vec![LlmEvent::Failed(message)],
            Some(Scripted::DeltaThenFail { chunk, message }) => {
                vec![LlmEvent::Delta(chunk), LlmEvent::Failed(message)]
            }
            Some(Scripted::TextPaced { chunks, delay_ms }) => {
                let content = chunks.concat();
                let q: std::collections::VecDeque<String> = chunks.into_iter().collect();
                let paced = futures::stream::unfold(Some(q), move |state| async move {
                    let mut q = state?;
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    let head = q.pop_front()?;
                    let next = if q.is_empty() { None } else { Some(q) };
                    Some((LlmEvent::Delta(head), next))
                });
                let s = paced.chain(futures::stream::once(async move {
                    LlmEvent::End {
                        finish: Finish::Stop,
                        content,
                        tool_calls: vec![],
                    }
                }));
                return Ok(Box::pin(s));
            }
            Some(Scripted::Delayed { delay_ms, chunks }) => {
                // First delta arrives only after the delay; then the rest.
                let content = chunks.concat();
                let mut it = chunks.into_iter();
                let head = it.next().unwrap_or_default();
                let rest: Vec<LlmEvent> = it.map(LlmEvent::Delta).collect();
                let s = futures::stream::once(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    LlmEvent::Delta(head)
                })
                .chain(futures::stream::iter(rest))
                .chain(futures::stream::once(async move {
                    LlmEvent::End {
                        finish: Finish::Stop,
                        content,
                        tool_calls: vec![],
                    }
                }));
                return Ok(Box::pin(s));
            }
            None => vec![LlmEvent::Failed("fake script exhausted".into())],
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}
