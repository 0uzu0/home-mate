use super::{
    discovery::{persist, revision, text, Block},
    service::value_tree,
    ConfigDelivery, DeliveryError, Fields, Layers, Result,
};
use crate::{
    store::Store,
    tool::remote_socket::{validate_remote_config, RemoteSocketConfig, REMOTE_SOCKET_SECRET_KEY},
};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use serde_json::Value;

const BLOCK: Block = Block::RemoteSocket;

#[derive(Default)]
pub(crate) struct RemoteSocketSnapshot {
    pub config: RemoteSocketConfig,
    pub token: Option<String>,
    pub revision: String,
    pub verified: bool,
}

impl ConfigDelivery {
    /// Capture the exact configuration and local credential used by an operation.
    pub(crate) fn remote_socket_snapshot(store: &Store) -> Result<RemoteSocketSnapshot> {
        database(store, |c| {
            let tx = c.unchecked_transaction()?;
            let token = active_token(&tx, store)?;
            let raw = text(&tx, BLOCK).map_err(DeliveryError::internal)?;
            let config = raw.as_deref().map_or_else(
                || Some(RemoteSocketConfig::default()),
                |raw| project(raw, token.as_deref()),
            );
            let verified = config.is_some();
            Ok(RemoteSocketSnapshot {
                config: config.unwrap_or_default(),
                token,
                verified,
                revision: revision(&tx, BLOCK, raw.as_deref()).map_err(DeliveryError::internal)?,
            })
        })
    }

    pub(crate) fn save_remote_socket(
        store: &Store,
        mut config: RemoteSocketConfig,
        replacement: Option<&str>,
        actor: &str,
        expected: Option<&str>,
        restore_node: Option<&str>,
    ) -> Result<RemoteSocketSnapshot> {
        validate_remote_config(&mut config).map_err(|error| {
            DeliveryError::new("INVALID_REMOTE_SOCKET_CONFIG", error).at(BLOCK.key())
        })?;
        let replacement = replacement.map(str::trim);
        let sealed = replacement
            .filter(|token| !token.is_empty())
            .map(|token| store.seal_secret(REMOTE_SOCKET_SECRET_KEY, token))
            .transpose()
            .map_err(DeliveryError::internal)?;

        database(store, |c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            if let Some(node) = restore_node {
                let current = crate::fleet::FleetService::stable_self_id_on(&tx)
                    .map_err(DeliveryError::internal)?;
                if current.as_deref() != Some(node) {
                    return Err(DeliveryError::conflict(
                        "REMOTE_SOCKET_RESTORE_TARGET_MISMATCH",
                        "Remote socket backups can only be restored on their source node",
                    )
                    .at(BLOCK.key()));
                }
            }
            let raw = text(&tx, BLOCK).map_err(DeliveryError::internal)?;
            let current = revision(&tx, BLOCK, raw.as_deref()).map_err(DeliveryError::internal)?;
            if expected.is_some_and(|expected| expected != current) {
                return Err(DeliveryError::conflict(
                    "REMOTE_SOCKET_REVISION_CONFLICT",
                    "Remote socket settings changed. Refresh before saving.",
                )
                .at(BLOCK.key()));
            }
            let existing = active_token(&tx, store)?;
            reject_exposed_token(&config, existing.as_deref())?;
            if replacement.is_none() {
                check_source_binding(&tx, &config)?;
            }
            if let Some(token) = replacement.filter(|token| !token.is_empty()) {
                reject_exposed_token(&config, Some(token))?;
                if public_history_contains_token(&tx, token)? {
                    return Err(exposed_token());
                }
            }
            if let Some(sealed) = sealed.as_deref() {
                tx.execute(
                    "INSERT INTO secrets(name,value) VALUES(?1,?2)
                    ON CONFLICT(name) DO UPDATE SET value=excluded.value",
                    params![REMOTE_SOCKET_SECRET_KEY, sealed],
                )?;
            } else if replacement.is_some() {
                tx.execute(
                    "DELETE FROM secrets WHERE name=?1",
                    [REMOTE_SOCKET_SECRET_KEY],
                )?;
            }
            let revision = persist(&tx, BLOCK, &config, actor).map_err(DeliveryError::internal)?;
            tx.commit()?;
            Ok(RemoteSocketSnapshot {
                config,
                revision,
                verified: true,
                token: replacement
                    .map(str::to_owned)
                    .or(existing)
                    .filter(|token| !token.is_empty()),
            })
        })
    }
}

fn database<T>(store: &Store, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    store
        .with(|connection| Ok(f(connection)))
        .map_err(DeliveryError::internal)?
}

pub(super) fn active_token(c: &Connection, store: &Store) -> Result<Option<String>> {
    store
        .secret_get_on(c, REMOTE_SOCKET_SECRET_KEY)
        .map(|token| token.filter(|token| !token.is_empty()))
        .map_err(DeliveryError::internal)
}

