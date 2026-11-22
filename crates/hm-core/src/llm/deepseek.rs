//! Minimal OpenAI-compatible streaming chat client (DeepSeek baseline).
//!
//! Hand-rolled on purpose as the "minimal client + own loop" baseline the
//! kernel-research doc requires; rust-genai stays a candidate to evaluate
//! when supplier variance grows. No SDK types leak past `llm/mod.rs`.

use super::{Finish, LlmError, LlmEvent, LlmStream, ToolSpec, WireMessage, WireToolCall};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

#[derive(Clone)]
pub struct DeepSeekClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    /// DeepSeek V4.1 hybrid thinking mode: "disabled" sends
    /// thinking {"type":"disabled"} (first content 1.87s→0.53s measured
    /// 2026-11-09); empty sends nothing (provider default = high effort).
    thinking: String,
    /// Per-chunk read timeout for the streaming body (kernel-selection §7):
    /// the window between consecutive byte chunks, NOT a total cap. Distinct
    /// from the agent loop's first-event timeout — this one guards stalls
    /// that start after the stream has begun.
    read_timeout: Duration,
}

// The client is rebuilt from config every turn, but a fresh reqwest::Client
// drops the connection pool and each turn pays a new TLS handshake — visible
// in first-token latency. One shared client keeps the pool warm; reqwest
// pools per host so config changes (different provider/host) are safe.
static LLM_HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn shared_llm_client() -> reqwest::Client {
    LLM_HTTP
        .get_or_init(|| {
            crate::net::apply_resolver(reqwest::Client::builder())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(180))
                .build()
                .expect("reqwest client")
        })
        .clone()
}

impl DeepSeekClient {
    /// `api_key` may be empty for local OpenAI-compatible servers (oMLX,
    /// ollama…); remote providers will simply 401.
    ///
    /// The per-chunk read timeout defaults to 20 s; override with
    /// `HM_LLM_READ_TIMEOUT_SECS` (invalid values fall back to the default).
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, LlmError> {
        Self::with_thinking(base_url, api_key, "")
    }

    pub fn with_thinking(base_url: &str, api_key: &str, thinking: &str) -> Result<Self, LlmError> {
        let read_timeout = std::env::var("HM_LLM_READ_TIMEOUT_SECS")
            .ok()
            .and_then(|v| match v.parse::<u64>() {
                Ok(secs) if secs > 0 => Some(Duration::from_secs(secs)),
                _ => {
                    tracing::warn!("invalid HM_LLM_READ_TIMEOUT_SECS='{v}', using default 20s");
                    None
                }
            })
            .unwrap_or(Duration::from_secs(20));
        let http = shared_llm_client();
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            read_timeout,
            thinking: thinking.trim().to_string(),
        })
    }

    /// Non-streaming minimal call, used by the connection-test endpoint.
    pub async fn ping(&self, model: &str) -> Result<(String, u64), LlmError> {
        let mut body = json!({
            "model": model,
            "messages": [WireMessage::user("只回复两个字：正常")],
            "max_tokens": 8,
            "stream": false,
        });
        if !self.thinking.is_empty() {
            body["thinking"] = json!({ "type": self.thinking });
        }
        let started = std::time::Instant::now();
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .timeout(Duration::from_secs(45))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Http {
                status: None,
                detail: e.to_string(),
            })?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return Err(LlmError::Http {
                status: Some(status.as_u16()),
                detail: truncate(&detail, 400),
            });
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| LlmError::Protocol(e.to_string()))?;
        let text = v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        Ok((text, started.elapsed().as_millis() as u64))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.into()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// Serialize one message into the OpenAI wire shape. Assistant tool calls
/// must nest as {"type":"function","function":{name,arguments}} with the
/// arguments kept as a JSON-encoded string.
fn wire_body(m: &WireMessage) -> Value {
    let mut v = json!({ "role": m.role });
    if let Some(c) = &m.content {
        v["content"] = json!(c);
    }
    if let Some(calls) = &m.tool_calls {
        v["tool_calls"] = Value::Array(
            calls
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id, "type": "function",
                        "function": {
                            "name": c.name,
                            "arguments": c.arguments,
                        }
                    })
                })
                .collect(),
        );
    }
    if let Some(id) = &m.tool_call_id {
        v["tool_call_id"] = json!(id);
    }
    v
}

fn wire_tool(t: &ToolSpec) -> Value {
    json!({ "type": "function", "function": {
        "name": t.name, "description": t.description, "parameters": t.parameters,
    }})
}

