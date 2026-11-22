//! Spike B: `genai` 0.7.3 behind Home Mate's real `LlmClient` trait.
//!
//! This is the swap-feasibility artifact: if this adapter compiles, satisfies
//! the trait contract (zero or more `Delta` then exactly one `End`, or a
//! single `Failed`), and behaves on the live bigmodel endpoint, then replacing
//! hm-core's own client is bounded by this file's size. Nothing in hm-core
//! changes; the spike path-depends on the crate.
//!
//! Model namespace note (from the research report, verified live): bare
//! `glm-*` routes to the Z.ai adapter; bigmodel must be addressed as
//! `bigmodel::<model>` — `GenaiLlmClient::model_spec()` owns that mapping.

use futures::{Stream, StreamExt};
use genai::chat::{
    ChatMessage, ChatOptions, ChatStreamEvent, StreamEnd, Tool, ToolResponse,
};
use genai::Client;
use hm_core::llm::{
    Finish, LlmClient, LlmError, LlmEvent, LlmStream, ToolSpec, WireMessage, WireToolCall,
    from_wire_tool_name, to_wire_tool_name,
};
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct GenaiLlmClient {
    client: Client,
    /// e.g. "bigmodel::glm-4-flash" — adapter namespace is decided here, once.
    model_spec: String,
    options: ChatOptions,
    /// Overrides namespace resolution entirely (wire-mock replay in the
    /// comparison harness): a fixed endpoint+auth+model triple.
    fixed_target: Option<genai::ServiceTarget>,
}

impl GenaiLlmClient {
    /// `model` is the bare model id ("glm-4-flash"); the adapter namespace is
    /// prepended here so callers never need to know genai's naming rules.
    /// Falls back to `genai::Client::builder().build()`; failure is a
    /// configuration/resolver problem the caller should surface, not panic on.
    pub fn new(namespace: &str, model: &str) -> Result<Self, String> {
        let client = Client::builder().build().map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            model_spec: format!("{namespace}::{model}"),
            options: ChatOptions::default()
                .with_capture_content(true)
                .with_capture_tool_calls(true),
            fixed_target: None,
        })
    }

    pub fn with_timeouts(mut self, header: std::time::Duration, read: std::time::Duration) -> Self {
        self.options = self
            .options
            .clone()
            .with_stream_header_timeout(header)
            .with_stream_read_timeout(read);
        self
    }

    /// Pin every call to one endpoint/auth/model (used by the replay harness).
    pub fn with_service_target(mut self, target: genai::ServiceTarget) -> Self {
        self.fixed_target = Some(target);
        self
    }

    fn model_spec(model: &str) -> String {
        // The trait passes the configured model through; if it already carries
        // a namespace keep it, else use the client's default namespace.
        if model.contains("::") {
            model.to_owned()
        } else {
            let ns = self_namespace();
            format!("{ns}::{model}")
        }
    }
}

/// Display plus the full source chain — genai's top-level Display omits causes.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(" <- ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

// Helper because `Self::model_spec` shadows in associated-fn vs method context.
fn self_namespace() -> &'static str {
    "bigmodel"
}

fn map_message(msg: &WireMessage) -> ChatMessage {
    match msg.role.as_str() {
        "system" => ChatMessage::system(msg.content.clone().unwrap_or_default()),
        "user" => ChatMessage::user(msg.content.clone().unwrap_or_default()),
        "assistant" => match (&msg.content, &msg.tool_calls) {
            (Some(text), None) => ChatMessage::assistant(text.clone()),
            (_, Some(calls)) => ChatMessage::from(
                calls
                    .iter()
                    .map(|c| genai::chat::ToolCall {
                        call_id: c.id.clone(),
                        // genai puts fn_name on the wire verbatim; history holds
                        // internal dotted names, so this direction maps TO the
                        // wire form (review P1: from_wire was the wrong way —
                        // providers would have seen "music.search").
                        fn_name: to_wire_tool_name(&c.name),
                        fn_arguments: serde_json::from_str(&c.arguments)
                            .unwrap_or(serde_json::Value::Null),
                        thought_signatures: None,
                    })
                    .collect::<Vec<_>>(),
            ),
            (None, None) => ChatMessage::assistant(String::new()),
        },
        "tool" => ChatMessage::from(ToolResponse {
            call_id: msg.tool_call_id.clone().unwrap_or_default(),
            fn_name: None,
            content: msg.content.clone().unwrap_or_default().into(),
        }),
        other => ChatMessage::user(format!("[unknown role {other}] {}", msg.content.clone().unwrap_or_default())),
    }
}

fn map_tool(spec: &ToolSpec) -> Tool {
    Tool::new(to_wire_tool_name(&spec.name))
        .with_description(spec.description.clone())
        .with_schema(spec.parameters.clone())
}

fn finish_of(end: &StreamEnd) -> Finish {
    if end.captured_tool_calls().map(|c| !c.is_empty()).unwrap_or(false) {
        Finish::ToolCalls
    } else {
        Finish::Stop
    }
}

