use crate::store::{Result, StoreError};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
pub(super) fn load(c: &Connection, id: &str) -> Result<Option<Value>> {
    let row: Option<(String, String, i64)> = c
        .query_row(
            "SELECT action_json,state,revision FROM confirmations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    row.map(|(raw, state, revision)| {
        let mut action: Value = serde_json::from_str(&raw)?;
        action["state"] = Value::String(state);
        action["revision"] = Value::from(revision);
        Ok(action)
    })
    .transpose()
}
pub(super) fn update(c: &Connection, action: &Value) -> Result<()> {
    let changed=c.execute("UPDATE confirmations SET state=?1,revision=?2,action_json=?3,private_payload=CASE WHEN ?1 IN ('pending','approved') THEN private_payload ELSE NULL END WHERE id=?4",params![action["state"].as_str().unwrap_or("stale"),action["revision"].as_u64().unwrap_or(0) as i64,action.to_string(),action["id"].as_str().unwrap_or("")])?;
    if changed != 1 {
        return Err(StoreError::Authority("CONFIRMATION_NOT_FOUND".into()));
    }
    Ok(())
}
pub(crate) fn invalidate_session(c: &Connection, session: &str) -> Result<()> {
    c.execute(
        "UPDATE sessions SET action_generation=action_generation+1 WHERE id=?1",
        [session],
    )?;
    c.execute("UPDATE confirmations SET state='cancelled',revision=revision+1,private_payload=NULL WHERE session_id=?1 AND state IN ('pending','approved')",[session])?;
    Ok(())
}
pub(super) fn generation(c: &Connection, session: &str) -> Result<i64> {
    c.query_row(
        "SELECT action_generation FROM sessions WHERE id=?1",
        [session],
        |r| r.get(0),
    )
    .map_err(Into::into)
}
