use super::{
    discovery::{text, Block},
    ConfigDelivery, DeliveryError, Result,
};
use crate::{
    api::AppState,
    store::Store,
    tool::{
        open_socket::{
            secret_key, validate_contract, OpenSocketContract, OpenSocketRuntime, MAX_OPEN_SOCKETS,
        },
        Registry,
    },
};
use rusqlite::Connection;
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc};

pub(super) const KEY: &str = "open_sockets";

#[derive(Clone)]
pub(super) struct Provider {
    pub runtime: Arc<OpenSocketRuntime>,
    pub registry: Arc<Registry>,
}

impl Provider {
    pub fn from_app(state: &AppState) -> Self {
        Self {
            runtime: state.open_sockets.clone(),
            registry: state.registry.clone(),
        }
    }

    pub fn competing_names(&self) -> Result<Vec<String>> {
        self.registry
            .competing_tool_names(self.runtime.as_ref())
            .map_err(collision)
    }

    pub fn effective(
        &self,
        store: &Store,
        source: Option<&str>,
    ) -> Option<Vec<OpenSocketContract>> {
        let contracts = self.runtime.contracts();
        let catalog = self.registry.catalog().ok()?;
        if !contracts
            .iter()
            .all(|contract| catalog.contains(&contract.name))
        {
            return None;
        }
        database(store, |c| {
            let tx = c.unchecked_transaction()?;
            if let Some(source) = source {
                check_source_node(&tx, source)?;
            }
            let native: Vec<OpenSocketContract> = match Block::OpenSockets.published(&tx, store)? {
                Some(value) => serde_json::from_value(value)?,
                None if text(&tx, Block::OpenSockets)
                    .map_err(DeliveryError::internal)?
                    .is_none() =>
                {
                    Vec::new()
                }
                None => return Ok(None),
            };
            Ok((native == contracts).then_some(contracts))
        })
        .ok()
        .flatten()
    }
}

fn collision(detail: impl Into<String>) -> DeliveryError {
    DeliveryError::conflict("OPEN_SOCKETS_CATALOG_CONFLICT", detail).at(KEY)
}

pub(super) fn validate_namespace(candidate: &[OpenSocketContract], peers: &[String]) -> Result<()> {
    if let Some(conflict) = candidate.iter().find(|entry| peers.contains(&entry.name)) {
        return Err(collision(format!(
            "tool '{}' is already registered",
            conflict.name
        )));
    }
    Ok(())
}

pub(super) fn canonical(value: Value) -> Result<Vec<OpenSocketContract>> {
    let mut candidate: Vec<OpenSocketContract> = serde_json::from_value(value)?;
    let invalid =
        |detail: String| DeliveryError::new("INVALID_OPEN_SOCKETS_CONFIG", detail).at(KEY);
    if candidate.len() > MAX_OPEN_SOCKETS {
        return Err(invalid(format!(
            "maximum {MAX_OPEN_SOCKETS} open sockets allowed"
        )));
    }
    let mut names = BTreeSet::new();
    for socket in &mut candidate {
        let original = socket.clone();
        validate_contract(socket).map_err(&invalid)?;
        if *socket != original {
            return Err(invalid(
                "a socket contract contains non-canonical fields".into(),
            ));
        }
        if !names.insert(socket.name.clone()) {
            return Err(collision(format!(
                "duplicate socket name '{}'",
                socket.name
            )));
        }
    }
    Ok(candidate)
}

