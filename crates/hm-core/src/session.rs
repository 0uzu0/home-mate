//! Sessions and message history (persisted; context is rebuilt from here).

mod context;
pub(crate) use context::MIGRATION_SQL as CONTEXT_MIGRATION;
pub use context::{ContextTicket, DeviceContext};

use crate::id;
use crate::llm::WireMessage;
use crate::store::{Result, Store};
use rusqlite::OptionalExtension;

pub struct SessionStore {
    store: std::sync::Arc<Store>,
}

impl SessionStore {
    pub(crate) fn store(&self) -> std::sync::Arc<Store> {
        self.store.clone()
    }
    pub fn new(store: std::sync::Arc<Store>) -> Self {
        Self { store }
    }

    /// Reuse only a verified physical scope. Unclassified legacy history stays archived.
    /// Trusted connected-device ingress; never use caller-supplied HTTP identity.
    pub fn device_session_id(&self, device: &str) -> std::result::Result<String, String> {
        if let Some(context) = self.current_device_context(device)? {
            return Ok(context.session_id);
        }
        self.legacy_device_session_id(device)
    }
    fn legacy_device_session_id(&self, device: &str) -> std::result::Result<String, String> {
        let base = format!("dev:{device}");
        let authority = crate::execution::Authority::internal(
            self.store.clone(),
            crate::working_memory::ScopeContext {
                device_id: Some(device.into()),
                person_id: None,
                room: None,
            },
        )?;
        self.ensure(&base).map_err(|e| e.to_string())?;
        match authority.bind_session(&base) {
            Ok(()) => Ok(base),
            Err(error) if error.contains("SESSION_SCOPE_UNVERIFIED") => {
                let fresh = format!("dev:scope11:{device}:{}", authority.epoch());
                self.ensure(&fresh).map_err(|e| e.to_string())?;
                authority.bind_session(&fresh)?;
                Ok(fresh)
            }
            Err(error) => Err(error),
        }
    }

    /// Readback of an existing physical session; never adopts or creates history.
    pub fn current_device_session_id(
        &self,
        device: &str,
    ) -> std::result::Result<Option<String>, String> {
        if let Some(context) = self.current_device_context(device)? {
            return Ok(Some(context.session_id));
        }
        let authority = crate::execution::Authority::internal(
            self.store.clone(),
            crate::working_memory::ScopeContext {
                device_id: Some(device.into()),
                person_id: None,
                room: None,
            },
        )?;
        for candidate in [
            format!("dev:{device}"),
            format!("dev:scope11:{device}:{}", authority.epoch()),
        ] {
            if authority.session_is_bound(&candidate)? {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    pub fn clear_device_conversation(
        &self,
        device: &str,
        expected: &str,
    ) -> std::result::Result<usize, String> {
        let authority = crate::execution::Authority::internal(
            self.store.clone(),
            crate::working_memory::ScopeContext {
                device_id: Some(device.into()),
                person_id: None,
                room: None,
            },
        )?;
        self.store.with(|c| {
            let tx=c.unchecked_transaction()?;
            authority.check_in(&tx)?;
            let mut current=tx.query_row("SELECT session_id FROM device_contexts WHERE device=?1 AND data_epoch=?2 ORDER BY generation DESC LIMIT 1",rusqlite::params![device,authority.epoch() as i64],|r|r.get::<_,String>(0)).optional()?;
            for candidate in [format!("dev:{device}"),format!("dev:scope11:{device}:{}",authority.epoch())] {
                if current.is_some() { break; }
                if authority.session_is_bound_in(&tx,&candidate)? { current=Some(candidate); break; }
            }
            if current.as_deref()!=Some(expected) { return Err(crate::store::StoreError::Authority("SESSION_CHANGED".into())); }
            crate::confirmation::store::invalidate_session(&tx,expected)?;
            let cleared=tx.execute("DELETE FROM messages WHERE session_id=?1",[expected])?;
            tx.commit()?; Ok(cleared)
        }).map_err(|e|e.to_string())
    }

    pub fn create(&self) -> Result<String> {
        let sid = format!(
            "S{}",
            crate::config_delivery::uuid()
                .map_err(|e| crate::store::StoreError::Authority(e.detail))?
        );
        self.store.with(|c| {
            c.execute(
                "INSERT INTO sessions (id, created_ms, title) VALUES (?1, ?2, ?3)",
                rusqlite::params![sid, id::now_ms() as i64, ""],
            )?;
            Ok(())
        })?;
        Ok(sid)
    }

    /// Find-or-create a session with a caller-chosen stable id (device
    /// conversations persist across restarts under e.g. "dev:dot2-d").
    pub fn ensure(&self, session_id: &str) -> Result<String> {
        self.store.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO sessions (id, created_ms, title) VALUES (?1, ?2, ?3)",
                rusqlite::params![session_id, id::now_ms() as i64, "device"],
            )?;
            Ok(())
        })?;
        Ok(session_id.to_string())
    }