#[async_trait::async_trait]
impl super::LlmClient for DeepSeekClient {
    async fn chat_stream(
        &self,
        model: &str,
        messages: Vec<WireMessage>,
        tools: &[ToolSpec],
    ) -> Result<LlmStream, LlmError> {
        let wire_messages: Vec<Value> = messages.iter().map(wire_body).collect();
        let mut body = json!({ "model": model, "messages": wire_messages, "stream": true });
        if !self.thinking.is_empty() {
            body["thinking"] = json!({ "type": self.thinking });
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.iter().map(wire_tool).collect());
        }
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Http {
                status: None,
                detail: e.to_string(),
            })?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(LlmError::Http {
                status: Some(status.as_u16()),
                detail: truncate(&detail, 400),
            });
        }
        Ok(Box::pin(SseEventStream::new(ReadTimeoutStream::new(
            resp.bytes_stream(),
            self.read_timeout,
        ))))
    }
}

/// Wraps a byte stream with a per-chunk read timeout (genai
/// `stream_read_timeout` semantics): every received chunk resets the window;
/// when the gap exceeds the budget the stream emits one error item instead
/// of hanging until reqwest's 180 s total timeout. The timer only engages
/// while the inner stream is Pending, so non-tokio executors that never idle
/// (plain iterators in unit tests) never construct it.
struct ReadTimeoutStream<S> {
    inner: S,
    timeout: Duration,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S> ReadTimeoutStream<S>
where
    S: futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
{
    fn new(inner: S, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            sleep: None,
        }
    }
}

impl<S> futures::Stream for ReadTimeoutStream<S>
where
    S: futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
{
    type Item = Result<bytes::Bytes, String>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.sleep = None; // activity: restart the window
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e.to_string()))),
            Poll::Ready(None) => {
                self.sleep = None;
                Poll::Ready(None)
            }
            Poll::Pending => {
                let timeout = self.timeout;
                let sleep = self
                    .sleep
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
                match sleep.as_mut().poll(cx) {
                    Poll::Ready(()) => {
                        self.sleep = None;
                        Poll::Ready(Some(Err(format!(
                            "read timeout: no SSE bytes for {timeout:?} (stalled mid-stream)"
                        ))))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}

/// A `Stream<Item = LlmEvent>` over an SSE byte stream: buffers bytes,
/// extracts `data:` lines, accumulates content/tool-call deltas, and emits
/// exactly one terminal `End` (on `[DONE]` or upstream EOF).
struct SseEventStream<S> {
    inner: S,
    buffer: String,
    content: String,
    accum: ToolCallAccum,
    finish: Option<Finish>,
    ended: bool,
    end_emitted: bool,
}

impl<S, E> SseEventStream<S>
where
    S: futures::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: String::new(),
            content: String::new(),
            accum: ToolCallAccum::default(),
            finish: None,
            ended: false,
            end_emitted: false,
        }
    }

    /// Try to pull one actionable event from buffered lines.
    /// `Some(None)` = need more bytes; `Some(Some(ev))` = event ready.
    fn poll_event(&mut self) -> Option<Option<LlmEvent>> {
        loop {
            if self.end_emitted {
                return None; // stream finished
            }
            let line = match next_data_line(&mut self.buffer) {
                Some(l) => l,
                None => {
                    return if self.ended {
                        self.end_emitted = true;
                        Some(Some(LlmEvent::End {
                            finish: self.finish.unwrap_or(Finish::Stop),
                            content: std::mem::take(&mut self.content),
                            tool_calls: std::mem::take(&mut self.accum).finish(),
                        }))
                    } else {
                        Some(None) // need more bytes
                    };
                }
            };
            if line == "[DONE]" {
                self.end_emitted = true;
                return Some(Some(LlmEvent::End {
                    finish: self.finish.unwrap_or(Finish::Stop),
                    content: std::mem::take(&mut self.content),
                    tool_calls: std::mem::take(&mut self.accum).finish(),
                }));
            }
            match parse_chunk(&line, &mut self.content, &mut self.accum, &mut self.finish) {
                ChunkAction::Delta(text) => return Some(Some(LlmEvent::Delta(text))),
                ChunkAction::Silent => continue,
            }
        }
    }
}

