//! Service configuration (LLM / ASR / TTS), persisted in the store.
//!
//! Each service is independently selectable, testable and replaceable
//! (acceptance: swapping one leaves the other two working). API keys are
//! write-only: values land in the secrets table, responses only report
//! whether one is set.
//!
//! This module keeps what is SHARED: the store-backed `ConfigService` (read,
//! save, validate, history), the canonical secret list and the revision hash
//! every block uses. Each block's own types and defaults live in the submodule
//! named after it and are re-exported here, so `crate::config::<Block>` keeps
//! resolving exactly as before.

mod audio;
mod endpoint;
mod home;
mod master_brain;
mod music;
mod news;
mod openapi;
mod personal;
mod profile;
mod reflex;
mod runtime;
mod search;
mod services;
mod shared_memory;
mod weather;

pub use audio::{AudioEqConfig, EqBandCfg, AUDIO_CURVES, AUDIO_EQ_KEY, VOICING_REF};
pub use endpoint::{
    valid_home_endpoint, valid_home_endpoint_with, valid_music_endpoint, valid_music_endpoint_with,
    valid_news_endpoint, valid_public_endpoint, valid_weather_endpoint,
};
pub use home::{
    home_domain_supported, EntitySource, HomeBackend, HomeConfig, HomeEntity, DEFAULT_HOME_RETRIES,
    DEFAULT_HOME_TIMEOUT_MS, HOME_KEY, HOME_SET_DOMAINS, HOME_TOKEN_SECRET,
};
pub use master_brain::{
    MasterBrainConfig, MasterBrainProtocol, DEFAULT_MASTER_BRAIN_TIMEOUT_MS, MASTER_BRAIN_KEY,
    MASTER_BRAIN_TOKEN_SECRET,
};
pub(crate) use music::MAX_MUSIC_CONNECTION_FIELD;
pub use music::{
    MusicConfig, MusicConnection, MusicConnectionStatus, MusicStatus, MUSIC_KEY,
    MUSIC_LIBRARY_SECRET, MUSIC_ONLINE_SECRET,
};
pub use news::{
    NewsConfig, NewsKind, NewsSource, NewsStatus, DEFAULT_NEWS_TIMEOUT_MS, MAX_NEWS_ITEMS,
    MAX_NEWS_SOURCES, MAX_NEWS_SOURCES_CAP, NEWS_KEY,
};
pub use openapi::{
    validate_auth_header_name, validate_openapi_config, EffectLevel, OpenApiConfig,
    OpenApiOperation, OpenApiParam, OpenApiParamLocation, OpenApiStatus, VerificationStrategy,
    DEFAULT_OPENAPI_RETRIES, DEFAULT_OPENAPI_TIMEOUT_MS, MAX_OPENAPI_OPERATIONS,
    MAX_OPENAPI_PARAMS, OPENAPI_KEY, OPENAPI_TOKEN_SECRET,
};
pub use personal::{
    PersonalEndpointConfig, DEFAULT_PERSONAL_TIMEOUT_MS, JOKE_KEY, JOKE_TOKEN_SECRET, LEARNING_KEY,
    LEARNING_TOKEN_SECRET, RADIO_KEY, RADIO_TOKEN_SECRET, STORY_KEY, STORY_TOKEN_SECRET,
};
pub use profile::{
    DeviceLedConfig, UnitAcousticProfile, DEVICE_LED_CONFIGS_KEY, UNIT_ACOUSTIC_PROFILES_KEY,
};
pub use reflex::{
    compile_keywords, default_reflex_words, reflex_intent_map, reflex_tokens, reflex_warnings,
    ReflexConfig, ReflexIntent, ReflexWord, REFLEX_KEY,
};
pub use runtime::RuntimeServices;
pub use search::{
    default_search_base_url, SearchConfig, SearchStatus, DEFAULT_SEARCH_COUNT,
    DEFAULT_SEARCH_TIMEOUT_MS, MAX_SEARCH_COUNT, MAX_SEARCH_RESULTS, SEARCH_KEY, SEARCH_SECRET,
};
pub use services::{DeviceMeta, ServiceConfig, ServiceStatus, ServicesConfig, DEVICE_META_KEY};
pub use shared_memory::{SharedMemoryConfig, SHARED_MEMORY_KEY, SHARED_MEMORY_TOKEN_SECRET};
pub use weather::{
    WeatherConfig, WeatherPlace, WeatherStatus, MAX_WEATHER_PLACES, WEATHER_KEY, WEATHER_SECRET,
};

use crate::store::{Result, Store};

use services::default_thinking;

pub const CONFIG_KEY: &str = "services";

/// Every secret name that belongs to a household config backup (and therefore
/// to a fleet push). One canonical list: the exporter, the importer, the
/// startup backfill and the console's key-presence checks all read this.
///
/// Not in the list, on purpose: the fleet peer tokens (`fleet.<id>.token`).
/// They are credentials for THIS node's wiring, not household configuration —
/// exporting them would spread one node's access around the household, and
/// importing them would silently point a device at a peer it cannot reach.
pub const CONFIG_SECRETS: [&str; 15] = [
    "llm.api_key",
    "llm_fallback.api_key",
    "asr.api_key",
    "tts.api_key",
    "music.library.secret",
    "music.online.secret",
    "weather.api_key",
    "home.token",
    "openapi.token",
    "search.api_key",
    MASTER_BRAIN_TOKEN_SECRET,
    STORY_TOKEN_SECRET,
    JOKE_TOKEN_SECRET,
    RADIO_TOKEN_SECRET,
    LEARNING_TOKEN_SECRET,
];

/// FNV-1a 64 (hex) — the same hash the gateway reports, so "did the device
/// get the file I compiled" is one comparison. Not a security hash.
pub fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// `true` for an optional on/off flag that defaults to on.
pub(super) fn default_true() -> bool {
    true
}

pub struct ConfigService {
    store: std::sync::Arc<Store>,
}

