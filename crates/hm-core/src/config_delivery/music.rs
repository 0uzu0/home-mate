use super::{
    discovery::{decode, record_history, text, write_config, Block},
    ConfigDelivery,
};
use crate::{
    config::{
        ConfigError, MusicConfig, MusicConnectionStatus, MusicStatus, MUSIC_LIBRARY_SECRET,
        MUSIC_ONLINE_SECRET,
    },
    store::{self, Store},
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::json;

const KEYS: [&str; 2] = [MUSIC_LIBRARY_SECRET, MUSIC_ONLINE_SECRET];

pub(crate) struct MusicSnapshot {
    pub config: MusicConfig,
    pub library: Option<String>,
    pub online: Option<String>,
    pub revision: String,
}

impl MusicSnapshot {
    pub(crate) fn status(self) -> MusicStatus {
        let conn = |c: crate::config::MusicConnection, set: bool| MusicConnectionStatus {
            endpoint: if MusicConfig::endpoint_is_portable(&c.endpoint) {
                c.endpoint
            } else {
                String::new()
            },
            user: c.user,
            secret_set: set,
        };
        MusicStatus {
            enabled: self.config.enabled,
            prefer_local: self.config.prefer_local,
            library: self.config.library.map(|c| conn(c, self.library.is_some())),
            online: self.config.online.map(|c| conn(c, self.online.is_some())),
            revision: self.revision,
        }
    }
}

fn revision(c: &Connection, raw: Option<&str>) -> store::Result<String> {
    let mut ciphertext = [None, None];
    let mut legacy = None;
    for (slot, name) in ciphertext.iter_mut().zip(KEYS) {
        let stored: Option<String> = c
            .query_row("SELECT value FROM secrets WHERE name=?1", [name], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(value) = stored {
            if crate::vault::is_sealed(&value) {
                *slot = Some(value);
            } else if legacy.is_none() {
                legacy = Some(c.query_row(
                    "SELECT revision FROM config_head WHERE id=1",
                    [],
                    |r| r.get::<_, i64>(0),
                )?);
            }
        }
    }
    super::digest(&json!([raw, ciphertext, legacy]))
        .map_err(|_| store::StoreError::Authority("cannot compute music revision".into()))
}

fn read(store: &Store, c: &Connection, allow_repair: bool) -> store::Result<MusicSnapshot> {
    let raw = text(c, Block::Music)?;
    let config: MusicConfig = decode(raw.as_deref()).or_else(|err| {
        if allow_repair {
            Ok(MusicConfig::default())
        } else {
            Err(err)
        }
    })?;
    let config = match config
        .clone()
        .normalized_with(store.allows_loopback_public())
    {
        Ok(normalized) => normalized,
        Err(_) if allow_repair => config,
        Err(error) => return Err(store::StoreError::Authority(error.to_string())),
    };
    Ok(MusicSnapshot {
        config,
        library: store.secret_get_on(c, MUSIC_LIBRARY_SECRET)?,
        online: store.secret_get_on(c, MUSIC_ONLINE_SECRET)?,
        revision: revision(c, raw.as_deref())?,
    })
}

impl ConfigDelivery {
    pub(crate) fn music_snapshot(
        store: &Store,
        allow_repair: bool,
    ) -> store::Result<MusicSnapshot> {
        store.with(|c| {
            let tx = c.unchecked_transaction()?;
            read(store, &tx, allow_repair)
        })
    }

    pub(crate) fn save_music(
        store: &Store,
        cfg: MusicConfig,
        proposed: [Option<&str>; 2],
        actor: &str,
        expected: Option<&str>,
        preserve_online_user: bool,
    ) -> Result<MusicStatus, ConfigError> {
        let allow_loopback = store.allows_loopback_public();
        let mut cfg = cfg.normalized_with(allow_loopback)?;
        let mut sealed = [None, None];
        for ((slot, key), value) in sealed.iter_mut().zip(KEYS).zip(proposed) {
            *slot = value
                .filter(|value| !value.is_empty())
                .map(|value| store.seal_secret(key, value))
                .transpose()?;
        }
        store.with(|c| Ok((|| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate).map_err(store::StoreError::from)?;
            let raw = text(&tx, Block::Music)?;
            if let Some(expected) = expected {
                let actual = revision(&tx, raw.as_deref())?;
                if actual != expected {
                    return Err(ConfigError::ServiceConflict { revision: actual });
                }
            }
            if let Some(online) = cfg.online.as_mut().filter(|_| preserve_online_user) {
                let previous: MusicConfig = decode(raw.as_deref()).unwrap_or_default();
                if let Some(previous) = previous.online {
                    online.user = previous.user;
                    online.normalize_with("online", allow_loopback)?;
                }
            }
            for ((name, value), change) in KEYS.into_iter().zip(sealed).zip(proposed) {
                if let Some(value) = value {
                    tx.execute("INSERT INTO secrets(name,value) VALUES (?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value", params![name, value]).map_err(store::StoreError::from)?;
                } else if change.is_some() {
                    tx.execute("DELETE FROM secrets WHERE name=?1", [name]).map_err(store::StoreError::from)?;
                }
            }
            write_config(&tx, Block::Music, &cfg)?;
            let saved = read(store, &tx, false)?.status();
            let payload = json!({
                "enabled": saved.enabled,
                "prefer_local": saved.prefer_local,
                "library": saved.library,
                "online": saved.online,
            });
            record_history(&tx, Block::Music, actor, &payload.to_string())?;
            tx.commit().map_err(store::StoreError::from)?;
            Ok(saved)
        })()))?
    }
}