    pub fn exists(&self, session_id: &str) -> Result<bool> {
        self.store.with(|c| {
            Ok(
                c.query_row("SELECT 1 FROM sessions WHERE id=?1", [session_id], |_| {
                    Ok(())
                })
                .is_ok(),
            )
        })
    }

    /// Append a message. `role` is one of user/assistant/tool; tool turns
    /// carry their JSON as content — history for context rebuild only needs
    /// user/assistant text in M1.
    pub fn append(&self, session_id: &str, role: &str, content: &str) -> Result<()> {
        self.store.with(|c| {
            c.execute(
                "INSERT INTO messages (session_id, role, content, ts_ms) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![session_id, role, content, id::now_ms() as i64],
            )?;
            Ok(())
        })
    }

    /// Append a message together with the structured wire form the model
    /// will see again. `content` stays readable for the console; `wire` is
    /// the source of truth for replay (R1f: assistant tool_calls and
    /// role=tool results, never `[调用 …]` prose).
    pub fn append_wire(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        wire: &WireMessage,
    ) -> Result<()> {
        let wire = serde_json::to_string(wire).unwrap_or_default();
        self.store.with(|c| {
            c.execute(
                "INSERT INTO messages (session_id, role, content, ts_ms, wire) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![session_id, role, content, id::now_ms() as i64, wire],
            )?;
            Ok(())
        })
    }

    /// Context for the next turn: user/assistant/tool messages in order.
    /// Rows with a stored `wire` replay exactly; legacy rows fall back to
    /// plain text, and legacy tool rows (no wire, hence no call id to pair
    /// with) are skipped.
    pub fn history(&self, session_id: &str, limit: usize) -> Result<Vec<WireMessage>> {
        self.store.with(|c| {
            let mut stmt = c.prepare(
                "SELECT role, content, wire FROM (
                    SELECT id, role, content, wire FROM messages
                    WHERE session_id = ?1 AND role IN ('user','assistant','tool')
                    ORDER BY id DESC LIMIT ?2
                 ) ORDER BY id ASC",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![session_id, limit as i64], |r| {
                    let role: String = r.get(0)?;
                    let content: String = r.get(1)?;
                    let wire: Option<String> = r.get(2)?;
                    Ok((role, content, wire))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut out = Vec::with_capacity(rows.len());
            for (role, content, wire) in rows {
                if let Some(Some(msg)) = wire.map(|w| serde_json::from_str::<WireMessage>(&w).ok())
                {
                    out.push(msg);
                    continue;
                }
                match role.as_str() {
                    "user" => out.push(WireMessage::user(content)),
                    "assistant" => out.push(WireMessage::assistant_text(content)),
                    _ => {}
                }
            }
            Ok(Self::complete_tool_exchanges(out))
        })
    }

