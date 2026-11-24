//! Published settings for Home, Music, News, Search and OpenAPI.
use super::{service::raw, ConfigDelivery, DeliveryError, Operation};
use crate::config::{
    ConfigError, HomeConfig, HomeEntity, MusicConfig, MusicConnection, NewsConfig, NewsSource,
    NewsStatus, OpenApiConfig, SearchConfig, SearchStatus, SEARCH_SECRET,
};
use crate::store::{self, Store};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Copy)]
pub(super) enum Block {
    Music,
    Home,
    News,
    Search,
    OpenApi,
    OpenSockets,
}

impl Block {
    pub(super) const ALL: [Self; 6] = [
        Self::Home,
        Self::Music,
        Self::News,
        Self::Search,
        Self::OpenApi,
        Self::OpenSockets,
    ];

    pub(super) fn named(name: &str) -> Option<Self> {
        match name {
            "music" => Some(Self::Music),
            "home" => Some(Self::Home),
            "news" => Some(Self::News),
            "search" => Some(Self::Search),
            "openapi" => Some(Self::OpenApi),
            "open_sockets" => Some(Self::OpenSockets),
            _ => None,
        }
    }

    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Music => "music",
            Self::News => "news",
            Self::Search => "search",
            Self::OpenApi => "openapi",
            Self::OpenSockets => super::open_sockets::KEY,
        }
    }

    pub(super) fn defaults(self) -> Value {
        match self {
            Self::Home => json!(HomeConfig::default()),
            Self::Music => json!(MusicConfig::default()),
            Self::News => json!(NewsConfig::default()),
            Self::Search => json!(SearchConfig::default()),
            Self::OpenApi => json!(OpenApiConfig::default()),
            Self::OpenSockets => json!([]),
        }
    }

    pub(super) fn published(self, c: &Connection, store: &Store) -> super::Result<Option<Value>> {
        let Some(text) = raw(c, self.key())? else {
            return Ok(None);
        };
        let typed = match self {
            Self::Music => serde_json::from_str::<MusicConfig>(&text)
                .ok()
                .and_then(|cfg| cfg.normalized_with(store.allows_loopback_public()).ok())
                .map(|cfg| json!(cfg)),
            Self::Home => serde_json::from_str::<HomeConfig>(&text)
                .ok()
                .map(|cfg| json!(cfg)),
            Self::News => serde_json::from_str::<NewsConfig>(&text)
                .ok()
                .map(|cfg| json!(cfg)),
            Self::Search => serde_json::from_str::<SearchConfig>(&text)
                .ok()
                .map(|cfg| json!(cfg)),
            Self::OpenApi => {
                let token = store
                    .secret_get_on(c, crate::config::OPENAPI_TOKEN_SECRET)
                    .map_err(DeliveryError::internal)?;
                serde_json::from_str::<OpenApiConfig>(&text)
                    .ok()
                    .filter(|cfg| {
                        super::openapi::reject_exposed_token(cfg, token.as_deref()).is_ok()
                    })
                    .map(|cfg| json!(cfg))
            }
            Self::OpenSockets => {
                let candidate = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|value| super::open_sockets::canonical(value).ok());
                if let Some(value) = candidate {
                    super::open_sockets::check_public(c, store, &value)
                        .ok()
                        .map(|_| json!(value))
                } else {
                    None
                }
            }
        };
        Ok(typed.filter(|value| {
            self.validate(value.clone(), store.allows_loopback_public())
                .is_ok()
        }))
    }

    pub(super) fn require_readable(self, c: &Connection, store: &Store) -> super::Result<()> {
        if raw(c, self.key())?.is_some() && self.published(c, store)?.is_none() {
            return Err(DeliveryError::conflict(
                "SERVICE_BASELINE_UNREADABLE",
                "saved configuration cannot be safely packaged; repair it in service settings",
            )
            .at(self.key()));
        }
        Ok(())
    }

    pub(super) fn validate(self, value: Value, allow_loopback: bool) -> super::Result<()> {
        let bad = |error: ConfigError| {
            let code = match self {
                Self::Home => "INVALID_HOME_CONFIG",
                Self::Music => "INVALID_MUSIC_CONFIG",
                _ => "INVALID_DISCOVERY_CONFIG",
            };
            DeliveryError::new(code, error.to_string()).at(self.key())
        };
        let canonical = match self {
            Self::Music => {
                let cfg: MusicConfig = serde_json::from_value(value)?;
                cfg.clone().normalized_with(allow_loopback).map_err(bad)? == cfg
            }
            Self::Home => {
                let cfg: HomeConfig = serde_json::from_value(value)?;
                cfg.clone().normalized_with(allow_loopback).map_err(bad)? == cfg
            }
            Self::News => {
                let cfg: NewsConfig = serde_json::from_value(value)?;
                cfg.clone().normalized(allow_loopback).map_err(bad)? == cfg
            }
            Self::Search => {
                let cfg: SearchConfig = serde_json::from_value(value)?;
                cfg.clone().normalized(allow_loopback).map_err(bad)? == cfg
            }
            Self::OpenApi => {
                let cfg: OpenApiConfig = serde_json::from_value(value)?;
                if !super::portable_service_url(&cfg.spec_url)
                    || !super::portable_service_url(&cfg.base_url)
                {
                    return Err(DeliveryError::new("INVALID_OPENAPI_CONFIG", "OpenAPI URLs must exclude inline credentials, authentication query parameters and fragments").at(self.key()));
                }
                cfg.clone().normalized(allow_loopback).map_err(|_| {
                    DeliveryError::new(
                        "INVALID_OPENAPI_CONFIG",
                        "invalid OpenAPI catalogue or endpoint",
                    )
                    .at(self.key())
                })? == cfg
            }
            Self::OpenSockets => {
                super::open_sockets::canonical(value)?;
                true
            }
        };
        if !canonical {
            return Err(DeliveryError::new(
                match self {
                    Self::Home => "INVALID_HOME_CONFIG",
                    Self::Music => "INVALID_MUSIC_CONFIG",
                    Self::OpenApi => "INVALID_OPENAPI_CONFIG",
                    _ => "INVALID_DISCOVERY_CONFIG",
                },
                "configuration must use canonical values",
            )
            .at(self.key()));
        }
        Ok(())
    }

    pub(super) fn checked_value(
        self,
        op: &Operation,
        allow_loopback: bool,
    ) -> super::Result<Option<Value>> {
        match op {
            Operation::Set { path, value }
                if matches!(path.as_str(), "music.library" | "music.online")
                    && !value.is_null() =>
            {
                Err(DeliveryError::new(
                    "INVALID_OPERATION",
                    "replace the whole music connection or set it to null",
                )
                .at(path))
            }
            Operation::Replace { path, value }
                if matches!(path.as_str(), "music.library" | "music.online") =>
            {
                let connection = value.as_object().ok_or_else(|| {
                    DeliveryError::new("INVALID_FIELD_TYPE", "expected connection object").at(path)
                })?;
                for (name, value) in connection {
                    let field = format!("{path}.{name}");
                    let desc = super::inventory::descriptor(&field)
                        .filter(|_| matches!(name.as_str(), "endpoint" | "user"))
                        .ok_or_else(|| {
                            DeliveryError::new(
                                "UNKNOWN_FIELD",
                                "unregistered music connection field",
                            )
                            .at(&field)
                        })?;
                    super::inventory::validate_descriptor_value(&desc, &field, value)?;
                }
                if !(connection.contains_key("endpoint") && connection.contains_key("user")) {
                    return Err(DeliveryError::new(
                        "INCOMPLETE_MUSIC_CONNECTION",
                        "a replacement requires both endpoint and user",
                    )
                    .at(path));
                }
                let field = path.split('.').nth(1).unwrap_or_default();
                self.validate(json!({ (field): value }), allow_loopback)?;
                let canonical: MusicConnection = serde_json::from_value(value.clone())?;
                Ok(Some(serde_json::to_value(canonical)?))
            }

            Operation::Set { path, .. } if path == super::open_sockets::KEY => {
                Err(DeliveryError::new(
                    "INVALID_OPERATION",
                    "open sockets require a whole-list replacement",
                )
                .at(path))
            }
            Operation::Replace { path, value } if path == super::open_sockets::KEY => {
                let items = value.as_array().ok_or_else(|| {
                    DeliveryError::new(
                        "INVALID_FIELD_TYPE",
                        "expected an array of socket contracts",
                    )
                    .at(path)
                })?;
                super::v1::validate_array_elements(path, items)?;
                Ok(Some(serde_json::to_value(super::open_sockets::canonical(
                    value.clone(),
                )?)?))
            }
            Operation::Set { path, .. } if path == "openapi.operations" => Err(DeliveryError::new(
                "INVALID_OPERATION",
                "OpenAPI operations require a whole-list replacement",
            )
            .at(path)),
            Operation::Replace { path, value } if path == "openapi.operations" => {
                let items = value.as_array().ok_or_else(|| {
                    DeliveryError::new("INVALID_FIELD_TYPE", "expected operations array").at(path)
                })?;
                super::v1::validate_array_elements(path, items)?;
                let operations: Vec<crate::config::OpenApiOperation> =
                    serde_json::from_value(value.clone())?;
                Ok(Some(serde_json::to_value(operations)?))
            }
            Operation::Set { path, .. } if path == "home.entities" => Err(DeliveryError::new(
                "INVALID_OPERATION",
                "home entities require a whole-list replacement",
            )
            .at(path)),
            Operation::Replace { path, value } if path == "home.entities" => {
                let items = value.as_array().ok_or_else(|| {
                    DeliveryError::new("INVALID_FIELD_TYPE", "expected entities array").at(path)
                })?;
                super::v1::validate_array_elements(path, items)?;
                self.validate(json!({"entities": value}), allow_loopback)?;
                let entities: Vec<HomeEntity> = serde_json::from_value(value.clone())?;
                Ok(Some(serde_json::to_value(entities)?))
            }
            Operation::Set { path, value } if path == "home.base_url" => {
                self.validate(json!({"base_url": value}), allow_loopback)?;
                Ok(None)
            }
            Operation::Set { path, .. } if path == "news.sources" => Err(DeliveryError::new(
                "INVALID_OPERATION",
                "news sources require a whole-list replacement",
            )
            .at(path)),
            Operation::Replace { path, value } if path == "news.sources" => {
                let items = value.as_array().ok_or_else(|| {
                    DeliveryError::new("INVALID_FIELD_TYPE", "expected sources array").at(path)
                })?;
                super::v1::validate_array_elements(path, items)?;
                self.validate(
                    json!({ "sources": value, "max_sources": crate::config::MAX_NEWS_SOURCES_CAP }),
                    allow_loopback,
                )?;
                // Materialize native defaults for older v1 source entries so
                // the plan and native consumer hash the same field shape.
                let sources: Vec<NewsSource> = serde_json::from_value(value.clone())?;
                Ok(Some(serde_json::to_value(sources)?))
            }
            Operation::Set { path, value } if path == "search.base_url" => {
                self.validate(json!({"base_url": value}), allow_loopback)?;
                Ok(None)
            }
            _ => Ok(None),
        }
    }
}

