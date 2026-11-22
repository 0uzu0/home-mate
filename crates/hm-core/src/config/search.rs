//! General web information search (M2.5, DeepSeek Search API shape).
//!
//! A household may point it at any compatible `/tools/search` endpoint, and an
//! unconfigured household must answer without one request. The key is NOT in
//! this block: it lives in the secrets table, so a status read can only say
//! whether one is set.

use super::{valid_public_endpoint, ConfigError};
use serde::{Deserialize, Serialize};

/// M2.5: general web information search (DeepSeek Search API shape). Its own
/// block: a household may point it at any compatible `/tools/search`
/// endpoint, and an unconfigured household must answer without one request.
pub const SEARCH_KEY: &str = "search";
/// M2.5: the search bearer key (secrets table, write-only, `secret_set`
/// read-back only). The user's decision (2026-11-13) is to reuse the existing
/// DeepSeek key, but the assistant never reads it from another service's
/// field — it is stored and cleared here.
pub const SEARCH_SECRET: &str = "search.api_key";

// --- general web search (M2.5) --------------------------------------------

/// The provider accepts 1–50; the assistant READS at most `MAX_SEARCH_RESULTS`
/// of them (a spoken answer cannot carry fifty sources, and every extra row is
/// an instruction-shaped string in the prompt).
pub const MAX_SEARCH_COUNT: u8 = 50;
pub const MAX_SEARCH_RESULTS: usize = 8;
pub const DEFAULT_SEARCH_COUNT: u8 = 6;
pub const DEFAULT_SEARCH_TIMEOUT_MS: u64 = 8_000;

pub fn default_search_base_url() -> String {
    "https://api.deepseek.com/v1/tools/search".into()
}

fn default_search_count() -> u8 {
    DEFAULT_SEARCH_COUNT
}

fn default_search_timeout_ms() -> u64 {
    DEFAULT_SEARCH_TIMEOUT_MS
}

/// Web information search. `api_key` is NOT here: it lives in the secrets
/// table, so a status read can only say whether one is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_search_base_url")]
    pub base_url: String,
    /// How many results to ask the provider for (1–50). The model receives at
    /// most `MAX_SEARCH_RESULTS`.
    #[serde(default = "default_search_count")]
    pub count: u8,
    #[serde(default = "default_search_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_search_base_url(),
            count: DEFAULT_SEARCH_COUNT,
            timeout_ms: DEFAULT_SEARCH_TIMEOUT_MS,
        }
    }
}

impl SearchConfig {
    /// Same three words as weather/news/home. A search endpoint without a key
    /// cannot answer (the provider bills a keyed account), so "no key" is
    /// unconfigured, not ready: the tool must refuse before any HTTP.
    pub fn state_word(&self, key_set: bool) -> &'static str {
        if !key_set {
            "unconfigured"
        } else if !self.enabled {
            "disabled"
        } else {
            "ready"
        }
    }

    /// What to ask the provider for (already validated on save, clamped here
    /// so a hand-edited store cannot send `count: 0`).
    pub fn effective_count(&self) -> u8 {
        self.count.clamp(1, MAX_SEARCH_COUNT)
    }

    /// What the model may see.
    pub fn result_cap(&self) -> usize {
        (self.effective_count() as usize).min(MAX_SEARCH_RESULTS)
    }
}

/// Read-back shape: no key value, only whether one is stored.
#[derive(Debug, Clone, Serialize)]
pub struct SearchStatus {
    pub enabled: bool,
    pub base_url: String,
    pub count: u8,
    pub timeout_ms: u64,
    pub api_key_set: bool,
    pub state: String,
    pub revision: String,
}

impl SearchConfig {
    pub(crate) fn normalized(self, allow_loopback: bool) -> Result<Self, ConfigError> {
        let mut cfg = self;
        let bad = |m: String| ConfigError::BadSearch(m);
        cfg.base_url = cfg.base_url.trim().to_string();
        if cfg.base_url.is_empty() {
            cfg.base_url = default_search_base_url();
        }
        if !valid_public_endpoint(&cfg.base_url, allow_loopback)
            || !crate::config_delivery::portable_service_url(&cfg.base_url)
        {
            return Err(bad(
                "接口地址必须是合法的公网 HTTP(S) 地址，凭据请用独立密钥字段".into(),
            ));
        }
        if cfg.count == 0 || cfg.count > MAX_SEARCH_COUNT {
            return Err(bad(format!("条数必须是 1–{MAX_SEARCH_COUNT}")));
        }
        cfg.timeout_ms = cfg.timeout_ms.clamp(500, 15_000);
        Ok(cfg)
    }
}