impl<S, E> futures::Stream for SseEventStream<S>
where
    S: futures::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    type Item = LlmEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(ev) = self.poll_event() {
                if let Some(ev) = ev {
                    return Poll::Ready(Some(ev));
                }
                // else: fall through to read more bytes
            } else {
                return Poll::Ready(None);
            }
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    self.buffer.push_str(&String::from_utf8_lossy(&bytes));
                }
                Poll::Ready(Some(Err(e))) => {
                    self.end_emitted = true;
                    return Poll::Ready(Some(LlmEvent::Failed(e.to_string())));
                }
                Poll::Ready(None) => {
                    self.ended = true;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Accumulates streaming tool-call fragments (indexed; arguments split
/// across chunks) into complete calls.
#[derive(Default)]
struct ToolCallAccum {
    calls: Vec<(usize, String, String, String)>, // index, id, name, args
}

impl ToolCallAccum {
    fn add(&mut self, idx: usize, id: Option<&str>, name: Option<&str>, args: &str) {
        if let Some(slot) = self.calls.iter_mut().find(|(i, ..)| *i == idx) {
            if let Some(id) = id {
                if slot.1.is_empty() {
                    slot.1 = id.into();
                }
            }
            if let Some(name) = name {
                if slot.2.is_empty() {
                    slot.2 = name.into();
                }
            }
            slot.3.push_str(args);
        } else {
            self.calls.push((
                idx,
                id.unwrap_or("").into(),
                name.unwrap_or("").into(),
                args.into(),
            ));
        }
    }

    fn finish(&mut self) -> Vec<WireToolCall> {
        let mut v: Vec<WireToolCall> = std::mem::take(&mut self.calls)
            .into_iter()
            .enumerate()
            .map(|(order, (i, id, name, args))| WireToolCall {
                id: if id.is_empty() {
                    format!("call_auto_{i}_{order}")
                } else {
                    id
                },
                name,
                arguments: args,
            })
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }
}

enum ChunkAction {
    Silent,
    Delta(String),
}

/// Pulls one complete `data: ...` payload out of the buffer, if any.
fn next_data_line(buffer: &mut String) -> Option<String> {
    loop {
        let nl = buffer.find('\n')?;
        let line = buffer[..nl].trim_end_matches('\r').to_string();
        buffer.replace_range(..=nl, "");
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.trim();
            if rest.is_empty() {
                continue; // `data:` keep-alive
            }
            return Some(rest.to_string());
        }
        // SSE comments (`: ping`) and blank lines are skipped.
    }
}

fn parse_chunk(
    data: &str,
    content: &mut String,
    accum: &mut ToolCallAccum,
    finish: &mut Option<Finish>,
) -> ChunkAction {
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return ChunkAction::Silent,
    };
    let choice = &v["choices"][0];
    let delta = &choice["delta"];
    if let Some(text) = delta["content"].as_str() {
        if !text.is_empty() {
            content.push_str(text);
            return ChunkAction::Delta(text.to_string());
        }
    }
    if let Some(tcs) = delta["tool_calls"].as_array() {
        for tc in tcs {
            let idx = tc["index"].as_u64().unwrap_or(0) as usize;
            accum.add(
                idx,
                tc["id"].as_str(),
                tc["function"]["name"].as_str(),
                tc["function"]["arguments"].as_str().unwrap_or(""),
            );
        }
    }
    match choice["finish_reason"].as_str() {
        Some("tool_calls") => *finish = Some(Finish::ToolCalls),
        Some("stop") => *finish = Some(Finish::Stop),
        _ => {}
    }
    ChunkAction::Silent
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn catalog_wire_names_and_call_ids_are_serialized_without_rewriting() {
        let name = "hm_0123456789abcdef_some_wire_id";
        let tool = ToolSpec {
            name: name.into(),
            canonical_name: "family_light.start".into(),
            description: "fixture".into(),
            parameters: json!({"type":"object"}),
        };
        assert_eq!(wire_tool(&tool)["function"]["name"], name);
        assert!(wire_tool(&tool)["function"].get("canonical_name").is_none());
        let message = WireMessage::assistant_calls(vec![WireToolCall {
            id: "exact-call-id".into(),
            name: name.into(),
            arguments: "{}".into(),
        }]);
        let body = wire_body(&message);
        assert_eq!(body["tool_calls"][0]["function"]["name"], name);
        assert_eq!(body["tool_calls"][0]["id"], "exact-call-id");
        assert_eq!(
            wire_body(&WireMessage::tool_result("exact-call-id", "done"))["tool_call_id"],
            "exact-call-id"
        );
    }

    fn byte_stream(
        chunks: Vec<String>,
    ) -> impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin {
        futures::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok(bytes::Bytes::from(c.to_string()))),
        )
    }

    fn drive(
        s: SseEventStream<impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin>,
    ) -> Vec<LlmEvent> {
        futures::executor::block_on_stream(s).collect()
    }

    #[test]
    fn data_line_parsing_skips_comments() {
        let mut buf = String::from(": keepalive\n\ndata: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(next_data_line(&mut buf).as_deref(), Some("{\"a\":1}"));
        assert_eq!(next_data_line(&mut buf).as_deref(), Some("[DONE]"));
        assert_eq!(next_data_line(&mut buf), None);
    }

    #[test]
    fn stream_text_then_done() {
        let s = SseEventStream::new(byte_stream(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n".into(),
            "data: {\"choices\":[{\"delta\":{\"content\":\"好\"},\"finish_reason\":\"stop\"}]}\n\n"
                .into(),
            "data: [DONE]\n\n".into(),
        ]));
        let events = drive(s);
        assert_eq!(events[0], LlmEvent::Delta("你".into()));
        assert_eq!(events[1], LlmEvent::Delta("好".into()));
        match &events[2] {
            LlmEvent::End {
                finish,
                content,
                tool_calls,
            } => {
                assert_eq!(*finish, Finish::Stop);
                assert_eq!(content, "你好");
                assert!(tool_calls.is_empty());
            }
            other => panic!("expected End, got {other:?}"),
        }
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn stream_tool_calls_split_across_chunks() {
        let s = SseEventStream::new(byte_stream(vec![
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"music.play\",\"arguments\":\"{\\\"a\\\":\"}}]}}]}\n\n".into(),
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n".into(),
            "data: [DONE]\n\n".into(),
        ]));
        let events = drive(s);
        match events.last().unwrap() {
            LlmEvent::End {
                finish, tool_calls, ..
            } => {
                assert_eq!(*finish, Finish::ToolCalls);
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "music.play");
                assert_eq!(tool_calls[0].arguments, "{\"a\":1}");
            }
            other => panic!("expected End, got {other:?}"),
        }
    }

    #[test]
    fn eof_without_done_marker_still_ends() {
        let s = SseEventStream::new(byte_stream(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".into(),
        ]));
        let events = drive(s);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1], LlmEvent::End { .. }));
    }

    #[test]
    fn chunk_split_mid_line() {
        let s = SseEventStream::new(byte_stream(vec![
            "data: {\"choices\":[{\"delta\":{\"conte".into(),
            "nt\":\"拼\"}}]}\n\ndata: [DONE]\n\n".into(),
        ]));
        let events = drive(s);
        assert_eq!(events[0], LlmEvent::Delta("拼".into()));
        assert!(matches!(events[1], LlmEvent::End { .. }));
    }

    /// Kernel-selection §7 regression: first event arrives, then the
    /// upstream goes silent forever. The stream must FAIL at exactly the
    /// read-timeout budget instead of hanging until the 180 s total timeout.
    #[tokio::test(start_paused = true)]
    async fn read_timeout_fails_stalled_stream_after_first_event() {
        let first = "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n";
        let stalled = futures::stream::iter(vec![Ok(bytes::Bytes::from(first.to_string()))])
            .chain(futures::stream::pending::<reqwest::Result<bytes::Bytes>>());
        let s = SseEventStream::new(ReadTimeoutStream::new(stalled, Duration::from_secs(20)));
        let started = tokio::time::Instant::now();
        let events: Vec<LlmEvent> = s.collect().await;
        assert_eq!(
            events.len(),
            2,
            "Delta then terminal Failed, got {events:?}"
        );
        assert_eq!(events[0], LlmEvent::Delta("你".into()));
        match &events[1] {
            LlmEvent::Failed(msg) => assert!(msg.contains("read timeout"), "msg: {msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(20),
            "must fire at the budget, not earlier/later"
        );
    }

    /// Gaps shorter than the window reset it: a slow-but-alive stream
    /// (5 s gaps, 20 s budget) completes normally and never times out.
    #[tokio::test(start_paused = true)]
    async fn read_timeout_resets_on_each_chunk() {
        let chunks = vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\n",
            "data: [DONE]\n\n",
        ];
        let slow = futures::stream::iter(chunks).then(|c| async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok::<_, reqwest::Error>(bytes::Bytes::from(c))
        });
        let s = SseEventStream::new(ReadTimeoutStream::new(
            Box::pin(slow),
            Duration::from_secs(20),
        ));
        let events: Vec<LlmEvent> = s.collect().await;
        assert_eq!(events.len(), 3, "two Deltas then End, got {events:?}");
        assert!(matches!(events[2], LlmEvent::End { .. }));
        assert!(!events.iter().any(|e| matches!(e, LlmEvent::Failed(_))));
    }
}