pub(super) fn project(raw: &str, token: Option<&str>) -> Option<RemoteSocketConfig> {
    let cfg = serde_json::from_str::<RemoteSocketConfig>(raw).ok()?;
    let mut normalized = cfg.clone();
    validate_remote_config(&mut normalized).ok()?;
    (normalized == cfg && !contains_token(&cfg, token)).then_some(cfg)
}

pub(super) fn contains(value: &Value, token: &str) -> bool {
    match value {
        Value::String(s) => s.contains(token),
        Value::Array(values) => values.iter().any(|value| contains(value, token)),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| key.contains(token) || contains(value, token)),
        _ => false,
    }
}

pub(super) fn contains_token(config: &RemoteSocketConfig, token: Option<&str>) -> bool {
    token
        .filter(|token| !token.is_empty())
        .is_some_and(|token| {
            config.endpoint_url.contains(token)
                || config.service_name.contains(token)
                || config.capabilities.iter().any(|capability| {
                    capability.name.contains(token)
                        || capability.description.contains(token)
                        || contains(&capability.parameters, token)
                })
        })
}

fn exposed_token() -> DeliveryError {
    DeliveryError::new(
        "REMOTE_SOCKET_SECRET_IN_PUBLIC_CONFIG",
        "Remote socket settings and receipts must contain only public values",
    )
    .at(BLOCK.key())
}

fn reject_exposed_token(config: &RemoteSocketConfig, token: Option<&str>) -> Result<()> {
    if contains_token(config, token) {
        return Err(exposed_token());
    }
    Ok(())
}

fn public_history_contains_token(c: &Connection, token: &str) -> Result<bool> {
    let mut query = c.prepare(
        "SELECT resolved_json FROM config_head
         UNION ALL SELECT json_extract(layers_json, '$.node') FROM config_head
         UNION ALL SELECT before_json FROM config_plans
         UNION ALL SELECT after_json FROM config_plans
         UNION ALL SELECT json_extract(before_json, '$.fields') FROM config_applications
         UNION ALL SELECT json_extract(after_json, '$.fields') FROM config_applications
         UNION ALL SELECT json_extract(before_json, '$.layers.node') FROM config_applications
         UNION ALL SELECT json_extract(after_json, '$.layers.node') FROM config_applications",
    )?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let Some(fields) = row.get::<_, Option<String>>(0)? else {
            continue;
        };
        let fields: Fields = serde_json::from_str(&fields)?;
        let config = serde_json::from_value(value_tree(&fields, BLOCK.key()))?;
        if contains_token(&config, Some(token)) {
            return Ok(true);
        }
    }
    let mut history = c.prepare("SELECT payload FROM config_history WHERE scope=?1")?;
    let mut rows = history.query([BLOCK.key()])?;
    while let Some(row) = rows.next()? {
        let value: Value = serde_json::from_str(&row.get::<_, String>(0)?)?;
        if contains_token(&serde_json::from_value(value)?, Some(token)) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn check_source_binding(c: &Connection, desired: &RemoteSocketConfig) -> Result<()> {
    let raw = text(c, BLOCK).map_err(DeliveryError::internal)?;
    let current_url = raw
        .as_deref()
        .and_then(|raw| {
            serde_json::from_str::<RemoteSocketConfig>(raw)
                .ok()
                .map(|current| current.endpoint_url)
        })
        .unwrap_or_default();
    if current_url.trim().trim_end_matches('/') != desired.endpoint_url
        && c.query_row(
            "SELECT EXISTS(SELECT 1 FROM secrets WHERE name=?1)",
            [REMOTE_SOCKET_SECRET_KEY],
            |row| row.get::<_, bool>(0),
        )?
    {
        return Err(DeliveryError::conflict("REMOTE_SOCKET_SOURCE_BINDING_CONFLICT",
            "Apply endpoint changes and explicitly replace or clear the credential in the node editor.").at(BLOCK.key()));
    }
    Ok(())
}

pub(super) fn check_publication(
    c: &Connection,
    store: &Store,
    config: &RemoteSocketConfig,
) -> Result<()> {
    reject_exposed_token(config, active_token(c, store)?.as_deref())?;
    check_source_binding(c, config)
}

pub(super) fn review_layers(
    c: &Connection,
    store: &Store,
    layers: &Layers,
    resolved: &Fields,
) -> Result<()> {
    let token = active_token(c, store)?;
    let layer = serde_json::from_value(value_tree(&layers.node, BLOCK.key()))?;
    reject_exposed_token(&layer, token.as_deref())?;
    let value = value_tree(resolved, BLOCK.key());
    BLOCK.validate(value.clone(), store.allows_loopback_public())?;
    let config = serde_json::from_value(value)?;
    reject_exposed_token(&config, token.as_deref())?;
    check_source_binding(c, &config)
}
