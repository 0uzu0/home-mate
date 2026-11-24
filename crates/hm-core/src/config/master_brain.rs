//! Provider-agnostic Master Brain endpoint configuration (#267).
//!
//! `hm-core` supports two clean protocols:
//! - `hermes` (default, zero-bridge direct mode): talks directly to a Hermes
//!   Agent HTTP/SSE API (`/v1/runs`, `/v1/runs/{id}/events`, `/steer`,
//!   `/approval`, `/stop`) and delivers finished summaries straight into
//!   in-process `VoicePipeline::enqueue_announce` (no bridge daemon, no
//!   `callback_url` required).
//! - `generic`: talks to any external Master Brain service implementing the
//!   Home Mate async contract (`/v1/tasks*` + downward `/api/v0/voice/announce`).
//!
//! Bearer tokens live solely in the encrypted Vault (`secrets` table), never
//! in `config` or `config_history`.

use super::{endpoint::valid_news_endpoint, ConfigError};
use serde::{Deserialize, Serialize};

pub const MASTER_BRAIN_KEY: &str = "master_brain";
pub const MASTER_BRAIN_TOKEN_SECRET: &str = "master_brain_token";
pub const DEFAULT_MASTER_BRAIN_TIMEOUT_MS: u64 = 8_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MasterBrainProtocol {
    #[default]
    Hermes,
    Generic,
}

impl MasterBrainProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hermes => "hermes",
            Self::Generic => "generic",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MasterBrainConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Protocol dialect: `"hermes"` (direct zero-bridge) or `"generic"` (custom bridge).
    #[serde(default)]
    pub protocol: MasterBrainProtocol,
    /// Master Brain HTTP(S) base URL (e.g. `http://192.168.1.10:8642`).
    #[serde(default)]
    pub url: String,
    /// Optional base URL of THIS core for `generic` mode callbacks to
    /// `{callback_url}/api/v0/voice/announce`. Unused in direct `hermes` mode.
    #[serde(default)]
    pub callback_url: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    DEFAULT_MASTER_BRAIN_TIMEOUT_MS
}

impl Default for MasterBrainConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            protocol: MasterBrainProtocol::Hermes,
            url: String::new(),
            callback_url: String::new(),
            timeout_ms: DEFAULT_MASTER_BRAIN_TIMEOUT_MS,
        }
    }
}

fn normalize_endpoint(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if !trimmed.is_empty() && !trimmed.contains("://") {
        format!("http://{trimmed}")
    } else {
        trimmed.to_string()
    }
}

impl MasterBrainConfig {
    pub fn normalized(mut self) -> Self {
        self.url = normalize_endpoint(&self.url);
        self.callback_url = normalize_endpoint(&self.callback_url);
        self.timeout_ms = self.timeout_ms.clamp(1_000, 30_000);
        self
    }

    pub fn validate(&self, allow_loopback: bool) -> Result<(), ConfigError> {
        if !self.url.is_empty() && !valid_news_endpoint(&self.url, allow_loopback) {
            return Err(ConfigError::PersonalEndpoint(
                "master_brain.url must be a LAN or public http(s) URL".into(),
            ));
        }
        if !self.callback_url.is_empty() && !valid_news_endpoint(&self.callback_url, allow_loopback)
        {
            return Err(ConfigError::PersonalEndpoint(
                "master_brain.callback_url must be a LAN or public http(s) URL".into(),
            ));
        }
        if self.enabled && self.url.is_empty() {
            return Err(ConfigError::PersonalEndpoint(
                "master_brain.url is required when enabled".into(),
            ));
        }
        Ok(())
    }

    pub fn is_ready(&self) -> bool {
        self.enabled && !self.url.is_empty()
    }

    pub fn state_word(&self) -> &'static str {
        if self.is_ready() {
            "ready"
        } else if !self.url.is_empty() {
            "disabled"
        } else {
            "unconfigured"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_brain_config_validates_lan_urls_and_rejects_metadata() {
        let cfg = MasterBrainConfig {
            enabled: true,
            protocol: MasterBrainProtocol::Hermes,
            url: "192.168.1.10:8642/".into(),
            callback_url: String::new(),
            timeout_ms: 8_000,
        }
        .normalized();
        assert_eq!(cfg.url, "http://192.168.1.10:8642");
        assert_eq!(cfg.protocol, MasterBrainProtocol::Hermes);
        assert!(cfg.validate(false).is_ok());
        assert_eq!(cfg.state_word(), "ready");

        let meta = MasterBrainConfig {
            enabled: true,
            protocol: MasterBrainProtocol::Hermes,
            url: "http://169.254.169.254/latest".into(),
            callback_url: String::new(),
            timeout_ms: 8_000,
        };
        assert!(meta.validate(false).is_err());

        let bad_cb = MasterBrainConfig {
            enabled: true,
            protocol: MasterBrainProtocol::Generic,
            url: "http://192.168.1.10:18940".into(),
            callback_url: "http://169.254.169.254/meta".into(),
            timeout_ms: 8_000,
        };
        assert!(bad_cb.validate(false).is_err());

        let loopback = MasterBrainConfig {
            enabled: true,
            protocol: MasterBrainProtocol::Hermes,
            url: "http://127.0.0.1:8642".into(),
            callback_url: String::new(),
            timeout_ms: 8_000,
        };
        assert!(loopback.validate(false).is_err());
        assert!(loopback.validate(true).is_ok());

        let disabled = MasterBrainConfig {
            enabled: false,
            protocol: MasterBrainProtocol::Generic,
            url: "http://192.168.1.10:18940".into(),
            callback_url: String::new(),
            timeout_ms: 8_000,
        };
        assert_eq!(disabled.state_word(), "disabled");
        assert_eq!(MasterBrainConfig::default().state_word(), "unconfigured");
    }
}
