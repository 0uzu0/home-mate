//! Durable physical contexts. A different message session is the prompt
//! boundary; delayed writes remain in their original archived session.
use super::*;
use crate::execution::Authority;
use rusqlite::OptionalExtension;
use serde::Serialize;

pub(crate) const MIGRATION_SQL: &str = "
CREATE TABLE device_contexts (
 generation INTEGER PRIMARY KEY AUTOINCREMENT,
 session_id TEXT NOT NULL UNIQUE REFERENCES sessions(id) ON DELETE CASCADE,
 device TEXT NOT NULL, data_epoch INTEGER NOT NULL,
 created_ms INTEGER NOT NULL, last_activity_ms INTEGER NOT NULL);
CREATE INDEX idx_device_contexts ON device_contexts(device,data_epoch,generation DESC);
";
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ContextTicket {
    pub generation: u64,
    pub data_epoch: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct DeviceContext {
    pub session_id: String,
    pub generation: u64,
    pub data_epoch: u64,
    pub created_ms: u64,
    pub last_activity_ms: u64,
}
impl DeviceContext {
    pub fn ticket(&self) -> ContextTicket {
        ContextTicket {
            generation: self.generation,
            data_epoch: self.data_epoch,
        }
    }
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceContext> {
    Ok(DeviceContext {
        session_id: r.get(0)?,
        generation: r.get::<_, i64>(1)? as u64,
        data_epoch: r.get::<_, i64>(2)? as u64,
        created_ms: r.get::<_, i64>(3)? as u64,
        last_activity_ms: r.get::<_, i64>(4)? as u64,
    })
}
const COLUMNS: &str = "session_id,generation,data_epoch,created_ms,last_activity_ms";
impl SessionStore {
    fn device_authority(&self, device: &str) -> std::result::Result<Authority, String> {
        Authority::internal(
            self.store.clone(),
            crate::working_memory::ScopeContext {
                device_id: Some(device.into()),
                person_id: None,
                room: None,
            },
        )
    }
    /// Current admission never materializes archive history.
    pub fn current_device_context(
        &self,
        device: &str,
    ) -> std::result::Result<Option<DeviceContext>, String> {
        let authority = self.device_authority(device)?;
        self.store.with(|c| {
            authority.check_in(c)?;
            let found=c.query_row(&format!("SELECT {COLUMNS} FROM device_contexts WHERE device=?1 AND data_epoch=?2 ORDER BY generation DESC LIMIT 1"),rusqlite::params![device,authority.epoch() as i64],row).optional()?;
            if let Some(context)=&found { if !authority.session_is_bound_in(c,&context.session_id)? { return Err(crate::store::StoreError::Authority("context scope changed".into())); } }
            Ok(found)
        }).map_err(|e|e.to_string())
    }
    pub fn device_contexts(
        &self,
        device: &str,
        before: Option<u64>,
        limit: usize,
    ) -> std::result::Result<Vec<DeviceContext>, String> {
        let authority = self.device_authority(device)?;
        self.store.with(|c| {
            authority.check_in(c)?;
            let mut stmt=c.prepare(&format!("SELECT {COLUMNS} FROM device_contexts WHERE device=?1 AND data_epoch=?2 AND generation<?3 AND generation!=(SELECT MAX(generation) FROM device_contexts WHERE device=?1 AND data_epoch=?2) ORDER BY generation DESC LIMIT ?4"))?;
            let rows=stmt.query_map(rusqlite::params![device,authority.epoch() as i64,before.unwrap_or(i64::MAX as u64).min(i64::MAX as u64) as i64,limit.clamp(1,51) as i64],row)?.collect::<std::result::Result<Vec<_>,_>>()?;
            for context in &rows { if !authority.session_is_bound_in(c,&context.session_id)? { return Err(crate::store::StoreError::Authority("context scope changed".into())); } }
            Ok(rows)
        }).map_err(|e|e.to_string())
    }
    /// Direct scoped lookup keeps old archives reachable beyond the list page.
    pub fn device_context(
        &self,
        device: &str,
        session: &str,
    ) -> std::result::Result<Option<DeviceContext>, String> {
        let authority = self.device_authority(device)?;
        self.store.with(|c| {
            authority.check_in(c)?;
            let found=c.query_row(&format!("SELECT {COLUMNS} FROM device_contexts WHERE session_id=?1 AND device=?2 AND data_epoch=?3 LIMIT 1"),rusqlite::params![session,device,authority.epoch() as i64],row).optional()?;
            if let Some(context)=&found { if !authority.session_is_bound_in(c,&context.session_id)? { return Err(crate::store::StoreError::Authority("context scope changed".into())); } }
            Ok(found)
        }).map_err(|e|e.to_string())
    }
    pub fn context_ticket(&self, session: &str, epoch: u64) -> Result<Option<ContextTicket>> {
        self.store.with(|c| Ok(c.query_row("SELECT generation,data_epoch FROM device_contexts WHERE session_id=?1 AND data_epoch=?2",rusqlite::params![session,epoch as i64],|r|Ok(ContextTicket {generation:r.get::<_,i64>(0)? as u64,data_epoch:r.get::<_,i64>(1)? as u64})).optional()?))
    }
    pub fn context_messages_page(
        &self,
        session: &str,
        before: Option<i64>,
    ) -> Result<(Vec<serde_json::Value>, Option<i64>)> {
        self.store.with(|c| {
            let mut stmt=c.prepare("SELECT id,role,content,ts_ms FROM messages WHERE session_id=?1 AND id<?2 ORDER BY id DESC LIMIT 51")?;
            let mut rows=stmt.query_map(rusqlite::params![session,before.unwrap_or(i64::MAX)],|r|Ok(serde_json::json!({"id":r.get::<_,i64>(0)?,"role":r.get::<_,String>(1)?,"content":r.get::<_,String>(2)?,"ts_ms":r.get::<_,i64>(3)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let more=rows.len()>50; rows.truncate(50);
            let next=if more {rows.last().and_then(|m|m["id"].as_i64())} else {None};
            rows.reverse(); Ok((rows,next))
        })
    }
    /// Verified wake/turn admission only. Readback never renews. An admitted
    /// continuation keeps its original context even when idle time elapsed.
    pub fn admit_device_context(
        &self,
        device: &str,
        now: u64,
        window_ms: u64,
        rotate_expired: bool,
    ) -> std::result::Result<DeviceContext, String> {
        let authority = self.device_authority(device)?;
        let legacy = self.legacy_device_session_id(device)?;
        let previous = self.current_device_context(device)?;
        let legacy_activity = if previous.is_none() {
            self.store.with(|c| {
                let activity:Option<i64>=c.query_row("SELECT ts_ms FROM messages WHERE session_id=?1 ORDER BY id DESC LIMIT 1",[&legacy],|r|r.get(0)).optional()?;
                Ok(activity.map(|v|v as u64))
            }).map_err(|e|e.to_string())?
        } else {
            None
        };
        let fresh = if rotate_expired
            && previous
                .as_ref()
                .map(|c| c.last_activity_ms)
                .or(legacy_activity)
                .is_some_and(|activity| now.saturating_sub(activity) >= window_ms)
        {
            let fresh = format!("dev:context12:{device}:{}", id::next("ctx"));
            self.ensure(&fresh).map_err(|e| e.to_string())?;
            authority.bind_session(&fresh)?;
            fresh
        } else {
            legacy.clone()
        };
        self.store.with(|c| {
            let tx=c.unchecked_transaction()?;
            authority.check_in(&tx)?;
            let mut previous=tx.query_row(&format!("SELECT {COLUMNS} FROM device_contexts WHERE device=?1 AND data_epoch=?2 ORDER BY generation DESC LIMIT 1"),rusqlite::params![device,authority.epoch() as i64],row).optional()?;
            // Preserve an already-bound v11 conversation as the first archive.
            // Its real message activity determines whether this first wake rolls over.
            if previous.is_none() {
                if let Some(activity)=legacy_activity {
                    tx.execute("INSERT INTO device_contexts(session_id,device,data_epoch,created_ms,last_activity_ms) VALUES (?1,?2,?3,?4,?5)",rusqlite::params![legacy,device,authority.epoch() as i64,now as i64,activity as i64])?;
                    previous=Some(DeviceContext {session_id:legacy.clone(),generation:tx.last_insert_rowid() as u64,data_epoch:authority.epoch(),created_ms:now,last_activity_ms:activity});
                }
            }
            let context=match previous {
                Some(mut previous) if !rotate_expired || now.saturating_sub(previous.last_activity_ms)<window_ms => {
                    previous.last_activity_ms=previous.last_activity_ms.max(now);
                    tx.execute("UPDATE device_contexts SET last_activity_ms=?1 WHERE session_id=?2",rusqlite::params![previous.last_activity_ms as i64,previous.session_id])?;
                    previous
                },
                previous => {
                    let session_id=if previous.is_none() {legacy.clone()} else {fresh.clone()};
                    let mut context=DeviceContext {session_id,generation:0,data_epoch:authority.epoch(),created_ms:now,last_activity_ms:now};
                    tx.execute("INSERT INTO device_contexts(session_id,device,data_epoch,created_ms,last_activity_ms) VALUES (?1,?2,?3,?4,?4)",rusqlite::params![context.session_id,device,context.data_epoch as i64,now as i64])?;
                    context.generation=tx.last_insert_rowid() as u64;
                    context
                }
            };
            if !authority.session_is_bound_in(&tx,&context.session_id)? { return Err(crate::store::StoreError::Authority("context scope changed".into())); }
            tx.commit()?;
            Ok(context)
        }).map_err(|e|e.to_string())
    }
    /// Resolve an admission's frozen identity. Existing B01 scope/data-epoch
    /// validation applies equally to current and archived contexts.
    pub fn context_session_id(
        &self,
        device: &str,
        ticket: ContextTicket,
    ) -> std::result::Result<String, String> {
        let authority = self.device_authority(device)?;
        if authority.epoch() != ticket.data_epoch {
            return Err("context data epoch changed".into());
        }
        self.store.with(|c| {
            authority.check_in(c)?;
            let session:String=c.query_row("SELECT session_id FROM device_contexts WHERE device=?1 AND data_epoch=?2 AND generation=?3",rusqlite::params![device,ticket.data_epoch as i64,ticket.generation as i64],|r|r.get(0))?;
            if !authority.session_is_bound_in(c,&session)? { return Err(crate::store::StoreError::Authority("context scope changed".into())); }
            Ok(session)
        }).map_err(|e|e.to_string())
    }
    /// The frozen ticket is captured before async work. Epoch, generation and
    /// current-head checks are in the update itself, including reset/name reuse.
    pub fn complete_context(&self, session: &str, ticket: ContextTicket, now: u64) -> Result<bool> {
        self.store.with(|c| {
            let changed=c.execute("UPDATE device_contexts SET last_activity_ms=MAX(last_activity_ms,?1) WHERE session_id=?2 AND generation=?3 AND data_epoch=?4 AND data_epoch=(SELECT data_epoch FROM config_head WHERE id=1) AND generation=(SELECT MAX(other.generation) FROM device_contexts other WHERE other.device=device_contexts.device AND other.data_epoch=device_contexts.data_epoch)",rusqlite::params![now as i64,session,ticket.generation as i64,ticket.data_epoch as i64])?;
            Ok(changed==1)
        })
    }
}