pub(super) fn text(c: &Connection, block: Block) -> store::Result<Option<String>> {
    Ok(c.query_row(
        "SELECT value FROM config_kv WHERE key=?1",
        [block.key()],
        |r| r.get(0),
    )
    .optional()?)
}

pub(super) fn decode<T: DeserializeOwned + Default>(text: Option<&str>) -> store::Result<T> {
    Ok(text
        .map(serde_json::from_str::<T>)
        .transpose()?
        .unwrap_or_default())
}

pub(super) fn revision(c: &Connection, block: Block, raw: Option<&str>) -> store::Result<String> {
    let credential = match block {
        Block::Search => Some(SEARCH_SECRET),
        _ => None,
    };
    let key: Option<String> = if let Some(name) = credential {
        c.query_row("SELECT value FROM secrets WHERE name=?1", [name], |r| {
            r.get(0)
        })
        .optional()?
    } else {
        None
    };
    let legacy = if key
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
    let sealed = key
        .as_deref()
        .filter(|value| crate::vault::is_sealed(value));
    super::digest(&json!([raw, sealed, legacy]))
        .map_err(|_| store::StoreError::Authority("cannot compute discovery revision".into()))
}

fn require_revision(
    c: &Connection,
    block: Block,
    raw: Option<&str>,
    expected: Option<&str>,
) -> Result<(), ConfigError> {
    if let Some(expected) = expected {
        let actual = revision(c, block, raw)?;
        if expected != actual {
            return Err(ConfigError::ServiceConflict { revision: actual });
        }
    }
    Ok(())
}

