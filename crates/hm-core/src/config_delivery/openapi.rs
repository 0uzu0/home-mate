use super::{
    discovery::{decode, text, Block},
    portable_service_url, ConfigDelivery, DeliveryError,
};
use crate::{
    config::{ConfigError, OpenApiConfig, OpenApiStatus, OPENAPI_TOKEN_SECRET},
    store::{self, Store},
};
use rusqlite::Connection;

#[derive(Default)]
pub(crate) struct OpenApiSnapshot {
    pub config: OpenApiConfig,
    pub token: Option<String>,
    pub revision: String,
}

impl OpenApiSnapshot {
    pub(crate) fn status(self) -> OpenApiStatus {
        let state = self.config.state_word().into();
        let safe = |url: String| {
            if portable_service_url(&url) {
                url
            } else {
                String::new()
            }
        };
        OpenApiStatus {
            enabled: self.config.enabled,
            spec_url: safe(self.config.spec_url),
            base_url: safe(self.config.base_url),
            auth_header: self.config.auth_header,
            token_set: self.token.is_some(),
            operations: self.config.operations,
            timeout_ms: self.config.timeout_ms,
            retries: self.config.retries,
            state,
            revision: self.revision,
        }
    }
}

impl ConfigDelivery {
    pub(crate) fn openapi_snapshot(store: &Store) -> store::Result<OpenApiSnapshot> {
        store.with(|c| {
            let tx = c.unchecked_transaction()?;
            let token = store.secret_get_on(&tx, OPENAPI_TOKEN_SECRET)?;
            let config: OpenApiConfig =
                decode(text(&tx, Block::OpenApi)?.as_deref()).unwrap_or_default();
            let config = if token
                .as_deref()
                .is_some_and(|token| config.contains_token(token))
            {
                OpenApiConfig::default()
            } else {
                config
            };
            Ok(OpenApiSnapshot {
                config,
                token,
                revision: Store::openapi_revision_on(&tx)?,
            })
        })
    }

    pub(crate) fn save_openapi(
        store: &Store,
        cfg: OpenApiConfig,
        token: Option<&str>,
        actor: &str,
        expected: Option<&str>,
    ) -> Result<OpenApiStatus, ConfigError> {
        let _gate = crate::lock_or_recover(&store.openapi_gate, "openapi.config");
        let existing = store.secret_get(OPENAPI_TOKEN_SECRET)?;
        if existing
            .as_deref()
            .into_iter()
            .chain(token)
            .any(|token| cfg.contains_token(token))
        {
            return Err(
                store::StoreError::Authority("OPENAPI_SECRET_IN_PUBLIC_CONFIG".into()).into(),
            );
        }
        let cfg = cfg.normalized(store.allows_loopback_public())?;
        store.save_openapi_atomic(&cfg, token.map(str::trim), actor, expected)?;
        Ok(Self::openapi_snapshot(store)?.status())
    }
}

impl ConfigDelivery {
    /// A credential entered on the target must also be absent from reviewed
    /// settings and still-readable before/after receipts.
    pub(crate) fn public_openapi_contains_token(
        c: &Connection,
        token: &str,
    ) -> store::Result<bool> {
        let (layers, resolved): (String, String) = c.query_row(
            "SELECT layers_json,resolved_json FROM config_head WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let layers: super::Layers = if layers == "{}" {
            super::Layers::default()
        } else {
            serde_json::from_str(&layers)?
        };
        let resolved: super::Fields = serde_json::from_str(&resolved)?;
        for fields in [&layers.family, &layers.node, &resolved] {
            let config: OpenApiConfig =
                serde_json::from_value(super::service::value_tree(fields, "openapi"))?;
            if config.contains_token(token) {
                return Ok(true);
            }
        }
        let mut query = c.prepare(
            "SELECT d.value FROM config_applications a, json_each(a.result_json, '$.diff') d
             WHERE json_extract(d.value, '$.path') LIKE 'openapi.%'",
        )?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let diff: serde_json::Value = serde_json::from_str(&row.get::<_, String>(0)?)?;
            let Some(path) = diff["path"].as_str() else {
                continue;
            };
            for slot in ["before", "after", "layer_before", "layer_after"] {
                let Some(value) = diff.get(slot).filter(|value| !value.is_null()) else {
                    continue;
                };
                let fields = super::Fields::from([(path.to_owned(), value.clone())]);
                let config: OpenApiConfig =
                    serde_json::from_value(super::service::value_tree(&fields, "openapi"))?;
                if config.contains_token(token) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

pub(super) fn review_layers(
    c: &Connection,
    store: &Store,
    layers: &super::Layers,
    resolved: &super::Fields,
) -> super::Result<()> {
    let token = store
        .secret_get_on(c, OPENAPI_TOKEN_SECRET)
        .map_err(DeliveryError::internal)?;
    for fields in [&layers.family, &layers.node, resolved] {
        let value = super::service::value_tree(fields, "openapi");
        Block::OpenApi.validate(value.clone(), store.allows_loopback_public())?;
        let cfg: OpenApiConfig = serde_json::from_value(value)?;
        reject_exposed_token(&cfg, token.as_deref())?;
    }
    let effective = serde_json::from_value(super::service::value_tree(resolved, "openapi"))?;
    check_source_binding(c, &effective)
}

pub(super) fn reject_exposed_token(cfg: &OpenApiConfig, token: Option<&str>) -> super::Result<()> {
    if token.is_some_and(|token| cfg.contains_token(token)) {
        return Err(DeliveryError::new(
            "OPENAPI_SECRET_IN_PUBLIC_CONFIG",
            "OpenAPI catalogue and endpoints must contain only public configuration values",
        )
        .at("openapi"));
    }
    Ok(())
}

/// OpenAPI package application preserves target-local credentials. A source
/// change requires the node editor's explicit rebind or clear operation.
pub(super) fn check_source_binding(c: &Connection, desired: &OpenApiConfig) -> super::Result<()> {
    let raw = text(c, Block::OpenApi).map_err(DeliveryError::internal)?;
    let current: OpenApiConfig = decode(raw.as_deref()).map_err(DeliveryError::internal)?;
    if current.same_credential_source(desired) {
        return Ok(());
    }
    let token_exists: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM secrets WHERE name=?1)",
        [OPENAPI_TOKEN_SECRET],
        |r| r.get(0),
    )?;
    if token_exists {
        return Err(DeliveryError::conflict(
            "OPENAPI_SOURCE_BINDING_CONFLICT",
            "apply source and credential changes together in the node's OpenAPI editor",
        )
        .at("openapi"));
    }
    Ok(())
}
