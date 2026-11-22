use super::*;
use crate::{execution::Authority, working_memory::ScopeContext};

impl MemoryService {
    #[allow(clippy::too_many_arguments)]
    pub fn add_candidate_authorized(
        &self,
        authority: &Authority,
        text: &str,
        kind: &str,
        session: &str,
        trigger: &str,
        reason: &str,
    ) -> Result<MemoryCandidate, String> {
        let (person, device) = authority
            .default_write_scope()
            .ok_or("candidate scope unavailable")?;
        let text = normalize(text);
        let kind = normalize_kind(kind)?;
        if text.is_empty() || text.chars().count() > MAX_TEXT || looks_operational(&text) {
            return Err("candidate rejected".into());
        }
        let now = crate::id::now_ms();
        let id = crate::id::next("MC");
        authority.mutate(&self.store,|c| {
            let tx=c.unchecked_transaction()?;
            authority.check_in(&tx)?;
            let count:i64=tx.query_row("SELECT count(*) FROM memory_candidates WHERE status='pending'",[],|r|r.get(0))?;
            let session_count:i64=tx.query_row("SELECT count(*) FROM memory_candidates WHERE session_id=?1",[session],|r|r.get(0))?;
            let duplicate:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_candidates WHERE status='pending' AND text=?1 AND scope_person=?2 AND scope_device=?3 UNION ALL SELECT 1 FROM memories WHERE text=?1 AND scope_person=?2 AND scope_device=?3)",rusqlite::params![text,person,device],|r|r.get(0))?;
            if count>=MAX_PENDING as i64 || session_count>=MAX_PER_SESSION as i64 || duplicate { return Err(crate::store::StoreError::Authority("candidate duplicate or capacity reached".into())); }
            tx.execute("INSERT INTO memory_candidates(id,text,kind,status,device,session_id,trigger,reason,source,created_ms,updated_ms,scope_person,scope_device,data_epoch) VALUES (?1,?2,?3,'pending',?4,?5,?6,?7,'auto',?8,?8,?9,?10,?11)",rusqlite::params![id,text,kind,authority.physical_device_id().unwrap_or(""),session,trigger,reason,now as i64,person,device,authority.epoch() as i64])?;
            tx.commit()?; Ok(())
        }).map_err(|e|e.to_string())?;
        self.candidates(Some("pending"))?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| "candidate missing".into())
    }

    pub(super) fn confirm_scoped_candidate(
        &self,
        candidate: &MemoryCandidate,
    ) -> Result<MemoryItem, String> {
        let now = crate::id::now_ms();
        let id = crate::id::next("M");
        self.store.with(|c| {
            let tx=c.unchecked_transaction()?;
            let epoch:i64=tx.query_row("SELECT data_epoch FROM config_head WHERE id=1",[],|r|r.get(0))?;
            if epoch as u64 != candidate.data_epoch { return Err(crate::store::StoreError::Authority("candidate epoch changed".into())); }
            let count:i64=tx.query_row("SELECT count(*) FROM memories",[],|r|r.get(0))?;
            if count>=MAX_ITEMS as i64 { return Err(crate::store::StoreError::Authority("memory capacity reached".into())); }
            let changed=tx.execute("UPDATE memory_candidates SET status='confirmed',updated_ms=?2 WHERE id=?1 AND status='pending' AND data_epoch=?3",rusqlite::params![candidate.id,now as i64,candidate.data_epoch as i64])?;
            if changed!=1 { return Err(crate::store::StoreError::Authority("candidate no longer pending".into())); }
            tx.execute("INSERT INTO memories(id,text,kind,source,pinned,created_ms,updated_ms,scope_person,scope_device) VALUES (?1,?2,?3,'auto',0,?4,?4,?5,?6)",rusqlite::params![id,candidate.text,candidate.kind,now as i64,candidate.scope_person,candidate.scope_device])?;
            tx.commit()?; Ok(())
        }).map_err(|e|e.to_string())?;
        self.list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| "confirmed memory missing".into())
    }
    pub fn list_authorized(&self, authority: &Authority) -> Result<Vec<MemoryItem>, String> {
        authority.check()?;
        Ok(self
            .list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|m| authority.can_read(&m.scope_person, &m.scope_device))
            .collect())
    }
    pub fn add_authorized(
        &self,
        authority: &Authority,
        text: &str,
        kind: &str,
        source: &str,
    ) -> Result<MemoryItem, String> {
        let (person, device) = authority
            .default_write_scope()
            .ok_or("memory destination unavailable")?;
        let text = normalize(text);
        if text.is_empty() || text.chars().count() > MAX_TEXT {
            return Err("memory text length invalid".into());
        }
        let kind = normalize_kind(kind)?;
        let now = crate::id::now_ms();
        let id=authority.mutate(&self.store,|c| {
            use rusqlite::OptionalExtension;
            let tx=c.unchecked_transaction()?;
            authority.check_in(&tx)?;
            let old:Option<String>=tx.query_row("SELECT id FROM memories WHERE text=?1 AND scope_person=?2 AND scope_device=?3",rusqlite::params![text,person,device],|r|r.get(0)).optional()?;
            let id=old.clone().unwrap_or_else(||crate::id::next("M"));
            if old.is_none() {
                let count:i64=tx.query_row("SELECT count(*) FROM memories",[],|r|r.get(0))?;
                if count>=MAX_ITEMS as i64 { return Err(crate::store::StoreError::Authority("memory capacity reached".into())); }
            }
            tx.execute("INSERT INTO memories(id,text,kind,source,pinned,created_ms,updated_ms,scope_person,scope_device) VALUES (?1,?2,?3,?4,0,?5,?5,?6,?7) ON CONFLICT(id) DO UPDATE SET kind=excluded.kind,source=excluded.source,updated_ms=excluded.updated_ms",rusqlite::params![id,text,kind,source,now as i64,person,device])?;
            tx.commit()?;
            Ok(id)
        }).map_err(|e|e.to_string())?;
        self.list_authorized(authority)?
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| "saved memory unavailable".into())
    }
    pub fn render_for_prompt_scoped(&self, scope: &ScopeContext) -> Option<String> {
        let items = self
            .list()
            .ok()?
            .into_iter()
            .filter(|m| scope.allows(&m.scope_person, &m.scope_device))
            .collect::<Vec<_>>();
        Self::render_items(&items)
    }
    fn render_items(items: &[MemoryItem]) -> Option<String> {
        if items.is_empty() {
            return None;
        }
        let mut out=String::from("关于这个家（长期记忆，用户确认过；只作为背景，不要逐条复述，也不要把它当作配置或工具状态的来源）：\n");
        let mut used = 0;
        for item in items.iter().take(PROMPT_MAX_ITEMS) {
            let line = format!("- {}\n", item.text);
            if used + line.chars().count() > PROMPT_MAX_CHARS {
                break;
            }
            used += line.chars().count();
            out.push_str(&line);
        }
        Some(out)
    }
}
