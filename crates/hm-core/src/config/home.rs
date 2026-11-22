//! Home automation (R1j/P0-b): which backend would execute `home.set`, the
//! whitelisted entities, and the bounded Home Assistant call window.
//!
//! The whitelist is a security boundary, not a UI filter: a domain outside
//! `HOME_SET_DOMAINS` must never become controllable, whoever typed it.

use serde::{Deserialize, Serialize};

/// Home automation (R1j/P0-b): which backend would execute `home.set`.
/// `none` (default) keeps the tool registered but Unconfigured — the model
/// must say it cannot control lights, and no layer may answer
/// `DeviceConfirmed` out of nothing.
pub const HOME_KEY: &str = "home";
/// R2-BM5: the Home Assistant long-lived token (secrets table, write-only).
pub const HOME_TOKEN_SECRET: &str = "home.token";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HomeBackend {
    /// No backend. Registered so the household can see the service is
    /// missing, not that it does not exist.
    #[default]
    None,
    /// Reserved for the R2-BM5 adapter (write-then-read-back). Accepted by
    /// the config today; the tool still refuses until the adapter ships.
    HomeAssistant,
}

/// R2-BM6.3 (2026-11-13): which mechanism supplies the entity directory.
/// The user asked for a choice when connecting, and the two answers are not
/// interchangeable: `exposed` is Home Assistant's own curated Assist set (no
/// local picking), `api` is the whole house filtered in the console.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EntitySource {
    /// `GET /api/states` + console search/domain filter/checkbox/manual add.
    #[default]
    Api,
    /// `homeassistant/expose_entity/list` (WebSocket): only entities HA
    /// exposes to Assist for conversation. Read-only in the console.
    Exposed,
}

/// R2-BM5: one whitelisted entity. The MODEL only ever names `name`; the
/// real `entity` id comes from this household-configured mapping, so an
/// invented name cannot reach Home Assistant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HomeEntity {
    /// What a person says («儿童房的灯»).
    pub name: String,
    /// The Home Assistant entity id («light.children_room»).
    pub entity: String,
}

/// R2-BM6.1: the domains `home.set` may control. The list is the security
/// boundary, not a UI filter: lock/alarm/valve/script/automation/
/// shell_command/python_script/hassio must never become controllable through
/// a discovered or hand-typed whitelist entry (RUI's `EXCLUDED_DOMAINS`
/// lesson, 2026-11-10: a misheard name must not reach a lock).
pub const HOME_SET_DOMAINS: [&str; 5] = ["light", "switch", "fan", "input_boolean", "cover"];

pub fn home_domain_supported(entity_id: &str) -> bool {
    entity_id
        .split('.')
        .next()
        .map(|d| HOME_SET_DOMAINS.contains(&d))
        .unwrap_or(false)
}

pub const DEFAULT_HOME_TIMEOUT_MS: u64 = 4_000;
pub const DEFAULT_HOME_RETRIES: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HomeConfig {
    #[serde(default)]
    pub backend: HomeBackend,
    /// R2-BM6.3 (2026-11-13): where the entity directory comes from.
    /// `api` = the house's `GET /api/states`, filtered in the console (the
    /// BM6.1 behaviour). `exposed` = Home Assistant's own Assist exposure
    /// (`homeassistant/expose_entity/list`, conversation:true) — the console
    /// only reads/refreshes, editing happens in HA.
    #[serde(default)]
    pub entity_source: EntitySource,
    /// Home Assistant root, e.g. `http://homeassistant.local:8123`.
    #[serde(default)]
    pub base_url: String,
    /// The ONLY entities the brain may touch (白名单).
    #[serde(default)]
    pub entities: Vec<HomeEntity>,
    /// Per-request bound for the adapter; clamped on save.
    #[serde(default = "default_home_timeout_ms")]
    pub timeout_ms: u64,
    /// Bounded retry count for transient failures (connect/5xx/timeout).
    #[serde(default = "default_home_retries")]
    pub retries: u32,
}

fn default_home_timeout_ms() -> u64 {
    DEFAULT_HOME_TIMEOUT_MS
}
fn default_home_retries() -> u32 {
    DEFAULT_HOME_RETRIES
}

impl Default for HomeConfig {
    fn default() -> Self {
        Self {
            backend: HomeBackend::None,
            entity_source: EntitySource::Api,
            base_url: String::new(),
            entities: Vec::new(),
            timeout_ms: DEFAULT_HOME_TIMEOUT_MS,
            retries: DEFAULT_HOME_RETRIES,
        }
    }
}

impl HomeConfig {
    pub(crate) fn endpoint_is_portable(endpoint: &str) -> bool {
        endpoint.is_empty()
            || reqwest::Url::parse(endpoint).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
            })
    }

    pub(crate) fn normalized_with(self, allow_loopback: bool) -> Result<Self, super::ConfigError> {
        use super::ConfigError;
        let mut cfg = self;
        cfg.base_url = cfg.base_url.trim().trim_end_matches('/').to_string();
        if !super::valid_home_endpoint_with(&cfg.base_url, allow_loopback)
            || !Self::endpoint_is_portable(&cfg.base_url)
        {
            return Err(ConfigError::BadHome("base_url must be an HTTP(S) Home Assistant root without inline credentials, query or fragment".into()));
        }
        if cfg.entities.len() > 64 {
            return Err(ConfigError::BadHome(
                "at most 64 whitelisted entities".into(),
            ));
        }
        let mut names = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        for e in &mut cfg.entities {
            e.name = e.name.trim().to_string();
            e.entity = e.entity.trim().trim_end_matches('/').to_string();
            if e.name.is_empty() || e.entity.is_empty() {
                return Err(ConfigError::BadHome(
                    "entity name and id must not be empty".into(),
                ));
            }
            if e.entity.contains(char::is_whitespace) || !e.entity.contains('.') {
                return Err(ConfigError::BadHome(format!(
                    "'{}' is not a Home Assistant entity id (domain.object_id)",
                    e.entity
                )));
            }
            if !home_domain_supported(&e.entity) {
                return Err(ConfigError::BadHome(format!(
                    "'{}' 不在可控域：只支持 {}（锁/报警/阀门/脚本等一律拒绝）",
                    e.entity,
                    HOME_SET_DOMAINS.join("/")
                )));
            }
            if !names.insert(e.name.clone()) {
                return Err(ConfigError::BadHome(format!(
                    "duplicate entity name '{}'",
                    e.name
                )));
            }
            if !ids.insert(e.entity.clone()) {
                return Err(ConfigError::BadHome(format!(
                    "duplicate entity id '{}'",
                    e.entity
                )));
            }
        }
        cfg.timeout_ms = cfg.timeout_ms.clamp(500, 15_000);
        cfg.retries = cfg.retries.min(3);
        Ok(cfg)
    }
}