pub(super) fn persist(
    c: &Connection,
    block: Block,
    cfg: &impl Serialize,
    actor: &str,
) -> store::Result<String> {
    let encoded = write_config(c, block, cfg)?;
    record_history(c, block, actor, &encoded)?;
    revision(c, block, Some(&encoded))
}

pub(super) fn write_config(
    c: &Connection,
    block: Block,
    cfg: &impl Serialize,
) -> store::Result<String> {
    let encoded = serde_json::to_string(cfg)?;
    c.execute("INSERT INTO config_kv(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![block.key(), encoded])?;
    Ok(encoded)
}

pub(super) fn record_history(
    c: &Connection,
    block: Block,
    actor: &str,
    payload: &str,
) -> store::Result<()> {
    let name = block.key();
    c.execute(
        "INSERT INTO config_history(ts_ms,actor,scope,summary,payload) VALUES (?1,?2,?3,?4,?5)",
        params![
            crate::id::now_ms() as i64,
            actor,
            name,
            format!("{name} configuration updated"),
            payload
        ],
    )?;
    Ok(())
}

fn news_status(mut cfg: NewsConfig, revision: String) -> NewsStatus {
    cfg.sources
        .retain(|source| super::portable_service_url(&source.url));
    if !cfg
        .sources
        .iter()
        .any(|source| source.alias == cfg.default_alias)
    {
        cfg.default_alias.clear();
    }
    let state = cfg.state_word().into();
    NewsStatus {
        enabled: cfg.enabled,
        sources: cfg.sources,
        default_alias: cfg.default_alias,
        timeout_ms: cfg.timeout_ms,
        max_sources: cfg.max_sources,
        state,
        revision,
    }
}

fn search_status(cfg: SearchConfig, key: Option<&str>, revision: String) -> SearchStatus {
    let api_key_set = key.is_some_and(|key| !key.trim().is_empty());
    let state = cfg.state_word(api_key_set).into();
    let base_url = if super::portable_service_url(&cfg.base_url) {
        cfg.base_url
    } else {
        String::new()
    };
    SearchStatus {
        enabled: cfg.enabled,
        base_url,
        count: cfg.count,
        timeout_ms: cfg.timeout_ms,
        api_key_set,
        state,
        revision,
    }
}

impl ConfigDelivery {
    pub(crate) fn news_snapshot(store: &Store) -> store::Result<NewsConfig> {
        store.with(|c| decode(text(c, Block::News)?.as_deref()))
    }

    pub(crate) fn news_status(store: &Store) -> store::Result<NewsStatus> {
        store.with(|c| {
            let tx = c.unchecked_transaction()?;
            let raw = text(&tx, Block::News)?;
            let version = revision(&tx, Block::News, raw.as_deref())?;
            Ok(news_status(
                decode(raw.as_deref()).unwrap_or_default(),
                version,
            ))
        })
    }

    pub(crate) fn search_snapshot(store: &Store) -> store::Result<(SearchConfig, Option<String>)> {
        store.with(|c| {
            let tx = c.unchecked_transaction()?;
            let cfg = decode(text(&tx, Block::Search)?.as_deref())?;
            let key = store
                .secret_get_on(&tx, SEARCH_SECRET)?
                .filter(|key| !key.trim().is_empty());
            Ok((cfg, key))
        })
    }

    pub(crate) fn search_status(store: &Store) -> store::Result<SearchStatus> {
        store.with(|c| {
            let tx = c.unchecked_transaction()?;
            let raw = text(&tx, Block::Search)?;
            let version = revision(&tx, Block::Search, raw.as_deref())?;
            let key = store.secret_get_on(&tx, SEARCH_SECRET)?;
            Ok(search_status(
                decode(raw.as_deref()).unwrap_or_default(),
                key.as_deref(),
                version,
            ))
        })
    }

    pub(crate) fn edit_news(
        store: &Store,
        actor: &str,
        expected: Option<&str>,
        edit: impl FnOnce(&mut NewsConfig),
    ) -> Result<NewsStatus, ConfigError> {
        store.with(|c| {
            Ok((|| {
                let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)
                    .map_err(store::StoreError::from)?;
                let raw = text(&tx, Block::News)?;
                require_revision(&tx, Block::News, raw.as_deref(), expected)?;
                let mut cfg = decode(raw.as_deref()).unwrap_or_default();
                edit(&mut cfg);
                let cfg = cfg.normalized(store.allows_loopback_public())?;
                let version = persist(&tx, Block::News, &cfg, actor)?;
                tx.commit().map_err(store::StoreError::from)?;
                Ok(news_status(cfg, version))
            })())
        })?
    }

    pub(crate) fn edit_search(
        store: &Store,
        api_key: Option<&str>,
        actor: &str,
        expected: Option<&str>,
        edit: impl FnOnce(&mut SearchConfig),
    ) -> Result<SearchStatus, ConfigError> {
        let sealed = api_key
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(|key| store.seal_secret(SEARCH_SECRET, key))
            .transpose()?;
        store.with(|c| Ok((|| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate).map_err(store::StoreError::from)?;
            let raw = text(&tx, Block::Search)?;
            require_revision(&tx, Block::Search, raw.as_deref(), expected)?;
            let mut cfg = decode(raw.as_deref()).unwrap_or_default();
            edit(&mut cfg);
            let cfg = cfg.normalized(store.allows_loopback_public())?;
            if let Some(sealed) = sealed {
                tx.execute("INSERT INTO secrets(name,value) VALUES (?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value", params![SEARCH_SECRET, sealed]).map_err(store::StoreError::from)?;
            } else if api_key.is_some() {
                tx.execute("DELETE FROM secrets WHERE name=?1", [SEARCH_SECRET]).map_err(store::StoreError::from)?;
            }
            let version = persist(&tx, Block::Search, &cfg, actor)?;
            let key = store.secret_get_on(&tx, SEARCH_SECRET)?;
            tx.commit().map_err(store::StoreError::from)?;
            Ok(search_status(cfg, key.as_deref(), version))
        })()))?
    }
}
