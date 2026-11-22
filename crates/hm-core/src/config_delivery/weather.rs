use super::{service::raw, ConfigDelivery, DeliveryError};
use crate::{
    config::{ConfigError, WeatherConfig, WeatherStatus, WEATHER_KEY, WEATHER_SECRET},
    store::{self, Store},
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};

/// The same published values are used by the weather tools and the management
/// probe. The credential remains private to the operation.
pub(crate) struct WeatherSnapshot {
    pub config: WeatherConfig,
    pub key: Option<String>,
    pub revision: String,
    key_set: bool,
}

impl WeatherSnapshot {
    fn read(store: &Store, c: &Connection, allow_repair: bool) -> store::Result<Self> {
        let text = local_text(c)?;
        let revision = revision(c, text.as_deref())?;
        let decoded = text
            .as_deref()
            .map(serde_json::from_str::<WeatherConfig>)
            .transpose();
        let config = if allow_repair {
            decoded.unwrap_or_default().unwrap_or_default()
        } else {
            decoded?.unwrap_or_default()
        };
        let stored = store.secret_get_on(c, WEATHER_SECRET)?;
        let key_set = stored.is_some();
        let key = stored.or_else(|| {
            std::env::var(&config.api_key_env)
                .ok()
                .filter(|s| !s.trim().is_empty())
        });
        Ok(Self {
            config,
            key,
            revision,
            key_set,
        })
    }

    pub fn status(self) -> WeatherStatus {
        let cfg = self.config;
        let state = cfg.state_word().into();
        WeatherStatus {
            enabled: cfg.enabled,
            provider: cfg.provider,
            endpoint: if WeatherConfig::endpoint_is_portable(&cfg.endpoint) {
                cfg.endpoint
            } else {
                String::new()
            },
            api_key_env: cfg.api_key_env,
            api_key_set: self.key_set,
            places: cfg.places,
            default_alias: cfg.default_alias,
            max_days: cfg.max_days,
            state,
            revision: self.revision,
        }
    }
}

fn local_text(c: &Connection) -> store::Result<Option<String>> {
    Ok(c.query_row(
        "SELECT value FROM config_kv WHERE key=?1",
        [WEATHER_KEY],
        |r| r.get(0),
    )
    .optional()?)
}

fn revision(c: &Connection, text: Option<&str>) -> store::Result<String> {
    let sealed: Option<String> = c
        .query_row(
            "SELECT value FROM secrets WHERE name=?1",
            [WEATHER_SECRET],
            |r| r.get(0),
        )
        .optional()?;
    // Older plaintext rows are fenced by the database revision until
    // startup reseals them. No plaintext enters the digest.
    let plaintext_revision = if sealed
        .as_deref()
        .is_some_and(|value| !crate::vault::is_sealed(value))
    {
        Some(
            c.query_row("SELECT revision FROM config_head WHERE id=1", [], |r| {
                r.get::<_, i64>(0)
            })?,
        )
    } else {
        None
    };
    let ciphertext = sealed
        .as_deref()
        .filter(|value| crate::vault::is_sealed(value));
    super::digest(&json!([text, ciphertext, plaintext_revision]))
        .map_err(|_| store::StoreError::Authority("cannot compute weather revision".into()))
}

impl ConfigDelivery {
    pub(crate) fn weather_snapshot(
        store: &Store,
        allow_repair: bool,
    ) -> store::Result<WeatherSnapshot> {
        store.with(|c| {
            let tx = c.unchecked_transaction()?;
            let snapshot = WeatherSnapshot::read(store, &tx, allow_repair)?;
            tx.commit()?;
            Ok(snapshot)
        })
    }

