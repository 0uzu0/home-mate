//! The three model services (LLM / ASR / TTS) and household device labels.
//!
//! `ServicesConfig` is the shape persisted under `CONFIG_KEY`; a key is never
//! part of it (secrets live in the secrets table, write-only).

use serde::{Deserialize, Serialize};

/// Household labels for devices (console-owned metadata): display name and
/// room. Stored separately from runtime state so an offline device keeps its
/// name, and included in config export/import and history restore.
pub const DEVICE_META_KEY: &str = "device_meta";

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct DeviceMeta {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub room: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServicesConfig {
    #[serde(default)]
    pub llm: ServiceConfig,
    /// Secondary LLM (user request 2026-11-10: DeepSeek primary + GLM free
    /// tier as fallback): one attempt after the primary's retries give up,
    /// only when no tokens were emitted. Unconfigured = no fallback.
    #[serde(default)]
    pub llm_fallback: ServiceConfig,
    #[serde(default)]
    pub asr: ServiceConfig,
    #[serde(default)]
    pub tts: ServiceConfig,
}

pub(super) fn default_thinking() -> String {
    "disabled".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ServiceConfig {
    /// e.g. "deepseek" for llm, "unconfigured" until the user picks one.
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    /// Environment variable consulted when no stored key exists.
    #[serde(default)]
    pub api_key_env: String,
    /// Thinking/reasoning mode (llm only, DeepSeek V4.1 hybrid): "disabled"
    /// sends thinking {"type":"disabled"} — first content token 1.87s→0.53s
    /// (measured 2026-11-09); "" sends nothing (provider default = high).
    #[serde(default = "default_thinking")]
    pub thinking: String,
    /// Voice/speaker id — meaningful for TTS providers (volc speaker ids,
    /// oMLX 12Hz voices); empty = provider default.
    #[serde(default)]
    pub voice: String,
    /// Optional ASR language hint (ISO 639-1, e.g. "zh", "en").
    /// Empty (default) omits `language` so OpenAI/Groq Whisper and SenseVoice
    /// auto-detect the spoken language cleanly for both Chinese and English users.
    #[serde(default)]
    pub language: String,
}

impl ServiceConfig {
    pub fn is_configured(&self) -> bool {
        !self.provider.is_empty() && self.provider != "unconfigured"
    }
}

/// Response shape: config plus whether a key is stored (never the value).
#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub api_key_set: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub voice: String,
    /// LLM thinking mode ("" = provider default).
    #[serde(default)]
    pub thinking: String,
    /// Optional ASR language hint ("" = auto-detect).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub language: String,
}