#[async_trait::async_trait]
impl LlmClient for GenaiLlmClient {
    async fn chat_stream(
        &self,
        model: &str,
        messages: Vec<WireMessage>,
        tools: &[ToolSpec],
    ) -> Result<LlmStream, LlmError> {
        let spec = match &self.fixed_target {
            Some(target) => genai::ModelSpec::from(target.clone()),
            None => genai::ModelSpec::from(Self::model_spec(model)),
        };
        let mut system = None;
        let mut chat_messages = Vec::with_capacity(messages.len());
        for m in &messages {
            if m.role == "system" {
                system = Some(m.content.clone().unwrap_or_default());
            } else {
                chat_messages.push(map_message(m));
            }
        }
        let mut req = genai::chat::ChatRequest::new(chat_messages)
            .with_tools(tools.iter().map(map_tool).collect::<Vec<_>>());
        if let Some(sys) = system {
            req = req.with_system(sys);
        }

        let resp = self
            .client
            .exec_chat_stream(spec, req, Some(&self.options))
            .await
            .map_err(|e| LlmError::Http {
                // Review P2: fill the HTTP status so `is_transient()` keeps its
                // meaning — mapping everything to status:None would make 4xx
                // request errors retryable by the agent loop.
                status: e.status().map(|s| s.as_u16()),
                detail: error_chain(&e),
            })?;

        Ok(Box::pin(GenaiStream {
            inner: resp.stream,
            text: String::new(),
            done: false,
        }))
    }
}

/// Adapter stream enforcing the LlmClient contract on top of genai events.
pub struct GenaiStream {
    inner: genai::chat::ChatStream,
    text: String,
    done: bool,
}

impl Stream for GenaiStream {
    type Item = LlmEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        loop {
            match self.inner.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    // genai always ends with End or an error; if the stream died
                    // without either, surface it as a failure rather than silence.
                    self.done = true;
                    return Poll::Ready(Some(LlmEvent::Failed(
                        "genai stream ended without End".into(),
                    )));
                }
                // Events we skip must not yield Pending (no waker would be
                // registered) — loop and poll the inner stream again.
                Poll::Ready(Some(Ok(ChatStreamEvent::Start))) => continue,
                Poll::Ready(Some(Ok(ChatStreamEvent::ReasoningChunk(_)))) => continue,
                Poll::Ready(Some(Ok(ChatStreamEvent::ThoughtSignatureChunk(_)))) => continue,
                Poll::Ready(Some(Ok(ChatStreamEvent::Heartbeat))) => continue,
                Poll::Ready(Some(Ok(ChatStreamEvent::ToolCallChunk(_)))) => {
                    // Complete calls are taken from StreamEnd; chunks are redundant
                    // for the contract (LlmEvent has no partial-tool event).
                    continue;
                }
                Poll::Ready(Some(Ok(ChatStreamEvent::Chunk(c)))) => {
                    self.text.push_str(&c.content);
                    return Poll::Ready(Some(LlmEvent::Delta(c.content)));
                }
                Poll::Ready(Some(Ok(ChatStreamEvent::End(end)))) => {
                    self.done = true;
                    let calls: Vec<WireToolCall> = end
                        .captured_tool_calls()
                        .into_iter()
                        .flatten()
                        .map(|c| WireToolCall {
                            id: c.call_id.clone(),
                            // genai returns the wire name (as advertised);
                            // map it back to the internal dotted name.
                            name: from_wire_tool_name(&c.fn_name),
                            arguments: c.fn_arguments.to_string(),
                        })
                        .collect();
                    let event = LlmEvent::End {
                        finish: finish_of(&end),
                        content: std::mem::take(&mut self.text),
                        tool_calls: calls,
                    };
                    return Poll::Ready(Some(event));
                }
                Poll::Ready(Some(Err(e))) => {
                    self.done = true;
                    return Poll::Ready(Some(LlmEvent::Failed(error_chain(&e))));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::MessageContent;

    /// Review P1: genai serializes `fn_name` verbatim onto the wire, so the
    /// history direction (internal dotted name -> genai ToolCall) must produce
    /// the wire name. The earlier `from_wire_tool_name` here sent "music.search"
    /// to providers while advertising "music_search" — caught by review, not by
    /// the live run (bigmodel tolerated the dotted name).
    #[test]
    fn history_tool_calls_carry_wire_names() {
        let msg = WireMessage::assistant_calls(vec![WireToolCall {
            id: "call_1".into(),
            name: "music.search".into(), // internal dotted name, as stored in sessions
            arguments: r#"{"query":"晴天"}"#.into(),
        }]);
        let chat = map_message(&msg);
        let calls = chat.content.tool_calls();
        let call = calls.first().expect("tool call part");
        assert_eq!(call.fn_name, "music_search", "fn_name goes on the wire verbatim; must be the wire name");
    }
}
