//! LLM abstraction: wire messages, tool specs, streaming events.
//!
//! The agent loop only depends on `LlmClient`; the concrete implementation
//! (DeepSeek via the OpenAI-compatible API, or the scripted fake in tests)
//! stays behind this interface — framework types never leak further up.

pub mod deepseek;
pub mod fake;

use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WireToolCall {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// JSON-encoded arguments string on the wire.
    #[serde(default)]
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WireMessage {
    pub role: String, // system | user | assistant | tool
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<WireToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl WireMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn assistant_text(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn assistant_calls(calls: Vec<WireToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(calls),
            tool_call_id: None,
        }
    }
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Exact provider-safe name from the Registry catalog; adapters never encode it.
    pub name: String,
    /// Internal metadata, excluded from provider JSON.
    #[serde(skip)]
    pub canonical_name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    /// A fragment of assistant text.
    Delta(String),
    /// Terminal event for one model call.
    End {
        finish: Finish,
        content: String,
        tool_calls: Vec<WireToolCall>,
    },
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    Stop,
    ToolCalls,
}

pub type LlmStream = Pin<Box<dyn Stream<Item = LlmEvent> + Send>>;

#[async_trait::async_trait]
pub trait LlmClient: Send + Sync {
    /// One streaming model call. Implementations must emit zero or more
    /// `Delta` followed by exactly one `End` (or a single `Failed`).
    ///
    /// ToolSpec names and history call names are exact wire values. Returned
    /// WireToolCall names are preserved verbatim and resolved only through the
    /// immutable catalog advertised for this model call, never a string codec.
    async fn chat_stream(
        &self,
        model: &str,
        messages: Vec<WireMessage>,
        tools: &[ToolSpec],
    ) -> Result<LlmStream, LlmError>;
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("http {}: {detail}", status.map(|s| s.to_string()).unwrap_or_else(|| "transport".into()))]
    Http { status: Option<u16>, detail: String },
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("not configured: {0}")]
    NotConfigured(String),
}

impl LlmError {
    /// Transient provider failures worth a bounded retry: transport-level
    /// errors (no status) and 408/429/5xx. 4xx request errors are ours and
    /// will not heal by repeating.
    pub fn is_transient(&self) -> bool {
        match self {
            LlmError::Http { status: None, .. } => true,
            LlmError::Http {
                status: Some(s), ..
            } => *s == 408 || *s == 429 || (500..=599).contains(s),
            _ => false,
        }
    }
}