    /// R2-BM5/audit Q3: a row window can start in the MIDDLE of a tool batch
    /// (a `role:"tool"` row whose assistant `tool_calls` is outside the
    /// window), and an interrupted write can leave assistant calls with
    /// missing receipts. Either shape is invalid for a strict provider, so
    /// the replay is repaired here:
    /// - a leading tool row is dropped (its call cannot be replayed);
    /// - an assistant batch is kept only when EVERY call has its tool row;
    ///   an incomplete batch (and its partial rows) is dropped, never sent
    ///   with a dangling `tool_call_id`;
    /// - tool rows with no matching kept call are dropped.
    fn complete_tool_exchanges(rows: Vec<WireMessage>) -> Vec<WireMessage> {
        use std::collections::HashSet;
        let mut out: Vec<WireMessage> = Vec::with_capacity(rows.len());
        let mut i = 0usize;
        while i < rows.len() {
            let m = &rows[i];
            let calls = m.tool_calls.as_ref().filter(|c| !c.is_empty());
            if m.role == "assistant" && calls.is_some() {
                let calls = calls.unwrap();
                let mut j = i + 1;
                let mut answered: HashSet<String> = HashSet::new();
                while j < rows.len() && rows[j].role == "tool" {
                    if let Some(id) = &rows[j].tool_call_id {
                        answered.insert(id.clone());
                    }
                    j += 1;
                }
                let wanted: HashSet<&str> = calls.iter().map(|c| c.id.as_str()).collect();
                let complete = wanted.iter().all(|id| answered.contains(*id));
                if complete {
                    out.push(m.clone());
                    for row in &rows[(i + 1)..j] {
                        if let Some(id) = &row.tool_call_id {
                            if wanted.contains(id.as_str()) {
                                out.push(row.clone());
                            }
                        }
                    }
                }
                // Incomplete: drop the call AND its partial receipts. A
                // cancelled row is synthesized at write time (agent loop),
                // never invented here.
                i = j;
                continue;
            }
            if m.role == "tool" {
                // Orphan: no kept assistant call precedes it.
                i += 1;
                continue;
            }
            out.push(m.clone());
            i += 1;
        }
        out
    }

    /// Drop a session's conversation context (the user's 「退下＝清零」).
    /// Traces keep the diagnostic record; the model's next turn starts with
    /// no history — a poisoned self-description must not survive a session
    /// close (live 2026-11-12: the model imitated its own wrong answer).
    pub fn clear(&self, session_id: &str) -> Result<usize> {
        self.store.with(|c| {
            let tx = c.unchecked_transaction()?;
            crate::confirmation::store::invalidate_session(&tx, session_id)?;
            let n = tx.execute("DELETE FROM messages WHERE session_id=?1", [session_id])?;
            tx.commit()?;
            Ok(n)
        })
    }

    /// Drop every DEVICE conversation (`dev:*`). A service whose availability
    /// changed may be described in them by the assistant's own older claims;
    /// the device reconnects into the same id, so the stale text must go.
    /// Console/text sessions are a different namespace and stay.
    pub fn clear_device_sessions(&self) -> Result<usize> {
        self.store.with(|c| {
            let tx=c.unchecked_transaction()?;
            tx.execute("UPDATE sessions SET action_generation=action_generation+1 WHERE id LIKE 'dev:%'",[])?;
            tx.execute("UPDATE confirmations SET state='cancelled',revision=revision+1,private_payload=NULL WHERE session_id LIKE 'dev:%' AND state IN ('pending','approved')",[])?;
            let n=tx.execute("DELETE FROM messages WHERE session_id LIKE 'dev:%'", [])?;
            tx.commit()?;Ok(n)
        })
    }

