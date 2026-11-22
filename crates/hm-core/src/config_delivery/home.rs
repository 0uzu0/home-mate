//! The Home Assistant settings and local credential are read and saved together.
use super::{
    discovery::{decode, persist, text, Block},
    ConfigDelivery,
};
use crate::{
    config::{ConfigError, HomeConfig, HOME_TOKEN_SECRET},
    store::{self, Store},
};
use rusqlite::{params, Transaction, TransactionBehavior};

#[derive(Default)]
pub(crate) struct HomeSnapshot {
    pub config: HomeConfig,
    pub token: Option<String>,
}

pub(super) fn validate_directory_transition(
    package: &super::Package,
    before: &super::Fields,
    after: &super::Fields,
) -> super::Result<()> {
    let entities = &after["home.entities"];
    if before["home.entity_source"] != after["home.entity_source"]
        && !entities.as_array().is_some_and(Vec::is_empty)
    {
        let operations = || {
            package
                .blocks
                .iter()
                .filter(|block| block.name == "home")
                .flat_map(|block| &block.operations)
        };
        let reviewed_list = operations().any(|operation| {
            matches!(operation, super::Operation::Replace {path, value}
                if path == "home.entities" && value == entities)
        });
        let clears = |field: &str| {
            operations().any(
                |operation| matches!(operation, super::Operation::Clear {path} if path == field),
            )
        };
        // Only the node layer inherits a lower directory. A family clear can
        // leave a higher node list in place while changing the effective source.
        let inherits_both = package.purpose != super::Purpose::FamilyPolicy
            && clears("home.entities")
            && clears("home.entity_source");
        if reviewed_list || inherits_both {
            return Ok(());
        }
        return Err(super::DeliveryError::new(
            "HOME_DIRECTORY_REPLACEMENT_REQUIRED",
            "switching entity source requires an explicit replacement, an empty directory or inheritance of both source and directory",
        ).at("home.entities"));
    }
    Ok(())
}

pub(super) fn validate_family_directory(
    package: &super::Package,
    before: &super::Layers,
    after: &super::Layers,
) -> super::Result<()> {
    if package.purpose != super::Purpose::FamilyPolicy
        || !package.blocks.iter().any(|block| block.name == "home")
    {
        return Ok(());
    }
    // A node override can hide a family edit now, but later inheritance
    // must not uncover a source paired with the former directory.
    let inherited = |layers: &super::Layers| {
        [
            (
                "home.entity_source",
                serde_json::json!(HomeConfig::default().entity_source),
            ),
            ("home.entities", serde_json::json!([])),
        ]
        .into_iter()
        .map(|(path, default)| {
            (
                path.to_owned(),
                layers.family.get(path).cloned().unwrap_or(default),
            )
        })
        .collect()
    };
    validate_directory_transition(package, &inherited(before), &inherited(after))
}

impl ConfigDelivery {
    pub(crate) fn home_snapshot(store: &Store) -> store::Result<HomeSnapshot> {
        store.with(|c| {
            let tx = c.unchecked_transaction()?;
            let config = decode(text(&tx, Block::Home)?.as_deref())?;
            let token = store.secret_get_on(&tx, HOME_TOKEN_SECRET)?;
            Ok(HomeSnapshot { config, token })
        })
    }

    pub(crate) fn edit_home(
        store: &Store,
        token: Option<&str>,
        actor: &str,
        edit: impl FnOnce(&mut HomeConfig),
    ) -> Result<HomeSnapshot, ConfigError> {
        let sealed = token
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| store.seal_secret(HOME_TOKEN_SECRET, value))
            .transpose()?;
        store.with(|c| Ok((|| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)
                .map_err(store::StoreError::from)?;
            let mut config: HomeConfig = decode(text(&tx, Block::Home)?.as_deref()).unwrap_or_default();
            let previous_source = config.entity_source;
            edit(&mut config);
            if config.entity_source != previous_source {
                // A local switch still requires a refresh from the new directory.
                config.entities.clear();
            }
            let config = config.normalized_with(store.allows_loopback_public())?;
            if let Some(sealed) = sealed {
                tx.execute("INSERT INTO secrets(name,value) VALUES (?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value", params![HOME_TOKEN_SECRET, sealed])
                    .map_err(store::StoreError::from)?;
            } else if token.is_some() {
                tx.execute("DELETE FROM secrets WHERE name=?1", [HOME_TOKEN_SECRET])
                    .map_err(store::StoreError::from)?;
            }
            let token = store.secret_get_on(&tx, HOME_TOKEN_SECRET)?;
            persist(&tx, Block::Home, &config, actor)?;
            tx.commit().map_err(store::StoreError::from)?;
            Ok(HomeSnapshot { config, token })
        })()))?
    }
}