    pub(crate) fn save_local_weather(
        store: &Store,
        cfg: WeatherConfig,
        api_key: Option<&str>,
        actor: &str,
        expected: Option<&str>,
    ) -> Result<WeatherStatus, ConfigError> {
        let cfg = cfg.normalized(store.allows_loopback_public())?;
        let sealed = api_key
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(|key| store.seal_secret(WEATHER_SECRET, key))
            .transpose()?;
        store.with(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            if let Some(expected) = expected {
                let actual = revision(&tx, local_text(&tx)?.as_deref())?;
                if expected != actual {
                    return Ok(Err(ConfigError::WeatherConflict { revision: actual }));
                }
            }
            tx.execute(
                "INSERT INTO config_kv(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![WEATHER_KEY, serde_json::to_string(&cfg)?],
            )?;
            if let Some(value) = &sealed {
                tx.execute(
                    "INSERT INTO secrets(name,value) VALUES (?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value",
                    params![WEATHER_SECRET, value],
                )?;
            } else if api_key.is_some() {
                tx.execute("DELETE FROM secrets WHERE name=?1", [WEATHER_SECRET])?;
            }
            let after = WeatherSnapshot::read(store, &tx, false)?;
            let payload = json!({
                "enabled": cfg.enabled, "provider": cfg.provider, "endpoint": cfg.endpoint,
                "api_key_env": cfg.api_key_env, "api_key_set": after.key_set, "places": cfg.places,
                "default_alias": cfg.default_alias, "max_days": cfg.max_days,
            });
            tx.execute(
                "INSERT INTO config_history(ts_ms,actor,scope,summary,payload) VALUES (?1,?2,?3,?4,?5)",
                params![crate::id::now_ms() as i64, actor, WEATHER_KEY,
                    format!("enabled={} places={} default={} max_days={}", cfg.enabled, cfg.places.len(), cfg.default_alias, cfg.max_days),
                    payload.to_string()],
            )?;
            tx.commit()?;
            Ok(Ok(after.status()))
        })?
    }
}

pub(super) fn published(c: &Connection) -> super::Result<Option<WeatherConfig>> {
    Ok(raw(c, WEATHER_KEY)?
        .as_deref()
        .and_then(|text| serde_json::from_str::<WeatherConfig>(text).ok())
        .filter(|cfg| WeatherConfig::endpoint_is_portable(&cfg.endpoint)))
}

pub(super) fn require_readable_baseline(c: &Connection) -> super::Result<()> {
    if let Some(text) = raw(c, WEATHER_KEY)? {
        let safe = serde_json::from_str::<WeatherConfig>(&text)
            .is_ok_and(|cfg| WeatherConfig::endpoint_is_portable(&cfg.endpoint));
        if !safe {
            return Err(
            DeliveryError::conflict(
                "WEATHER_BASELINE_UNREADABLE",
                "saved weather configuration cannot be safely packaged; repair it in weather settings",
            )
            .at(WEATHER_KEY));
        }
    }
    Ok(())
}

pub(super) fn validate(value: Value, allow_loopback: bool) -> super::Result<()> {
    let cfg: WeatherConfig = serde_json::from_value(value)?;
    let normalized = cfg
        .clone()
        .normalized(allow_loopback)
        .map_err(|e| DeliveryError::new("INVALID_WEATHER", e.to_string()).at(WEATHER_KEY))?;
    if normalized != cfg {
        return Err(DeliveryError::new(
            "INVALID_WEATHER",
            "weather configuration must use canonical values",
        )
        .at(WEATHER_KEY));
    }
    Ok(())
}

pub(super) fn validate_operation(op: &super::Operation, allow_loopback: bool) -> super::Result<()> {
    use super::Operation;
    match op {
        Operation::Set { path, .. } if path == "weather.places" => Err(DeliveryError::new(
            "INVALID_OPERATION",
            "weather places require a whole-list replacement",
        )
        .at(path)),
        Operation::Replace { path, value } if path == "weather.places" => {
            let items = value.as_array().ok_or_else(|| {
                DeliveryError::new("INVALID_FIELD_TYPE", "expected places array").at(path)
            })?;
            super::v1::validate_array_elements(path, items)?;
            validate(json!({"places": value}), allow_loopback)
        }
        Operation::Set { path, value } if path == "weather.endpoint" => {
            let endpoint = value.as_str().unwrap_or_default();
            validate(json!({"endpoint": endpoint}), allow_loopback)
        }
        Operation::Set { path, value } if path == "weather.provider" => {
            validate(json!({"provider": value}), allow_loopback)
        }
        _ => Ok(()),
    }
}