    pub fn messages(&self, session_id: &str) -> Result<Vec<serde_json::Value>> {
        self.store.with(|c| {
            let mut stmt = c.prepare(
                "SELECT role, content, ts_ms FROM messages WHERE session_id=?1 ORDER BY id",
            )?;
            let rows = stmt
                .query_map([session_id], |r| {
                    Ok(serde_json::json!({
                        "role": r.get::<_, String>(0)?,
                        "content": r.get::<_, String>(1)?,
                        "ts_ms": r.get::<_, i64>(2)?,
                    }))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit Q3: the 24-row window can begin in the middle of a tool batch.
    /// The replay must not start with a receipt whose assistant call is gone.
    #[test]
    fn a_window_that_starts_mid_batch_drops_the_orphan_receipts() {
        let s = SessionStore::new(std::sync::Arc::new(Store::open_in_memory().unwrap()));
        let sid = s.create().unwrap();
        for turn in 0..4 {
            s.append(&sid, "user", &format!("turn {turn}")).unwrap();
            let calls: Vec<crate::llm::WireToolCall> = (0..4)
                .map(|i| crate::llm::WireToolCall {
                    id: format!("C{turn}{i}"),
                    name: "time_now".into(),
                    arguments: "{}".into(),
                })
                .collect();
            s.append_wire(
                &sid,
                "assistant",
                "[调用 …]",
                &WireMessage::assistant_calls(calls.clone()),
            )
            .unwrap();
            for c in &calls {
                s.append_wire(
                    &sid,
                    "tool",
                    "[回执 …]",
                    &WireMessage::tool_result(c.id.clone(), r#"{"status":"ok"}"#),
                )
                .unwrap();
            }
            s.append(&sid, "assistant", "done").unwrap();
        }
        s.append(&sid, "user", "next").unwrap();
        let h = s.history(&sid, 24).unwrap();
        assert_ne!(
            h.first().map(|m| m.role.as_str()),
            Some("tool"),
            "orphan receipt: {h:?}"
        );
        let mut live = std::collections::HashSet::new();
        for m in &h {
            if let Some(calls) = &m.tool_calls {
                for c in calls {
                    live.insert(c.id.clone());
                }
            }
            if m.role == "tool" {
                let id = m.tool_call_id.as_deref().unwrap_or("");
                assert!(live.contains(id), "tool row without its call: {h:?}");
            }
        }
    }

    /// Audit Q3: a crash between the assistant-call insert and the receipt
    /// inserts must not replay a dangling tool_call_id forever.
    #[test]
    fn an_interrupted_batch_is_not_replayed_with_dangling_ids() {
        let s = SessionStore::new(std::sync::Arc::new(Store::open_in_memory().unwrap()));
        let sid = s.create().unwrap();
        s.append(&sid, "user", "关灯").unwrap();
        let calls: Vec<crate::llm::WireToolCall> = (0..4)
            .map(|i| crate::llm::WireToolCall {
                id: format!("C{i}"),
                name: "home.light".into(),
                arguments: "{}".into(),
            })
            .collect();
        s.append_wire(
            &sid,
            "assistant",
            "[调用 …]",
            &WireMessage::assistant_calls(calls),
        )
        .unwrap();
        // Only two receipts were written before the process died.
        for i in 0..2 {
            s.append_wire(
                &sid,
                "tool",
                "[回执 …]",
                &WireMessage::tool_result(format!("C{i}"), r#"{"status":"failed"}"#),
            )
            .unwrap();
        }
        s.append(&sid, "user", "算了").unwrap();
        let h = s.history(&sid, 24).unwrap();
        assert!(
            !h.iter().any(|m| m.tool_calls.is_some()),
            "incomplete batch must be dropped: {h:?}"
        );
        assert!(
            !h.iter().any(|m| m.role == "tool"),
            "partial receipts must be dropped: {h:?}"
        );
        assert_eq!(h.iter().filter(|m| m.role == "user").count(), 2);
    }

    #[test]
    fn history_keeps_order_and_replays_structure() {
        let s = SessionStore::new(std::sync::Arc::new(Store::open_in_memory().unwrap()));
        let sid = s.create().unwrap();
        s.append(&sid, "user", "讲个睡前故事").unwrap();
        s.append(&sid, "assistant", "从前……").unwrap();
        // Legacy tool row (pre-R1f): no wire form, no call id to pair with.
        s.append(&sid, "tool", "{}").unwrap();
        let h = s.history(&sid, 10).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].content.as_deref(), Some("讲个睡前故事"));
        assert_eq!(h[1].content.as_deref(), Some("从前……"));

        // A structured turn replays as assistant.tool_calls + role=tool, and
        // the readable `[调用 …]` line never reaches the model again.
        let calls = WireMessage::assistant_calls(vec![crate::llm::WireToolCall {
            id: "C1".into(),
            name: "time_now".into(),
            arguments: "{}".into(),
        }]);
        s.append_wire(&sid, "assistant", "[调用 time.now {}]", &calls)
            .unwrap();
        s.append_wire(
            &sid,
            "tool",
            "[回执 time.now ok]",
            &WireMessage::tool_result("C1", r#"{"status":"ok"}"#),
        )
        .unwrap();
        let h = s.history(&sid, 10).unwrap();
        assert_eq!(h.len(), 4);
        assert_eq!(h[2].tool_calls.as_ref().unwrap()[0].id, "C1");
        assert_eq!(h[3].role, "tool");
        assert_eq!(h[3].tool_call_id.as_deref(), Some("C1"));
        assert!(
            !h.iter()
                .any(|m| m.content.as_deref().unwrap_or("").contains("[调用")),
            "the model must not see the readable call line: {h:?}"
        );
    }
}