fn check_bindings_for_edit(
    c: &Connection,
    store: &Store,
    desired: &[OpenSocketContract],
    replacement: Option<(&str, &str)>,
) -> Result<()> {
    let previous = text(c, Block::OpenSockets)
        .map_err(DeliveryError::internal)?
        .map(|raw| serde_json::from_str::<Vec<OpenSocketContract>>(&raw))
        .transpose()?
        .unwrap_or_default();
    let names: BTreeSet<&str> = previous
        .iter()
        .chain(desired)
        .map(|entry| entry.name.as_str())
        .collect();
    check_public(c, store, desired)?;
    let document = serde_json::to_value(desired)?;
    if let Some((_, token)) = replacement.filter(|(_, token)| !token.is_empty()) {
        if contains(&document, token)
            || contains(&serde_json::to_value(&previous)?, token)
            || public_history_contains(c, token)?
        {
            return Err(exposed_token());
        }
    }
    for name in names {
        let key = secret_key(name);
        if replacement.is_some_and(|(changed, _)| changed == key) {
            continue;
        }
        let present: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM secrets WHERE name=?1)",
            [&key],
            |row| row.get(0),
        )?;
        if !present {
            continue;
        }
        let before = previous.iter().find(|entry| entry.name == name);
        let after = desired.iter().find(|entry| entry.name == name);
        if !matches!((before, after), (Some(a), Some(b))
            if a.same_credential_source(b))
        {
            return Err(DeliveryError::conflict(
                "OPEN_SOCKETS_SOURCE_BINDING_CONFLICT",
                "clear or rebind the node-local credential before removing the socket or changing its source",
            ).at(KEY));
        }
    }
    Ok(())
}

pub(super) fn check_public(
    c: &Connection,
    store: &Store,
    candidate: &[OpenSocketContract],
) -> Result<()> {
    let document = serde_json::to_value(candidate)?;
    let mut query = c.prepare("SELECT name FROM secrets WHERE name GLOB 'open_socket.token.*'")?;
    let keys = query.query_map([], |row| row.get::<_, String>(0))?;
    for key in keys {
        if let Some(token) = store
            .secret_get_on(c, &key?)
            .map_err(DeliveryError::internal)?
        {
            if !token.is_empty() && contains(&document, &token) {
                return Err(exposed_token());
            }
        }
    }
    Ok(())
}

fn exposed_token() -> DeliveryError {
    DeliveryError::new(
        "OPEN_SOCKETS_SECRET_IN_PUBLIC_CONFIG",
        "socket contracts must contain only public values",
    )
    .at(KEY)
}

