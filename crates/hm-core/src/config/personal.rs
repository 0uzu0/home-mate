//! NAS `rui-personal-services` endpoint configuration (`story-svc`, `joke-svc`, `learning-svc`).
//!
//! Each service is registered by default but disabled (`enabled = false`) until
//! a household saves a LAN/HTTPS URL (and optional Bearer token). Secrets live
//! only in the `secrets` table, never in `config` or `config_history`.

use super::{endpoint::valid_news_endpoint, ConfigError};
use serde::{Deserialize, Serialize};

pub const STORY_KEY: &str = "story";
pub const STORY_TOKEN_SECRET: &str = "story_token";
pub const JOKE_KEY: &str = "joke";
pub const JOKE_TOKEN_SECRET: &str = "joke_token";
pub const RADIO_KEY: &str = "radio";
pub const RADIO_TOKEN_SECRET: &str = "radio_token";
pub const LEARNING_KEY: &str = "learning";
pub const LEARNING_TOKEN_SECRET: &str = "learning_token";
pub const DEFAULT_PERSONAL_TIMEOUT_MS: u64 = 4_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonalEndpointConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    DEFAULT_PERSONAL_TIMEOUT_MS
}

impl Default for PersonalEndpointConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            timeout_ms: DEFAULT_PERSONAL_TIMEOUT_MS,
        }
    }
}

impl PersonalEndpointConfig {
    pub fn normalized(mut self) -> Self {
        let trimmed = self.url.trim().trim_end_matches('/');
        self.url = if !trimmed.is_empty() && !trimmed.contains("://") {
            format!("http://{trimmed}")
        } else {
            trimmed.to_string()
        };
        self.timeout_ms = self.timeout_ms.clamp(1_000, 15_000);
        self
    }

    pub fn validate(&self, name: &str, allow_loopback: bool) -> Result<(), ConfigError> {
        if !self.url.is_empty() && !valid_news_endpoint(&self.url, allow_loopback) {
            return Err(ConfigError::PersonalEndpoint(format!(
                "{name}.url must be a LAN or public http(s) URL"
            )));
        }
        if self.enabled && self.url.is_empty() {
            return Err(ConfigError::PersonalEndpoint(format!(
                "{name}.url is required when enabled"
            )));
        }
        Ok(())
    }

    pub fn is_ready(&self) -> bool {
        self.enabled && !self.url.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_endpoint_accepts_lan_and_rejects_empty_when_enabled() {
        let ok = PersonalEndpointConfig {
            enabled: true,
            url: "http://192.168.1.10:18083/".into(),
            timeout_ms: 4000,
        }
        .normalized();
        assert_eq!(ok.url, "http://192.168.1.10:18083");
        assert!(ok.validate("story", false).is_ok());

        let missing = PersonalEndpointConfig {
            enabled: true,
            url: String::new(),
            timeout_ms: 4000,
        };
        assert!(missing.validate("story", false).is_err());

        let metadata = PersonalEndpointConfig {
            enabled: true,
            url: "http://169.254.169.254/latest".into(),
            timeout_ms: 4000,
        };
        assert!(metadata.validate("joke", false).is_err());

        let loopback = PersonalEndpointConfig {
            enabled: true,
            url: "http://127.0.0.1:8930".into(),
            timeout_ms: 4000,
        };
        assert!(loopback.validate("story", false).is_err());
        assert!(loopback.validate("story", true).is_ok());

        let bare_lan = PersonalEndpointConfig {
            enabled: true,
            url: "192.168.1.10:18081/".into(),
            timeout_ms: 4000,
        }
        .normalized();
        assert_eq!(bare_lan.url, "http://192.168.1.10:18081");
        assert!(bare_lan.validate("joke", false).is_ok());

        let bare_meta = PersonalEndpointConfig {
            enabled: true,
            url: "169.254.169.254/latest".into(),
            timeout_ms: 4000,
        }
        .normalized();
        assert!(bare_meta.validate("joke", false).is_err());
    }
}