pub(crate) fn decode_services(raw: Option<&str>) -> Result<ServicesConfig> {
    if let Some(raw) = raw {
        return Ok(serde_json::from_str(raw)?);
    }
    Ok(ServicesConfig {
        llm: ServiceConfig {
            provider: "deepseek".into(),
            base_url: default_llm_base_url(),
            model: "deepseek-flash".into(),
            api_key_env: "DEEPSEEK_API_KEY".into(),
            voice: String::new(),
            thinking: default_thinking(),
        },
        ..Default::default()
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unknown service '{0}' (expected llm|asr|tts)")]
    UnknownService(String),
    #[error("store: {0}")]
    Store(#[from] crate::store::StoreError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("music endpoint '{which}' must be a LAN HTTP(S) root without credentials, query or fragment")]
    BadMusicEndpoint { which: &'static str },
    #[error("music {which} connection fields exceed the length limit or contain NUL")]
    BadMusicConnection { which: &'static str },
    #[error("unsupported weather provider '{0}' (expected open_meteo, met_no, or wttr_in)")]
    UnsupportedWeatherProvider(String),
    #[error("weather endpoint must be an HTTP(S) URL without credentials, a query or a fragment")]
    BadWeatherEndpoint,
    #[error("weather max_days must be 1–7, got {0}")]
    BadWeatherDays(u8),
    #[error("{0}")]
    PersonalEndpoint(String),
    #[error("configuration changed")]
    ServiceConflict { revision: String },
    #[error("weather configuration changed")]
    WeatherConflict { revision: String },
    #[error("weather place: {0}")]
    BadWeatherPlace(String),
    #[error("device label: {0}")]
    BadDeviceLabel(String),
    #[error("device profile: {0}")]
    BadDeviceProfile(String),
    #[error("reflex vocabulary: {0}")]
    BadReflex(String),
    #[error("output tone: {0}")]
    BadAudioEq(String),
    #[error("home automation: {0}")]
    BadHome(String),
    #[error("news: {0}")]
    BadNews(String),
    #[error("search: {0}")]
    BadSearch(String),
    #[error("shared_memory: {0}")]
    BadSharedMemory(String),
    #[error("openapi: {0}")]
    BadOpenApi(String),
}

impl ConfigService {
    /// Read the editable blocks from the shared voice configuration plus
    /// the latest versioned `Agent.md` content (hot-reloaded per turn).
    pub fn prompt_preferences(
        &self,
    ) -> std::result::Result<crate::agent::prompt::PromptPreferences, String> {
        let mut prefs: crate::agent::prompt::PromptPreferences = self
            .store
            .config_get(crate::voice::VOICE_CFG_KEY)
            .map_err(|e| e.to_string())?
            .map(|raw| serde_json::from_str(&raw).map_err(|e| e.to_string()))
            .transpose()?
            .unwrap_or_default();
        prefs.agent_md = crate::agent_md::AgentMdService::new(self.store.clone()).current_content();
        Ok(prefs)
    }

    pub fn new(store: std::sync::Arc<Store>) -> Self {
        Self { store }
    }

    /// Load the stored config, or seed defaults for a fresh install.
    pub fn load(&self) -> Result<ServicesConfig> {
        decode_services(self.store.config_get(CONFIG_KEY)?.as_deref())
    }

    /// Save one service's config (and optionally its key). Returns the
    /// effective status. `actor` records who changed it (console/voice/system).
    pub fn save_service(
        &self,
        service: &str,
        cfg: ServiceConfig,
        api_key: Option<&str>,
        actor: &str,
    ) -> std::result::Result<ServiceStatus, ConfigError> {
        crate::config_delivery::ConfigDelivery::save_local_service(
            &self.store,
            service,
            cfg,
            api_key,
            actor,
        )
    }

    pub fn status(&self, service: &str) -> std::result::Result<ServiceStatus, ConfigError> {
        let all = self.load()?;
        let (cfg, set) = match service {
            "llm" => (all.llm, self.key_set("llm")?),
            "llm_fallback" => (all.llm_fallback, self.key_set("llm_fallback")?),
            "asr" => (all.asr, self.key_set("asr")?),
            "tts" => (all.tts, self.key_set("tts")?),
            other => return Err(ConfigError::UnknownService(other.into())),
        };
        Ok(ServiceStatus {
            provider: cfg.provider,
            base_url: cfg.base_url,
            model: cfg.model,
            api_key_env: cfg.api_key_env,
            api_key_set: set,
            voice: cfg.voice,
            thinking: cfg.thinking,
        })
    }

    pub fn statuses(&self) -> Result<serde_json::Value> {
        let all = self.load()?;
        let one = |c: &ServiceConfig, set: bool| {
            serde_json::json!({
                "provider": c.provider, "base_url": c.base_url, "model": c.model,
                "api_key_env": c.api_key_env, "api_key_set": set, "configured": c.is_configured(),
                "voice": c.voice, "thinking": c.thinking,
            })
        };
        Ok(serde_json::json!({
            "llm": one(&all.llm, self.key_set("llm")?),
            "llm_fallback": one(&all.llm_fallback, self.key_set("llm_fallback")?),
            "asr": one(&all.asr, self.key_set("asr")?),
            "tts": one(&all.tts, self.key_set("tts")?),
        }))
    }

    /// Resolve the effective API key: stored secret first, then env var.
    pub fn resolve_key(&self, service: &str) -> Result<Option<String>> {
        if let Some(k) = self.store.secret_get(&format!("{service}.api_key"))? {
            return Ok(Some(k));
        }
        let all = self.load()?;
        let cfg = match service {
            "llm" => &all.llm,
            "llm_fallback" => &all.llm_fallback,
            "asr" => &all.asr,
            "tts" => &all.tts,
            _ => return Ok(None),
        };
        if cfg.api_key_env.is_empty() {
            return Ok(None);
        }
        Ok(std::env::var(&cfg.api_key_env).ok())
    }

    /// Is a key actually available for this service?
    ///
    /// Two sources, in `resolve_key` order: a stored secret, then the
    /// service's `api_key_env`. Before 2026-11-14 this only looked at the
    /// database, so a household that keeps `DEEPSEEK_API_KEY`/`VOLC_API_KEY`
    /// in `data/env.local` (the documented deployment) saw
    /// `api_key_set:false` on a service that was working — the console said
    /// "no key" next to a live model. A stored-but-empty secret still
    /// shadows the env var (that is what "clear" means), so presence of the
    /// row is checked before falling back.
    fn key_set(&self, service: &str) -> Result<bool> {
        let name = format!("{service}.api_key");
        if let Some(k) = self.store.secret_get(&name)? {
            return Ok(!k.trim().is_empty());
        }
        let all = self.load()?;
        let env = match service {
            "llm" => &all.llm.api_key_env,
            "llm_fallback" => &all.llm_fallback.api_key_env,
            "asr" => &all.asr.api_key_env,
            "tts" => &all.tts.api_key_env,
            _ => return Ok(false),
        };
        if env.is_empty() {
            return Ok(false);
        }
        Ok(std::env::var(env)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false))
    }

    // --- secret vault backfill (M12a-1, 2026-11-15) ------------------------

    /// Which environment variable may hold each config secret.
    ///
    /// A service block already names its own (`api_key_env`) — that is the
    /// documented deployment (`data/env.local`), so it wins. The blocks with
    /// no such field (music, weather when unset, home, openapi, search) fall
    /// back to the conventional uppercase spelling of the secret name, which
    /// is what a hand-written `env.local` would use anyway. Nothing here reads
    /// a value; it is a name list.
    pub fn secret_env_names(&self) -> Result<Vec<(String, String)>> {
        let services = self.load()?;
        let weather = self.weather()?;
        let mut out: Vec<(String, String)> = Vec::new();
        let mut push = |secret: &str, env: &str| {
            let env = env.trim();
            let name = if env.is_empty() {
                secret.replace('.', "_").to_uppercase()
            } else {
                env.to_string()
            };
            out.push((secret.to_string(), name));
        };
        push("llm.api_key", &services.llm.api_key_env);
        push("llm_fallback.api_key", &services.llm_fallback.api_key_env);
        push("asr.api_key", &services.asr.api_key_env);
        push("tts.api_key", &services.tts.api_key_env);
        push("weather.api_key", &weather.api_key_env);
        push("music.library.secret", "");
        push("music.online.secret", "");
        push("home.token", "");
        push("openapi.token", "");
        push("search.api_key", "");
        push(MASTER_BRAIN_TOKEN_SECRET, "");
        push(STORY_TOKEN_SECRET, "");
        push(JOKE_TOKEN_SECRET, "");
        push(RADIO_TOKEN_SECRET, "");
        push(LEARNING_TOKEN_SECRET, "");
        debug_assert_eq!(out.len(), CONFIG_SECRETS.len());
        Ok(out)
    }

    /// M12a-1 startup pass: a credential that only exists in the environment
    /// (`DEEPSEEK_API_KEY`, `VOLC_API_KEY`, …) is invisible to an export or a
    /// fleet push — that is exactly why dot2-d's keys never reached a backup
    /// (2026-11-14). Sealing them into the store once makes them ordinary
    /// household configuration.
    ///
    /// Rules:
    /// - a stored row always wins, even an empty one ("cleared" must not be
    ///   resurrected by an env var that is still exported);
    /// - an empty/whitespace env value is not a credential;
    /// - values are written through `secret_set`, so they land sealed;
    /// - the returned list is names only — nothing here logs a value.
    pub fn backfill_env_secrets(&self) -> Result<Vec<String>> {
        let mut done = Vec::new();
        for (secret, env) in self.secret_env_names()? {
            if self.store.secret_get(&secret)?.is_some() {
                continue;
            }
            let Ok(value) = std::env::var(&env) else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            self.store.secret_set(&secret, value)?;
            done.push(format!("{secret}<-{env}"));
        }
        Ok(done)
    }

    // --- music (R1e) -------------------------------------------------------

    pub fn music(&self) -> Result<MusicConfig> {
        Ok(self.music_snapshot(false)?.config)
    }

    pub(crate) fn music_snapshot(
        &self,
        allow_repair: bool,
    ) -> Result<crate::config_delivery::MusicSnapshot> {
        crate::config_delivery::ConfigDelivery::music_snapshot(&self.store, allow_repair)
    }

    pub fn music_status(&self) -> Result<MusicStatus> {
        Ok(self.music_snapshot(true)?.status())
    }

    /// Absent secrets are preserved. An empty string clears that binding.
    pub fn save_music(
        &self,
        cfg: MusicConfig,
        library_secret: Option<&str>,
        online_secret: Option<&str>,
        actor: &str,
    ) -> std::result::Result<MusicStatus, ConfigError> {
        self.save_music_expected(cfg, library_secret, online_secret, actor, None, false)
    }

    pub(crate) fn save_music_expected(
        &self,
        cfg: MusicConfig,
        library_secret: Option<&str>,
        online_secret: Option<&str>,
        actor: &str,
        expected_revision: Option<&str>,
        preserve_online_user: bool,
    ) -> std::result::Result<MusicStatus, ConfigError> {
        crate::config_delivery::ConfigDelivery::save_music(
            &self.store,
            cfg,
            [library_secret, online_secret],
            actor,
            expected_revision,
            preserve_online_user,
        )
    }

    // --- weather (R1h) -----------------------------------------------------

    pub fn weather(&self) -> Result<WeatherConfig> {
        Ok(self.weather_snapshot()?.config)
    }

    pub(crate) fn weather_snapshot(&self) -> Result<crate::config_delivery::WeatherSnapshot> {
        crate::config_delivery::ConfigDelivery::weather_snapshot(&self.store, false)
    }

    pub fn weather_status(&self) -> Result<WeatherStatus> {
        Ok(crate::config_delivery::ConfigDelivery::weather_snapshot(&self.store, true)?.status())
    }

    /// Omitted credentials preserve the stored key. An empty value clears it.
    pub fn save_weather(
        &self,
        cfg: WeatherConfig,
        api_key: Option<&str>,
        actor: &str,
    ) -> std::result::Result<WeatherStatus, ConfigError> {
        self.save_weather_expected(cfg, api_key, actor, None)
    }

    pub(crate) fn save_weather_expected(
        &self,
        cfg: WeatherConfig,
        api_key: Option<&str>,
        actor: &str,
        expected: Option<&str>,
    ) -> std::result::Result<WeatherStatus, ConfigError> {
        crate::config_delivery::ConfigDelivery::save_local_weather(
            &self.store,
            cfg,
            api_key,
            actor,
            expected,
        )
    }

    pub fn news(&self) -> Result<NewsConfig> {
        crate::config_delivery::ConfigDelivery::news_snapshot(&self.store)
    }

    pub fn news_status(&self) -> Result<NewsStatus> {
        crate::config_delivery::ConfigDelivery::news_status(&self.store)
    }

    pub fn save_news(
        &self,
        cfg: NewsConfig,
        actor: &str,
    ) -> std::result::Result<NewsStatus, ConfigError> {
        self.edit_news(actor, None, |current| *current = cfg)
    }

    pub(crate) fn edit_news(
        &self,
        actor: &str,
        expected: Option<&str>,
        edit: impl FnOnce(&mut NewsConfig),
    ) -> std::result::Result<NewsStatus, ConfigError> {
        crate::config_delivery::ConfigDelivery::edit_news(&self.store, actor, expected, edit)
    }

    pub fn search(&self) -> Result<SearchConfig> {
        Ok(self.search_snapshot()?.0)
    }

    pub(crate) fn search_snapshot(&self) -> Result<(SearchConfig, Option<String>)> {
        crate::config_delivery::ConfigDelivery::search_snapshot(&self.store)
    }

    pub fn personal_endpoint(&self, key: &str) -> Result<PersonalEndpointConfig> {
        let raw = self.store.config_get(key)?;
        Ok(raw
            .and_then(|s| serde_json::from_str::<PersonalEndpointConfig>(&s).ok())
            .unwrap_or_default()
            .normalized())
    }

    pub fn personal_token(&self, secret_key: &str) -> Result<Option<String>> {
        self.store.secret_get(secret_key)
    }

    pub fn save_personal_endpoint(
        &self,
        key: &str,
        secret_key: &str,
        cfg: PersonalEndpointConfig,
        token: Option<&str>,
        actor: &str,
    ) -> std::result::Result<(PersonalEndpointConfig, bool), ConfigError> {
        let cfg = cfg.normalized();
        cfg.validate(key, self.store.allows_loopback_public())?;
        let json = serde_json::to_string(&cfg).map_err(ConfigError::Json)?;
        self.store
            .config_set(key, &json)
            .map_err(ConfigError::Store)?;
        if let Some(tok) = token {
            let trimmed = tok.trim();
            if trimmed.is_empty() {
                self.store
                    .secret_delete(secret_key)
                    .map_err(ConfigError::Store)?;
            } else {
                self.store
                    .secret_set(secret_key, trimmed)
                    .map_err(ConfigError::Store)?;
            }
        }
        let has_token = self
            .store
            .secret_get(secret_key)
            .map_err(ConfigError::Store)?
            .is_some();
        let payload = serde_json::to_value(&cfg).map_err(ConfigError::Json)?;
        let _ = append_history(
            &self.store,
            actor,
            key,
            &format!("{key} endpoint updated (enabled={})", cfg.enabled),
            &payload,
        );
        Ok((cfg, has_token))
    }

    pub fn master_brain(&self) -> Result<MasterBrainConfig> {
        let raw = self.store.config_get(MASTER_BRAIN_KEY)?;
        Ok(raw
            .and_then(|s| serde_json::from_str::<MasterBrainConfig>(&s).ok())
            .unwrap_or_default()
            .normalized())
    }

    pub fn master_brain_token(&self) -> Result<Option<String>> {
        self.store.secret_get(MASTER_BRAIN_TOKEN_SECRET)
    }

    pub fn save_master_brain(
        &self,
        cfg: MasterBrainConfig,
        token: Option<&str>,
        actor: &str,
    ) -> std::result::Result<(MasterBrainConfig, bool), ConfigError> {
        let cfg = cfg.normalized();
        cfg.validate(self.store.allows_loopback_public())?;
        let json = serde_json::to_string(&cfg).map_err(ConfigError::Json)?;
        self.store
            .config_set(MASTER_BRAIN_KEY, &json)
            .map_err(ConfigError::Store)?;
        if let Some(tok) = token {
            let trimmed = tok.trim();
            if trimmed.is_empty() {
                self.store
                    .secret_delete(MASTER_BRAIN_TOKEN_SECRET)
                    .map_err(ConfigError::Store)?;
            } else {
                self.store
                    .secret_set(MASTER_BRAIN_TOKEN_SECRET, trimmed)
                    .map_err(ConfigError::Store)?;
            }
        }
        let has_token = self
            .store
            .secret_get(MASTER_BRAIN_TOKEN_SECRET)
            .map_err(ConfigError::Store)?
            .is_some();
        let payload = serde_json::to_value(&cfg).map_err(ConfigError::Json)?;
        let _ = append_history(
            &self.store,
            actor,
            MASTER_BRAIN_KEY,
            &format!(
                "master_brain updated (enabled={}, protocol={})",
                cfg.enabled,
                cfg.protocol.as_str()
            ),
            &payload,
        );
        Ok((cfg, has_token))
    }

    pub fn search_status(&self) -> Result<SearchStatus> {
        crate::config_delivery::ConfigDelivery::search_status(&self.store)
    }

    pub fn save_search(
        &self,
        cfg: SearchConfig,
        api_key: Option<&str>,
        actor: &str,
    ) -> std::result::Result<SearchStatus, ConfigError> {
        self.edit_search(api_key, actor, None, |current| *current = cfg)
    }

    pub(crate) fn edit_search(
        &self,
        api_key: Option<&str>,
        actor: &str,
        expected: Option<&str>,
        edit: impl FnOnce(&mut SearchConfig),
    ) -> std::result::Result<SearchStatus, ConfigError> {
        crate::config_delivery::ConfigDelivery::edit_search(
            &self.store,
            api_key,
            actor,
            expected,
            edit,
        )
    }

    // --- Shared memory (W3, #80) -------------------------------------------

    pub fn load_shared_memory(&self) -> Result<SharedMemoryConfig> {
        match self.store.config_get(SHARED_MEMORY_KEY)? {
            Some(raw) => Ok(serde_json::from_str(&raw).unwrap_or_default()),
            None => Ok(SharedMemoryConfig::default()),
        }
    }

    pub fn save_shared_memory(
        &self,
        cfg: SharedMemoryConfig,
        actor: &str,
    ) -> std::result::Result<SharedMemoryConfig, ConfigError> {
        self.save_shared_memory_with_token(cfg, None, actor)
    }

    pub fn save_shared_memory_with_token(
        &self,
        cfg: SharedMemoryConfig,
        token: Option<&str>,
        actor: &str,
    ) -> std::result::Result<SharedMemoryConfig, ConfigError> {
        let cfg = cfg.normalized();
        cfg.validate().map_err(ConfigError::BadSharedMemory)?;
        self.store
            .config_set(SHARED_MEMORY_KEY, &serde_json::to_string(&cfg)?)?;
        if let Some(raw_token) = token {
            let trimmed = raw_token.trim();
            if trimmed.is_empty() {
                self.store.secret_delete(SHARED_MEMORY_TOKEN_SECRET)?;
            } else {
                self.store.secret_set(SHARED_MEMORY_TOKEN_SECRET, trimmed)?;
            }
        }
        let token_set = self
            .store
            .secret_get(SHARED_MEMORY_TOKEN_SECRET)
            .ok()
            .flatten()
            .is_some_and(|v| !v.trim().is_empty());
        append_history(
            &self.store,
            actor,
            "shared_memory",
            &format!(
                "enabled={} upload_enabled={} nas_base_url={} token_set={}",
                cfg.enabled, cfg.upload_enabled, cfg.nas_base_url, token_set
            ),
            &serde_json::to_value(&cfg)?,
        )?;
        Ok(cfg)
    }

    // --- OpenAPI query adapter (R2-BM6) ------------------------------------

    pub fn openapi(&self) -> Result<OpenApiConfig> {
        Ok(self.openapi_snapshot()?.config)
    }

    pub(crate) fn openapi_snapshot(&self) -> Result<crate::config_delivery::OpenApiSnapshot> {
        crate::config_delivery::ConfigDelivery::openapi_snapshot(&self.store)
    }

    pub fn openapi_token(&self) -> Result<Option<String>> {
        self.store.secret_get(OPENAPI_TOKEN_SECRET)
    }

    pub fn openapi_revision(&self) -> Result<String> {
        self.store.openapi_revision()
    }

    /// Save a resolved catalogue after structural validation. Explicit token
    /// changes share the execution-revision and history transaction.
    pub fn save_openapi(
        &self,
        cfg: OpenApiConfig,
        token: Option<&str>,
        actor: &str,
    ) -> std::result::Result<OpenApiStatus, ConfigError> {
        self.save_openapi_expected(cfg, token, actor, None)
    }

    pub fn save_openapi_expected(
        &self,
        cfg: OpenApiConfig,
        token: Option<&str>,
        actor: &str,
        expected: Option<&str>,
    ) -> std::result::Result<OpenApiStatus, ConfigError> {
        crate::config_delivery::ConfigDelivery::save_openapi(
            &self.store,
            cfg,
            token,
            actor,
            expected,
        )
    }

    pub fn home(&self) -> Result<HomeConfig> {
        Ok(self.home_snapshot()?.config)
    }

    pub(crate) fn home_snapshot(&self) -> Result<crate::config_delivery::HomeSnapshot> {
        crate::config_delivery::ConfigDelivery::home_snapshot(&self.store)
    }

    pub fn save_home(
        &self,
        cfg: HomeConfig,
        actor: &str,
    ) -> std::result::Result<HomeConfig, ConfigError> {
        Ok(self
            .edit_home(None, actor, |current| *current = cfg)?
            .config)
    }

    pub(crate) fn edit_home(
        &self,
        token: Option<&str>,
        actor: &str,
        edit: impl FnOnce(&mut HomeConfig),
    ) -> std::result::Result<crate::config_delivery::HomeSnapshot, ConfigError> {
        crate::config_delivery::ConfigDelivery::edit_home(&self.store, token, actor, edit)
    }

    /// Write-only token. An omitted value preserves it and an empty value clears it.
    pub fn save_home_token(&self, token: Option<&str>) -> std::result::Result<bool, ConfigError> {
        if token.is_some() {
            return Ok(self.edit_home(token, "home.token", |_| ())?.token.is_some());
        }
        self.home_token_set().map_err(Into::into)
    }

    pub fn home_token(&self) -> Result<Option<String>> {
        self.store.secret_get(HOME_TOKEN_SECRET)
    }

    pub fn home_token_set(&self) -> Result<bool> {
        Ok(self.home_token()?.is_some())
    }

    /// R1j/P1: the device-side reflex vocabulary (one source for the spotter).
    /// The output tone, or the 2026-11-14 default (EchoMuse voicing plus the
    /// taste EQ chosen on the device) when nothing has been saved.
    pub fn audio_eq(&self) -> Result<AudioEqConfig> {
        match self.store.config_get(AUDIO_EQ_KEY)? {
            Some(raw) => Ok(serde_json::from_str(&raw).unwrap_or_default()),
            None => Ok(AudioEqConfig::default()),
        }
    }

    /// Validate → save → history (scope `audio`). The caller pushes the new
    /// tone to the devices; nothing here talks to the gateway.
    pub fn save_audio_eq(
        &self,
        cfg: AudioEqConfig,
        actor: &str,
    ) -> std::result::Result<AudioEqConfig, ConfigError> {
        cfg.validate().map_err(ConfigError::BadAudioEq)?;
        self.store
            .config_set(AUDIO_EQ_KEY, &serde_json::to_string(&cfg)?)?;
        append_history(
            &self.store,
            actor,
            "audio",
            &format!(
                "curve={} bands={}",
                cfg.curve,
                cfg.bands
                    .iter()
                    .map(|b| format!("{}Hz{:+.1}dB Q{:.2}", b.freq, b.gain_db, b.q))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            &serde_json::to_value(&cfg).unwrap_or(serde_json::Value::Null),
        )?;
        Ok(cfg)
    }

    pub fn reflex(&self) -> Result<ReflexConfig> {
        match self.store.config_get(REFLEX_KEY)? {
            Some(raw) => Ok(serde_json::from_str(&raw).unwrap_or_default()),
            None => Ok(ReflexConfig::default()),
        }
    }

    pub(crate) fn validate_reflex_config(
        mut cfg: ReflexConfig,
    ) -> std::result::Result<(ReflexConfig, String), ConfigError> {
        // R1j/P1: tokens are generated from the 汉字 when the caller did not
        // supply them — a word is entered once, not spelled twice.
        for w in &mut cfg.words {
            if w.tokens.trim().is_empty() {
                w.tokens = reflex_tokens(&w.text).map_err(ConfigError::BadReflex)?;
            }
        }
        let compiled = compile_keywords(&cfg).map_err(ConfigError::BadReflex)?;
        Ok((cfg, compiled))
    }

    /// Save the reflex block. Validation happens in [`compile_keywords`]
    /// BEFORE anything is written, so a bad edit never leaves a vocabulary
    /// that compiles into a silently-dead keyword file.
    pub fn save_reflex(
        &self,
        cfg: ReflexConfig,
        actor: &str,
    ) -> std::result::Result<ReflexConfig, ConfigError> {
        let (cfg, compiled) = Self::validate_reflex_config(cfg)?;
        self.store
            .config_set(REFLEX_KEY, &serde_json::to_string(&cfg)?)?;
        append_history(
            &self.store,
            actor,
            "reflex",
            &format!(
                "words={}",
                cfg.words
                    .iter()
                    .map(|w| format!(
                        "{}({}{})",
                        w.text,
                        w.intent.word(),
                        if w.enabled { "" } else { ",off" }
                    ))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            &serde_json::json!({ "words": cfg.words, "compiled": compiled }),
        )?;
        Ok(self.reflex()?)
    }

    /// All household device labels, keyed by stable id.
    pub fn device_meta(&self) -> Result<std::collections::BTreeMap<String, DeviceMeta>> {
        match self.store.config_get(DEVICE_META_KEY)? {
            Some(raw) => Ok(serde_json::from_str(&raw)?),
            None => Ok(Default::default()),
        }
    }

    pub(crate) fn validate_device_meta_entry(
        device: &str,
        name: &str,
        room: &str,
    ) -> std::result::Result<(), ConfigError> {
        let device = device.trim();
        if device.is_empty() || device.chars().count() > 64 || device.chars().any(char::is_control)
        {
            return Err(ConfigError::BadDeviceLabel(
                "device id must be 1–64 visible characters".into(),
            ));
        }
        let name = name.trim();
        let room = room.trim();
        for (kind, value) in [("name", name), ("room", room)] {
            if value.chars().count() > 40 {
                return Err(ConfigError::BadDeviceLabel(format!(
                    "{kind} must be at most 40 characters (got {})",
                    value.chars().count()
                )));
            }
            if value.chars().any(char::is_control) {
                return Err(ConfigError::BadDeviceLabel(format!(
                    "{kind} must not contain control characters"
                )));
            }
        }
        Ok(())
    }

    /// Save one device's display name and room. Empty name AND empty room
    /// removes the entry (reset to the stable id / unassigned room).
    /// Every save lands in the config history with scope `device`.
    pub fn save_device_meta(
        &self,
        device: &str,
        name: &str,
        room: &str,
        actor: &str,
    ) -> std::result::Result<DeviceMeta, ConfigError> {
        Self::validate_device_meta_entry(device, name, room)?;
        let device = device.trim();
        let name = name.trim();
        let room = room.trim();
        let mut all = self.device_meta()?;
        if name.is_empty() && room.is_empty() {
            all.remove(device);
        } else {
            all.insert(
                device.to_string(),
                DeviceMeta {
                    name: name.to_string(),
                    room: room.to_string(),
                },
            );
        }
        self.store
            .config_set(DEVICE_META_KEY, &serde_json::to_string(&all)?)?;
        let meta = all.get(device).cloned().unwrap_or_default();
        append_history(
            &self.store,
            actor,
            "device",
            &format!("{device}: name={:?} room={:?}", meta.name, meta.room),
            &serde_json::json!({
                "device": device,
                "name": meta.name,
                "room": meta.room,
            }),
        )?;
        Ok(meta)
    }

    /// One history row (for the restore endpoint).
    pub fn history_entry(&self, id: i64) -> Result<Option<serde_json::Value>> {
        self.store.with(|c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row(
                "SELECT id, ts_ms, actor, scope, summary, payload FROM config_history WHERE id=?1",
                [id],
                read_history_row,
            )
            .optional()?)
        })
    }

    pub fn history(&self, limit: u32) -> Result<Vec<serde_json::Value>> {
        self.store.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, ts_ms, actor, scope, summary, payload FROM config_history ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = stmt
                .query_map([limit], read_history_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }
}

fn read_history_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value> {
    use serde_json::{json, Value};
    let scope: String = row.get(3)?;
    let mut summary: String = row.get(4)?;
    let mut payload = serde_json::from_str::<Value>(&row.get::<_, String>(5)?).unwrap_or(json!({}));
    if scope == HOME_KEY {
        if let Some(endpoint) = payload.get_mut("base_url") {
            if !endpoint
                .as_str()
                .is_some_and(HomeConfig::endpoint_is_portable)
            {
                *endpoint = Value::Null;
            }
        }
    }
    if scope == MUSIC_KEY {
        for source in ["library", "online"] {
            if let Some(endpoint) = payload.get_mut(source).and_then(|v| v.get_mut("endpoint")) {
                if !endpoint
                    .as_str()
                    .is_some_and(MusicConfig::endpoint_is_portable)
                {
                    *endpoint = Value::Null;
                    summary = "music configuration updated".into();
                }
            }
        }
    }
    Ok(json!({
        "id": row.get::<_, i64>(0)?, "ts_ms": row.get::<_, i64>(1)?,
        "actor": row.get::<_, String>(2)?, "scope": scope,
        "summary": summary, "payload": payload,
    }))
}

/// Append a config-history row from any owner (ConfigService for services,
/// the voice pipeline for mode settings). `payload` is the restorable
/// snapshot of the NEW state — never secrets.
pub fn append_history(
    store: &std::sync::Arc<Store>,
    actor: &str,
    scope: &str,
    summary: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    store.with(|c| {
        c.execute(
            "INSERT INTO config_history (ts_ms, actor, scope, summary, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                crate::id::now_ms() as i64,
                actor,
                scope,
                summary,
                serde_json::to_string(payload).unwrap_or_else(|_| "{}".into())
            ],
        )?;
        Ok(())
    })
}

/// Default DeepSeek base URL. Honors `DEEPSEEK_BASE_URL` when set (the
/// ecosystem convention, e.g. hermes' env) so fresh installs match the
/// machine's known-good endpoint; root path per DeepSeek's own SDK example.
pub fn default_llm_base_url() -> String {
    std::env::var("DEEPSEEK_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "https://api.deepseek.com".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc() -> ConfigService {
        ConfigService::new(std::sync::Arc::new(Store::open_in_memory().unwrap()))
    }

    #[test]
    fn fresh_install_has_deepseek_llm_default() {
        let s = svc();
        let cfg = s.load().unwrap();
        assert_eq!(cfg.llm.provider, "deepseek");
        // deepseek-chat was removed from the provider's catalog (2026-11-07
        // incident report); the alias still routes, but the catalog name is
        // the honest default.
        assert_eq!(cfg.llm.model, "deepseek-flash");
        assert_eq!(cfg.llm.api_key_env, "DEEPSEEK_API_KEY");
    }

    /// 2026-11-14: `api_key_set` means "a key is available", not "a row
    /// exists in the secrets table". The deployment keeps
    /// `DEEPSEEK_API_KEY`/`VOLC_API_KEY` in `data/env.local`, and the console
    /// showed "no key" next to a working model.
    #[test]
    fn a_key_from_the_environment_counts_as_set() {
        let s = svc();
        let mut cfg = s.load().unwrap().tts;
        cfg.provider = "volc".into();
        cfg.model = "seed-tts-2.0".into();
        cfg.api_key_env = "HM_TEST_TTS_KEY_ENV".into();
        s.save_service("tts", cfg, None, "test").unwrap();
        assert!(!s.key_set("tts").unwrap());

        std::env::set_var("HM_TEST_TTS_KEY_ENV", "k");
        assert!(s.key_set("tts").unwrap(), "env var is a real key source");
        assert!(s.status("tts").unwrap().api_key_set);

        // An explicitly cleared secret still shadows the env var; clearing is
        // how a household says "this key is gone".
        s.store.secret_set("tts.api_key", "").unwrap();
        assert!(!s.key_set("tts").unwrap());
        s.store.secret_delete("tts.api_key").unwrap();
        std::env::remove_var("HM_TEST_TTS_KEY_ENV");
        assert!(!s.key_set("tts").unwrap());
    }

    /// M12a-1: a credential that lives only in `env.local` is invisible to an
    /// export or a push. One startup pass seals it into the store, which is
    /// what made dot2-d's keys undeliverable before (2026-11-14).
    #[test]
    fn the_env_backfill_seals_env_only_keys_and_yields_to_stored_ones() {
        let s = svc();
        let mut cfg = s.load().unwrap().llm;
        cfg.api_key_env = "HM_TEST_BACKFILL_LLM_KEY".into();
        s.save_service("llm", cfg, None, "test").unwrap();
        std::env::set_var("HM_TEST_BACKFILL_LLM_KEY", "sk-from-env");
        assert!(s.store.secret_get("llm.api_key").unwrap().is_none());

        let done = s.backfill_env_secrets().unwrap();
        assert!(
            done.iter()
                .any(|d| d.starts_with("llm.api_key<-HM_TEST_BACKFILL_LLM_KEY")),
            "{done:?}"
        );
        // Sealed at rest, readable in code.
        let raw = s.store.secret_raw("llm.api_key").unwrap().unwrap();
        assert!(raw.starts_with(crate::vault::SEALED_PREFIX), "{raw}");
        assert_eq!(
            s.resolve_key("llm").unwrap().as_deref(),
            Some("sk-from-env")
        );

        // A second pass is a no-op; a stored (even empty) row always wins.
        assert!(s.backfill_env_secrets().unwrap().is_empty());
        s.store.secret_set("llm.api_key", "").unwrap();
        assert!(s.backfill_env_secrets().unwrap().is_empty());
        assert_eq!(
            s.store.secret_get("llm.api_key").unwrap().as_deref(),
            Some("")
        );

        std::env::remove_var("HM_TEST_BACKFILL_LLM_KEY");
    }

    #[test]
    fn a_cleared_env_var_backfills_nothing() {
        let s = svc();
        let mut cfg = s.load().unwrap().asr;
        cfg.api_key_env = "HM_TEST_BACKFILL_ASR_EMPTY".into();
        s.save_service("asr", cfg, None, "test").unwrap();
        std::env::set_var("HM_TEST_BACKFILL_ASR_EMPTY", "   ");
        assert!(
            s.backfill_env_secrets().unwrap().is_empty(),
            "whitespace is not a key"
        );
        std::env::remove_var("HM_TEST_BACKFILL_ASR_EMPTY");
    }

    #[test]
    fn saving_one_service_leaves_others() {
        let s = svc();
        s.save_service(
            "tts",
            ServiceConfig {
                provider: "mock".into(),
                base_url: "".into(),
                model: "m1".into(),
                api_key_env: "TTS_KEY".into(),
                voice: String::new(),
                thinking: String::new(),
            },
            Some("secret-value"),
            "console",
        )
        .unwrap();
        let st = s.status("tts").unwrap();
        assert_eq!(st.provider, "mock");
        assert!(st.api_key_set);
        // value never re-appears in status
        let raw = serde_json::to_string(&st).unwrap();
        assert!(!raw.contains("secret-value"));
        // llm untouched
        assert_eq!(s.status("llm").unwrap().provider, "deepseek");
        assert_eq!(s.history(10).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_key_clears_the_stored_one() {
        let s = svc();
        let cfg = ServiceConfig {
            provider: "deepseek".into(),
            base_url: String::new(),
            model: "m".into(),
            api_key_env: "DEEPSEEK_API_KEY".into(),
            voice: String::new(),
            thinking: String::new(),
        };
        s.save_service("llm", cfg.clone(), Some("stored-key"), "console")
            .unwrap();
        assert!(s.status("llm").unwrap().api_key_set);
        // "" = clear: the env var must be able to answer again.
        let st = s.save_service("llm", cfg, Some(""), "console").unwrap();
        assert!(!st.api_key_set, "an empty key clears the stored secret");
        assert!(s.store.secret_get("llm.api_key").unwrap().is_none());
    }

    #[test]
    fn device_meta_roundtrip_history_and_validation() {
        let s = svc();
        assert!(s.device_meta().unwrap().is_empty());
        s.save_device_meta("dot2-d", "客厅小达", "客厅", "console")
            .unwrap();
        let all = s.device_meta().unwrap();
        assert_eq!(all["dot2-d"].name, "客厅小达");
        assert_eq!(all["dot2-d"].room, "客厅");
        let hist = s.history(10).unwrap();
        assert_eq!(hist[0]["scope"], "device");
        assert!(hist[0]["payload"]["device"] == "dot2-d");

        // 40 is the limit; 41 and control characters are refused.
        assert!(s
            .save_device_meta("dot2-d", &"x".repeat(41), "", "console")
            .is_err());
        assert!(s.save_device_meta("dot2-d", "a\nb", "", "console").is_err());
        assert!(s.save_device_meta("", "x", "", "console").is_err());

        // Clearing both fields removes the entry (fall back to stable id).
        s.save_device_meta("dot2-d", "", "", "console").unwrap();
        assert!(s.device_meta().unwrap().is_empty());
    }

    #[test]
    fn unknown_service_rejected() {
        assert!(svc()
            .save_service("sms", ServiceConfig::default(), None, "x")
            .is_err());
    }
}

#[cfg(test)]
mod reflex_tests {
    use super::*;

    fn word(text: &str, tokens: &str, intent: ReflexIntent) -> ReflexWord {
        ReflexWord {
            text: text.into(),
            tokens: tokens.into(),
            intent,
            enabled: true,
        }
    }

    /// RJ-1: the file is COMPILED from the block — one line per enabled
    /// word, in order, with the device's own token spelling.
    #[test]
    fn the_default_table_compiles_to_the_device_file() {
        let file = compile_keywords(&ReflexConfig::default()).unwrap();
        let expected = concat!(
            "n ǐ h ǎo g āo d á @你好高达\n",
            "z àn t íng b ō f àng @暂停播放\n",
            "j ì x ù b ō f àng @继续播放\n",
            "x ià y ì sh ǒu @下一首\n",
            "t íng zh ǐ b ō f àng @停止播放\n",
            "t uì x ià @退下\n",
            "t uì x ià b a @退下吧\n",
        );
        assert_eq!(file, expected);
        // Disabled words are absent, not commented out.
        let mut cfg = ReflexConfig::default();
        cfg.words[1].enabled = false;
        let file = compile_keywords(&cfg).unwrap();
        assert!(!file.contains("暂停播放"));
        assert!(file.contains("继续播放"));
    }

    /// A bad file kills the WHOLE vocabulary on the device (measured), so
    /// every refusal must be explicit.
    #[test]
    fn bad_vocabularies_are_refused_by_name() {
        let mut cfg = ReflexConfig::default();
        for w in cfg.words.iter_mut() {
            w.enabled = false;
        }
        assert!(compile_keywords(&cfg).unwrap_err().contains("没有启用的词"));

        // A short word is legal (the text layer executes it); the trade-off
        // is a warning, not a refusal.
        let short = ReflexConfig {
            words: vec![word("拜拜", "b ài b ài", ReflexIntent::Retreat)],
        };
        assert!(compile_keywords(&short).is_ok());
        let warns = reflex_warnings(&short);
        // Audit R2-24: the retreat-only vocabulary keeps compiling (the text
        // layer can still wake it), but the "no wake phrase" caution comes
        // first — deploying it silently is how a room stops answering.
        assert_eq!(warns.len(), 2);
        assert!(
            warns[0].contains("唤醒") && warns[0].contains("兜底"),
            "{warns:?}"
        );
        assert!(
            warns[1].contains("拜拜") && warns[1].contains("转写"),
            "{warns:?}"
        );

        let dup = ReflexConfig {
            words: vec![
                word("暂停播放", "z àn t íng b ō f àng", ReflexIntent::Pause),
                word("暂停播放", "z àn t íng b ō f àng", ReflexIntent::Pause),
            ],
        };
        assert!(compile_keywords(&dup).unwrap_err().contains("重复"));

        let bad = ReflexConfig {
            words: vec![word(
                "暂停 播放",
                "z àn t íng b ō f àng",
                ReflexIntent::Pause,
            )],
        };
        assert!(compile_keywords(&bad).is_err());
    }

    /// The hash is what the device reports back: one comparison proves the
    /// file it holds is the file this core compiled.
    #[test]
    fn the_hash_is_stable_and_content_addressed() {
        let a = compile_keywords(&ReflexConfig::default()).unwrap();
        let b = compile_keywords(&ReflexConfig::default()).unwrap();
        assert_eq!(fnv1a_hex(a.as_bytes()), fnv1a_hex(b.as_bytes()));
        assert_ne!(fnv1a_hex(a.as_bytes()), fnv1a_hex(b"other"));
    }

    /// The reflex vocabulary round-trips through the store (backup/restore
    /// uses the same KV) and a bad save never reaches the store.
    #[test]
    fn saving_validates_before_writing() {
        let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
        let svc = ConfigService::new(store);
        // Only a genuinely invalid table is refused now (a 1-token / empty
        // word); a short word is a warning.
        let bad = ReflexConfig {
            words: vec![word(" ", "t uì x ià", ReflexIntent::Retreat)],
        };
        assert!(svc.save_reflex(bad, "test").is_err());
        assert_eq!(
            svc.reflex().unwrap().words,
            default_reflex_words(),
            "refused save left no trace"
        );
        let good = ReflexConfig {
            words: default_reflex_words(),
        };
        svc.save_reflex(good.clone(), "test").unwrap();
        assert_eq!(svc.reflex().unwrap(), good);
    }
}

#[cfg(test)]
mod token_tests {
    use super::reflex_tokens;

    /// R1j/P1: the generator must reproduce the spellings that were measured
    /// on the device — these three are the shipped vocabulary, and a
    /// hand-typed 退下 once silenced the whole file by splitting `uì`.
    #[test]
    fn the_shipped_words_generate_their_measured_tokens() {
        assert_eq!(reflex_tokens("你好高达").unwrap(), "n ǐ h ǎo g āo d á");
        assert_eq!(reflex_tokens("暂停播放").unwrap(), "z àn t íng b ō f àng");
        assert_eq!(reflex_tokens("继续播放").unwrap(), "j ì x ù b ō f àng");
        assert_eq!(reflex_tokens("退下").unwrap(), "t uì x ià");
        assert_eq!(
            reflex_tokens("关闭儿童房的灯").unwrap(),
            "g uān b ì ér t óng f áng d e d ēng"
        );
    }

    /// Tone sandhi the speaker actually uses: 一 before a 4th tone is yí,
    /// otherwise yì (`暂停一下` must be `y í`, not `y ī`); 不 before a 4th tone
    /// is bú.
    #[test]
    fn tone_sandhi_is_applied() {
        assert_eq!(reflex_tokens("暂停一下").unwrap(), "z àn t íng y í x ià");
        assert_eq!(reflex_tokens("一起").unwrap(), "y ì q ǐ");
        assert_eq!(reflex_tokens("不对").unwrap(), "b ú d uì");
    }

    /// The table's unusual finals are one token each, not two.
    #[test]
    fn multi_letter_finals_and_initials_stay_whole() {
        assert_eq!(reflex_tokens("女").unwrap(), "n ǚ");
        assert_eq!(reflex_tokens("略").unwrap(), "l üè");
        assert_eq!(reflex_tokens("吃").unwrap(), "ch ī");
        assert_eq!(reflex_tokens("有").unwrap(), "y ǒu");
        assert_eq!(reflex_tokens("我").unwrap(), "w ǒ");
        assert_eq!(reflex_tokens("爱").unwrap(), "ài");
    }

    /// Non-Hanzi input is refused by name (a Latin word has no device token).
    #[test]
    fn non_hanzi_has_no_spelling() {
        assert!(reflex_tokens("ok").is_err());
        assert!(reflex_tokens("暂停。").unwrap_err().contains("。"));
        assert!(reflex_tokens("").is_err());
    }

    /// M6c-2: the default tone is the sound chosen by ear on the device
    /// (EchoMuse voicing + -6/+4/+2 taste EQ) and must validate; anything a
    /// console might send outside the gateway's clamp is refused here.
    #[test]
    fn the_default_audio_eq_validates_and_bad_curves_are_refused() {
        let d = super::AudioEqConfig::default();
        assert_eq!(d.curve, "echomuse");
        assert_eq!(d.bands.len(), 3);
        assert!(d.validate().is_ok());

        let mut bad = d.clone();
        bad.curve = "loud".into();
        assert!(bad.validate().unwrap_err().contains("curve"));

        let mut bad = d.clone();
        bad.bands[0].freq = 20_000.0;
        assert!(bad.validate().unwrap_err().contains("freq"));

        let mut bad = d.clone();
        bad.bands[0].gain_db = 24.0;
        assert!(bad.validate().unwrap_err().contains("gain"));

        let mut bad = d.clone();
        bad.bands[0].q = 0.1;
        assert!(bad.validate().unwrap_err().contains("Q"));

        let mut bad = d;
        bad.curve = "custom".into();
        bad.bands.clear();
        assert!(bad.validate().unwrap_err().contains("at least one band"));
    }
}