fn public_history_contains(c: &Connection, token: &str) -> Result<bool> {
    let mut query = c.prepare(
        "SELECT json_extract(resolved_json, '$.open_sockets') FROM config_head
         UNION ALL SELECT json_extract(layers_json, '$.node.open_sockets') FROM config_head
         UNION ALL SELECT json_extract(before_json, '$.open_sockets') FROM config_plans
         UNION ALL SELECT json_extract(after_json, '$.open_sockets') FROM config_plans
         UNION ALL SELECT json_extract(before_json, '$.fields.open_sockets') FROM config_applications
         UNION ALL SELECT json_extract(after_json, '$.fields.open_sockets') FROM config_applications
         UNION ALL SELECT json_extract(before_json, '$.layers.node.open_sockets') FROM config_applications
         UNION ALL SELECT json_extract(after_json, '$.layers.node.open_sockets') FROM config_applications",
    )?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        if let Some(raw) = row.get::<_, Option<String>>(0)? {
            if contains(&serde_json::from_str(&raw)?, token) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn contains(value: &Value, token: &str) -> bool {
    match value {
        Value::String(s) => s.contains(token),
        Value::Array(values) => values.iter().any(|value| contains(value, token)),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| key.contains(token) || contains(value, token)),
        _ => false,
    }
}

pub(super) fn check_bindings(
    c: &Connection,
    store: &Store,
    desired: &[OpenSocketContract],
) -> Result<()> {
    check_bindings_for_edit(c, store, desired, None)
}

impl ConfigDelivery {
    pub(crate) fn check_open_socket_edit_on(
        store: &Store,
        c: &Connection,
        raw: &str,
        replacement: Option<(&str, &str)>,
    ) -> crate::store::Result<()> {
        let desired: Vec<OpenSocketContract> = serde_json::from_str(raw)?;
        check_bindings_for_edit(c, store, &desired, replacement).map_err(|error| {
            crate::store::StoreError::Authority(format!("{}: {}", error.code, error.detail))
        })
    }

    pub(crate) fn open_sockets_export(
        state: &AppState,
        node: &str,
    ) -> Option<Vec<OpenSocketContract>> {
        Provider::from_app(state).effective(&state.store, Some(node))
    }

    pub(crate) fn prepare_open_sockets_restore(
        state: &AppState,
        value: Value,
        source: &str,
    ) -> Result<Vec<OpenSocketContract>> {
        let provider = Provider::from_app(state);
        let entries = value.as_array().ok_or_else(|| {
            DeliveryError::new(
                "INVALID_FIELD_TYPE",
                "open_sockets must be a list of socket contracts",
            )
            .at(KEY)
        })?;
        super::v1::validate_array_elements(KEY, entries)?;
        let desired = canonical(value)?;
        validate_namespace(&desired, &provider.competing_names()?)?;
        database(&state.store, |c| {
            let tx = c.unchecked_transaction()?;
            check_source_node(&tx, source)?;
            check_bindings(&tx, &state.store, &desired)
        })?;
        Ok(desired)
    }

    pub(crate) fn import_open_sockets(
        state: &AppState,
        desired: Vec<OpenSocketContract>,
        source: &str,
        actor: &str,
    ) -> Result<()> {
        let provider = Provider::from_app(state);
        provider
            .runtime
            .publish_reviewed(&provider.registry, desired.clone(), || {
                database(&state.store, |c| {
                    let tx = rusqlite::Transaction::new_unchecked(
                        c,
                        rusqlite::TransactionBehavior::Immediate,
                    )?;
                    check_source_node(&tx, source)?;
                    check_bindings(&tx, &state.store, &desired)?;
                    let previous = text(&tx, Block::OpenSockets)
                        .map_err(DeliveryError::internal)?
                        .map(|raw| serde_json::from_str::<Vec<OpenSocketContract>>(&raw))
                        .transpose()?
                        .unwrap_or_default();
                    if previous != desired {
                        publish_on(&tx, &desired, actor)?;
                    }
                    tx.commit()?;
                    Ok(true)
                })
            })
    }

    pub(crate) fn open_sockets_restore_mismatch() -> DeliveryError {
        restore_target_mismatch()
    }
}

fn database<T>(store: &Store, run: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    store
        .with(|c| Ok(run(c)))
        .map_err(DeliveryError::internal)?
}

fn check_source_node(c: &Connection, source: &str) -> Result<()> {
    if live_node_matches(c, source)? {
        Ok(())
    } else {
        Err(restore_target_mismatch())
    }
}

pub(super) fn live_node_matches(c: &Connection, node: &str) -> Result<bool> {
    Ok(crate::fleet::FleetService::stable_self_id_on(c)
        .map_err(DeliveryError::internal)?
        .as_deref()
        == Some(node))
}

pub(super) fn require_enrolled_identity(c: &Connection, node: &str) -> Result<()> {
    if live_node_matches(c, node)? {
        Ok(())
    } else {
        Err(DeliveryError::conflict("OPEN_SOCKETS_ENROLLED_NODE_MISMATCH",
            "The configured node identity differs from the enrolled node. Reconcile enrollment before reviewing sockets.")
            .at(KEY))
    }
}

fn restore_target_mismatch() -> DeliveryError {
    DeliveryError::conflict(
        "OPEN_SOCKETS_RESTORE_TARGET_MISMATCH",
        "Open socket backups can only be restored on their source node",
    )
    .at(KEY)
}

pub(super) fn publish_on(
    c: &Connection,
    desired: &[OpenSocketContract],
    actor: &str,
) -> Result<()> {
    let encoded = serde_json::to_string(desired)?;
    Store::write_open_sockets_on(c, &encoded).map_err(DeliveryError::internal)?;
    Store::record_open_sockets_on(c, actor, desired.len()).map_err(DeliveryError::internal)?;
    Ok(())
}
