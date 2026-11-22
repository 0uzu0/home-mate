//! News feeds (R2-BM6): RSS/Atom/JSON sources.
//!
//! Registered by default but unusable until a household configures at least one
//! source — the prompt says so and the tool answers `NEWS_UNCONFIGURED` without
//! touching the network.

use super::{endpoint::valid_place_alias, valid_news_endpoint, ConfigError};
use serde::{Deserialize, Serialize};

/// R2-BM6: news feeds (RSS/Atom/JSON). Registered by default but unusable
/// until a household configures at least one source — the prompt says so and
/// the tool answers `NEWS_UNCONFIGURED` without touching the network.
pub const NEWS_KEY: &str = "news";

// --- news (R2-BM6) ---------------------------------------------------------

pub const MAX_NEWS_SOURCES: usize = 5;
pub const MAX_NEWS_SOURCES_CAP: usize = 50;
pub const MAX_NEWS_ITEMS: u8 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NewsKind {
    /// RSS 2.0: `<item><title>/<link>`.
    #[default]
    Rss,
    /// Atom: `<entry><title>/<link href>`.
    Atom,
    /// `{"items":[{"title","link"}]}` (or a bare array of those objects).
    Json,
}

impl NewsKind {
    pub fn label(self) -> &'static str {
        match self {
            NewsKind::Rss => "rss",
            NewsKind::Atom => "atom",
            NewsKind::Json => "json",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NewsSource {
    /// What a person calls it («科技»). Required, unique.
    pub alias: String,
    pub url: String,
    #[serde(default)]
    pub kind: NewsKind,
    /// Per-source cap; clamped to 1…MAX_NEWS_ITEMS on save.
    #[serde(default = "default_news_items")]
    pub max_items: u8,
}

fn default_news_items() -> u8 {
    5
}

pub const DEFAULT_NEWS_TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub sources: Vec<NewsSource>,
    /// The source a bare "今天有什么新闻" means. "" with one source = it.
    #[serde(default)]
    pub default_alias: String,
    #[serde(default = "default_news_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_max_news_sources")]
    pub max_sources: usize,
}

fn default_news_timeout_ms() -> u64 {
    DEFAULT_NEWS_TIMEOUT_MS
}

fn default_max_news_sources() -> usize {
    MAX_NEWS_SOURCES
}

impl Default for NewsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sources: Vec::new(),
            default_alias: String::new(),
            timeout_ms: DEFAULT_NEWS_TIMEOUT_MS,
            max_sources: MAX_NEWS_SOURCES,
        }
    }
}

impl NewsConfig {
    /// The same three words as weather/home: never configured = unconfigured,
    /// configured but switched off = disabled, otherwise ready.
    pub fn state_word(&self) -> &'static str {
        if self.sources.is_empty() {
            "unconfigured"
        } else if !self.enabled {
            "disabled"
        } else {
            "ready"
        }
    }

    pub fn resolve_source(&self, alias: Option<&str>) -> Option<&NewsSource> {
        match alias.map(str::trim).filter(|a| !a.is_empty()) {
            Some(a) => self
                .sources
                .iter()
                .find(|s| s.alias.eq_ignore_ascii_case(a) || s.alias == a),
            None => self.default_sources().into_iter().next(),
        }
    }

    /// What "今天有什么新闻" means when the caller names no source.
    ///
    /// M7-3 (2026-11-13): the old rule made the MODEL choose whenever more
    /// than one source was configured (`NEWS_SOURCE_REQUIRED`) — a round-trip
    /// the household did not ask for. The default is now: the configured
    /// `default_alias` when it resolves, otherwise ALL sources (the tool
    /// merges them, and one broken feed never poisons the rest).
    pub fn default_sources(&self) -> Vec<&NewsSource> {
        if !self.default_alias.is_empty() {
            if let Some(s) = self.sources.iter().find(|s| s.alias == self.default_alias) {
                return vec![s];
            }
        }
        self.sources.iter().collect()
    }
}

/// Read-back shape: sources are visible; there is no secret.
#[derive(Debug, Clone, Serialize)]
pub struct NewsStatus {
    pub enabled: bool,
    pub sources: Vec<NewsSource>,
    pub default_alias: String,
    pub timeout_ms: u64,
    pub max_sources: usize,
    pub state: String,
    pub revision: String,
}

impl NewsConfig {
    pub(crate) fn normalized(self, allow_loopback: bool) -> Result<Self, ConfigError> {
        let mut cfg = self;
        let bad = |m: String| ConfigError::BadNews(m);
        cfg.max_sources = cfg.max_sources.clamp(1, MAX_NEWS_SOURCES_CAP);
        if cfg.sources.len() > cfg.max_sources {
            return Err(bad(format!("最多 {} 个新闻源", cfg.max_sources)));
        }
        let mut seen: Vec<String> = Vec::new();
        for s in cfg.sources.iter_mut() {
            s.alias = s.alias.trim().to_string();
            s.url = s.url.trim().to_string();
            if !valid_place_alias(&s.alias) {
                return Err(bad(format!(
                    "源「{}」的名字不合法：1–20 字，不含引号/斜杠",
                    s.alias
                )));
            }
            if seen.iter().any(|a| a.eq_ignore_ascii_case(&s.alias)) {
                return Err(bad(format!("源「{}」重复", s.alias)));
            }
            seen.push(s.alias.clone());
            if !valid_news_endpoint(&s.url, allow_loopback)
                || s.url.is_empty()
                || !crate::config_delivery::portable_service_url(&s.url)
            {
                return Err(bad(format!(
                    "源「{}」的地址必须是合法的公网或局域网 http(s)://",
                    s.alias
                )));
            }
            if s.max_items == 0 || s.max_items > MAX_NEWS_ITEMS {
                return Err(bad(format!(
                    "源「{}」的条数必须是 1–{MAX_NEWS_ITEMS}",
                    s.alias
                )));
            }
        }
        cfg.default_alias = cfg.default_alias.trim().to_string();
        if !cfg.default_alias.is_empty()
            && !cfg.sources.iter().any(|s| s.alias == cfg.default_alias)
        {
            return Err(bad(format!("默认源「{}」不在列表里", cfg.default_alias)));
        }
        cfg.timeout_ms = cfg.timeout_ms.clamp(500, 15_000);
        Ok(cfg)
    }
}
