//! Atomic persistence for the OpenSocket domain; callers publish after commit.

use super::{Result, Store, StoreError};
use rusqlite::params;

impl Store {
    /// None preserves the token; an empty value clears it. Sealing occurs
    /// before SQL and plaintext is never included in config/history metadata.
    pub(crate) fn save_open_socket_config(
        &self,
        raw: &str,
        token: Option<(&str, &str)>,
        actor: &str,
        count: usize,
    ) -> Result<()> {
        let sealed = match token {
            Some((name, value)) if !value.is_empty() => {
                let vault = self
                    .vault
                    .as_ref()
                    .map_err(|e| StoreError::Vault(e.clone()))?;
                Some(
                    vault
                        .seal(name, value)
                        .map_err(|e| StoreError::Vault(e.to_string()))?,
                )
            }
            _ => None,
        };
        self.with(|conn| {
            let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
            crate::config_delivery::ConfigDelivery::check_open_socket_edit_on(self, &tx, raw, token)?;
            Self::write_open_sockets_on(&tx, raw)?;
            if let Some((name, _)) = token {
                if let Some(value) = &sealed {
                    tx.execute("INSERT INTO secrets(name,value) VALUES (?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value", params![name,value])?;
                } else {
                    tx.execute("DELETE FROM secrets WHERE name=?1", [name])?;
                }
            }
            Self::record_open_sockets_on(&tx, actor, count)?;
            tx.commit()?;
            Ok(())
        })
    }
    pub(crate) fn write_open_sockets_on(c: &rusqlite::Connection, raw: &str) -> Result<()> {
        c.execute(
            "INSERT INTO config_kv(key,value) VALUES ('open_sockets',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [raw],
        )?;
        Ok(())
    }

    pub(crate) fn record_open_sockets_on(
        c: &rusqlite::Connection,
        actor: &str,
        count: usize,
    ) -> Result<()> {
        c.execute(
            "INSERT INTO config_history(ts_ms,actor,scope,summary,payload) VALUES (?1,?2,'open_sockets',?3,?4)",
            params![crate::id::now_ms() as i64, actor,
                format!("updated open sockets ({count} registered)"),
                serde_json::json!({"count":count}).to_string()],
        )?;
        Ok(())
    }
}
