//! Weather (R1h): places, provider and the bounded forecast window.
//!
//! A place is stored as COORDINATES, not as a free-text city: a tool call must
//! never trigger a geocode lookup with arbitrary user text, and a resolved place
//! keeps answering when the geocoder is down.

use serde::{Deserialize, Serialize};

use super::{
    endpoint::{valid_place_alias, valid_timezone},
    valid_public_endpoint, ConfigError,
};

/// Weather (R1h): its own block too — places, provider and the bounded
/// forecast window are not a ServiceConfig shape, and a household with no
/// weather service must still have a working assistant.
pub const WEATHER_KEY: &str = "weather";
pub const WEATHER_SECRET: &str = "weather.api_key";

// --- weather (R1h) ---------------------------------------------------------

/// One place the household asks about. Stored as COORDINATES, not as a
/// free-text city: a tool call must never trigger a geocode lookup with
/// arbitrary user text (that is an outbound query per guess), and a resolved
/// place keeps answering when the geocoder is down.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WeatherPlace {
    /// What the person calls it ("家", "公司"). Required, unique.
    pub alias: String,
    /// The provider's resolved display name ("杭州市西湖区"). Informational.
    #[serde(default)]
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    /// IANA zone the provider should phrase local times in ("" = auto).
    #[serde(default)]
    pub timezone: String,
    /// 中国天气网城市代码（备用源用，如沈阳 101070101）。空＝该地点没有备用源。
    #[serde(default)]
    pub city_code: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherConfig {
    #[serde(default)]
    pub enabled: bool,
    /// `open_meteo` (default, no key required). The same response shape is
    /// served by Open-Meteo's commercial/keyed host — point `endpoint` at it
    /// and store a key instead of adding a second parser.
    #[serde(default = "default_weather_provider")]
    pub provider: String,
    /// "" = the provider's own default host; otherwise `http(s)://…`.
    #[serde(default)]
    pub endpoint: String,
    /// Env var consulted when no stored key exists ("" for keyless use).
    #[serde(default)]
    pub api_key_env: String,
    /// At most `MAX_WEATHER_PLACES` entries, unique aliases.
    #[serde(default)]
    pub places: Vec<WeatherPlace>,
    /// The place a bare "天气怎么样" means. "" with exactly one place = it.
    #[serde(default)]
    pub default_alias: String,
    /// Bound on `weather.forecast`'s `days` (1–7).
    #[serde(default = "default_weather_days")]
    pub max_days: u8,
}

pub const MAX_WEATHER_PLACES: usize = 8;

pub(super) fn default_weather_provider() -> String {
    "open_meteo".into()
}

fn default_weather_days() -> u8 {
    3
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_weather_provider(),
            endpoint: String::new(),
            api_key_env: String::new(),
            places: Vec::new(),
            default_alias: String::new(),
            max_days: default_weather_days(),
        }
    }
}

impl WeatherConfig {
    /// Weather requests append provider parameters and the separately stored
    /// key. A configured URL is a base endpoint and carries no query data.
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

    /// Usable = switched on AND has at least one place. A service with no
    /// place cannot answer, so it must not be advertised as ready.
    pub fn is_configured(&self) -> bool {
        self.enabled && !self.places.is_empty()
    }

    /// The state word the console card, the probe and the prompt all use.
    /// ONE vocabulary: never set up = `unconfigured`, set up but switched off
    /// = `disabled`, otherwise `ready`.
    pub fn state_word(&self) -> &'static str {
        if self.places.is_empty() {
            "unconfigured"
        } else if !self.enabled {
            "disabled"
        } else {
            "ready"
        }
    }

    /// The place a request means: an explicit alias, else the configured
    /// default, else the only place. None = ask which place.
    pub fn resolve_place(&self, alias: Option<&str>) -> Option<&WeatherPlace> {
        match alias.map(str::trim).filter(|a| !a.is_empty()) {
            Some(a) => self
                .places
                .iter()
                .find(|p| p.alias.eq_ignore_ascii_case(a) || p.alias == a),
            None => {
                if !self.default_alias.is_empty() {
                    self.places.iter().find(|p| p.alias == self.default_alias)
                } else if self.places.len() == 1 {
                    self.places.first()
                } else {
                    None
                }
            }
        }
    }
}

