//! OpenAPI config, sealed token, history and durable execution revision commit together.
use super::{Result, Store, StoreError};
use crate::config::{OpenApiConfig, OPENAPI_KEY, OPENAPI_TOKEN_SECRET};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

impl Store {
    pub(crate) fn openapi_revision(&self) -> Result<String> {
        self.with(Self::openapi_revision_on)
    }

    pub(crate) fn openapi_revision_on(c: &Connection) -> Result<String> {
        let revision: i64 = c.query_row(
            "SELECT revision FROM openapi_execution WHERE id=1",
            [],
            |r| r.get(0),
        )?;
        Ok(format!("openapi:{revision}"))
    }

    /// Caller holds openapi_gate. None preserves; empty deletes even an unreadable row.
    pub(crate) fn save_openapi_atomic(
        &self,
        cfg: &OpenApiConfig,
        token: Option<&str>,
        actor: &str,
        expected: Option<&str>,
    ) -> Result<()> {
        let sealed = token
            .filter(|token| !token.is_empty())
            .map(|token| self.seal_secret(OPENAPI_TOKEN_SECRET, token))
            .transpose()?;
        self.with(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            if let Some(expected) = expected {
                if Self::openapi_revision_on(&tx)? != expected {
                    return Err(StoreError::Authority("OPENAPI_REVISION_CONFLICT".into()));
                }
            }
            let raw: Option<String> = tx
                .query_row(
                    "SELECT value FROM config_kv WHERE key=?1",
                    [OPENAPI_KEY],
                    |r| r.get(0),
                )
                .optional()?;
            let old_config: std::result::Result<OpenApiConfig, _> = raw
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map(Option::unwrap_or_default);
            if token.is_none()
                && !old_config
                    .as_ref()
                    .is_ok_and(|old| old.same_credential_source(cfg))
                && Self::openapi_token_stored(&tx)?
            {
                return Err(StoreError::Authority(
                    "OPENAPI_SOURCE_BINDING_CONFLICT".into(),
                ));
            }
            let existing = self.secret_get_on(&tx, OPENAPI_TOKEN_SECRET)?;
            if existing
                .as_deref()
                .into_iter()
                .chain(token)
                .any(|candidate| cfg.contains_token(candidate))
            {
                return Err(StoreError::Authority(
                    "OPENAPI_SECRET_IN_PUBLIC_CONFIG".into(),
                ));
            }
            let token_changed = match token {
                None => false,
                Some("") => Self::openapi_token_stored(&tx)?,
                Some(next) => existing.as_deref() != Some(next),
            };
            if let Some(next) = token.filter(|value| token_changed && !value.is_empty()) {
                if crate::config_delivery::ConfigDelivery::public_openapi_contains_token(&tx, next)?
                {
                    return Err(StoreError::Authority(
                        "OPENAPI_SECRET_IN_PUBLIC_CONFIG".into(),
                    ));
                }
            }
            if old_config.as_ref().is_ok_and(|old| old == cfg) && !token_changed {
                return Ok(());
            }
            Self::write_openapi(&tx, cfg, token.map(|_| sealed.as_deref()), actor)?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Publish on the caller's transaction, with openapi_gate held.
    pub(crate) fn publish_openapi_on(
        c: &Transaction<'_>,
        cfg: &OpenApiConfig,
        actor: &str,
    ) -> Result<()> {
        Self::write_openapi(c, cfg, None, actor)
    }

    fn openapi_token_stored(c: &Transaction<'_>) -> Result<bool> {
        Ok(c.query_row(
            "SELECT EXISTS(SELECT 1 FROM secrets WHERE name=?1)",
            [OPENAPI_TOKEN_SECRET],
            |r| r.get(0),
        )?)
    }

    fn write_openapi(
        c: &Transaction<'_>,
        cfg: &OpenApiConfig,
        token: Option<Option<&str>>,
        actor: &str,
    ) -> Result<()> {
        c.execute(
            "INSERT INTO config_kv(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![OPENAPI_KEY, serde_json::to_string(cfg)?],
        )?;
        match token {
            Some(Some(value)) => {
                c.execute(
                    "INSERT INTO secrets(name,value) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value",
                    params![OPENAPI_TOKEN_SECRET, value],
                )?;
            }
            Some(None) => {
                c.execute("DELETE FROM secrets WHERE name=?1", [OPENAPI_TOKEN_SECRET])?;
            }
            None => {}
        }
        let revision: i64 = c.query_row(
            "UPDATE openapi_execution SET revision=revision+1 WHERE id=1 RETURNING revision",
            [],
            |r| r.get(0),
        )?;
        let payload =
            serde_json::json!({"revision":revision,"token_set":Self::openapi_token_stored(c)?});
        c.execute(
            "INSERT INTO config_history(ts_ms,actor,scope,summary,payload) VALUES(?1,?2,'openapi','OpenAPI execution configuration updated',?3)",
            params![crate::id::now_ms() as i64, actor, payload.to_string()],
        )?;
        Ok(())
    }
}