/// Read-back shape: coordinates are visible, the key is not.
#[derive(Debug, Clone, Serialize)]
pub struct WeatherStatus {
    pub enabled: bool,
    pub provider: String,
    pub endpoint: String,
    pub api_key_env: String,
    pub api_key_set: bool,
    pub places: Vec<WeatherPlace>,
    pub default_alias: String,
    pub max_days: u8,
    /// `ready` | `unconfigured` | `disabled` — the same words the prompt uses.
    pub state: String,
    /// Opaque revision of this block (config + whether a key is set), sent
    /// back by the console so a stale form cannot overwrite a newer save.
    pub revision: String,
}

impl WeatherConfig {
    /// Normalize legacy form values before either a local or reviewed write.
    pub(crate) fn normalized(mut self, allow_loopback: bool) -> Result<Self, ConfigError> {
        let cfg = &mut self;
        let raw_provider = cfg.provider.trim().to_ascii_lowercase();
        let canonical_provider = match raw_provider.as_str() {
            "" | "open_meteo" | "open-meteo" => "open_meteo",
            "met_no" | "met-no" | "met_norway" | "met-norway" => "met_no",
            "wttr_in" | "wttr-in" | "wttr" => "wttr_in",
            _ => {
                return Err(ConfigError::UnsupportedWeatherProvider(
                    cfg.provider.clone(),
                ));
            }
        };
        cfg.provider = canonical_provider.to_string();
        cfg.endpoint = cfg.endpoint.trim().to_string();
        if !Self::endpoint_is_portable(&cfg.endpoint)
            || !valid_public_endpoint(&cfg.endpoint, allow_loopback)
        {
            return Err(ConfigError::BadWeatherEndpoint);
        }
        cfg.api_key_env = cfg.api_key_env.trim().to_string();
        if cfg.max_days == 0 || cfg.max_days > 7 {
            return Err(ConfigError::BadWeatherDays(cfg.max_days));
        }
        if cfg.places.len() > MAX_WEATHER_PLACES {
            return Err(ConfigError::BadWeatherPlace(format!(
                "最多 {MAX_WEATHER_PLACES} 个地点"
            )));
        }
        let mut seen: Vec<String> = Vec::new();
        for p in cfg.places.iter_mut() {
            p.alias = p.alias.trim().to_string();
            p.name = p.name.trim().chars().take(40).collect();
            p.timezone = p.timezone.trim().to_string();
            if !valid_place_alias(&p.alias) {
                return Err(ConfigError::BadWeatherPlace(format!(
                    "别名「{}」不合法：1–20 字，不含引号/斜杠",
                    p.alias
                )));
            }
            if seen.iter().any(|a| a.eq_ignore_ascii_case(&p.alias)) {
                return Err(ConfigError::BadWeatherPlace(format!(
                    "别名「{}」重复",
                    p.alias
                )));
            }
            seen.push(p.alias.clone());
            if !p.latitude.is_finite() || !(-90.0..=90.0).contains(&p.latitude) {
                return Err(ConfigError::BadWeatherPlace(format!(
                    "{}: 纬度超出 −90…90",
                    p.alias
                )));
            }
            if !p.longitude.is_finite() || !(-180.0..=180.0).contains(&p.longitude) {
                return Err(ConfigError::BadWeatherPlace(format!(
                    "{}: 经度超出 −180…180",
                    p.alias
                )));
            }
            if !valid_timezone(&p.timezone) {
                return Err(ConfigError::BadWeatherPlace(format!(
                    "{}: 时区形如 Asia/Shanghai",
                    p.alias
                )));
            }
        }
        cfg.default_alias = cfg.default_alias.trim().to_string();
        if !cfg.default_alias.is_empty() && !cfg.places.iter().any(|p| p.alias == cfg.default_alias)
        {
            return Err(ConfigError::BadWeatherPlace(format!(
                "默认地点「{}」不在列表里",
                cfg.default_alias
            )));
        }
        Ok(self)
    }
}
