use super::*;
use crate::execution::Authority;

impl WorkingMemoryStore {
    pub fn list_authorized(
        &self,
        authority: &Authority,
        person: Option<&str>,
        device: Option<&str>,
        key: Option<&str>,
    ) -> Result<Vec<WorkingMemoryItem>, String> {
        authority.check()?;
        Ok(self
            .list_all()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|item| {
                authority.can_read(&item.scope_person, &item.scope_device)
                    && person.is_none_or(|p| p.trim() == item.scope_person)
                    && device.is_none_or(|d| d.trim() == item.scope_device)
                    && key.is_none_or(|k| k.trim() == item.key_or_topic)
            })
            .collect())
    }

    pub fn set_authorized(
        &self,
        authority: &Authority,
        person: &str,
        device: &str,
        key: &str,
        content: &str,
        trace: &str,
    ) -> Result<WorkingMemoryItem, String> {
        if !authority.can_write(person, device) {
            return Err("memory destination unavailable".into());
        }
        let key = normalize_key(key)?;
        let content = normalize_content(content)?;
        let now = crate::id::now_ms();
        authority.mutate(&self.store,|c| {
            let tx=c.unchecked_transaction()?;
            authority.check_in(&tx)?;
            let exists: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM working_memory WHERE scope_person=?1 AND scope_device=?2 AND key_or_topic=?3)",rusqlite::params![person,device,key],|r|r.get(0))?;
            let count:i64=tx.query_row("SELECT count(*) FROM working_memory",[],|r|r.get(0))?;
            if !exists && count>=MAX_ITEMS as i64 { return Err(crate::store::StoreError::Authority("working memory capacity reached; manage entries in Console".into())); }
            tx.execute("INSERT INTO working_memory(id,scope_person,scope_device,key_or_topic,content,created_ms,updated_ms,written_by_device,source_trace_id) VALUES (?1,?2,?3,?4,?5,?6,?6,?7,?8) ON CONFLICT(scope_person,scope_device,key_or_topic) DO UPDATE SET content=excluded.content,updated_ms=excluded.updated_ms,written_by_device=excluded.written_by_device,source_trace_id=excluded.source_trace_id",
                rusqlite::params![crate::id::next("WM"),person,device,key,content,now as i64,authority.physical_device_id().unwrap_or(""),trace])?;
            tx.commit()?;
            Ok(())
        }).map_err(|e|e.to_string())?;
        self.get_raw(person, device, &key)?
            .ok_or_else(|| "saved memory missing".into())
    }

    pub fn delete_authorized(
        &self,
        authority: &Authority,
        person: &str,
        device: &str,
        key: &str,
    ) -> Result<bool, String> {
        if !authority.can_write(person, device) {
            return Ok(false);
        }
        let key = normalize_key(key)?;
        authority.mutate(&self.store,|c| {
            let tx=c.unchecked_transaction()?;
            authority.check_in(&tx)?;
            let n=tx.execute("DELETE FROM working_memory WHERE scope_person=?1 AND scope_device=?2 AND key_or_topic=?3",rusqlite::params![person,device,key])?;
            tx.commit()?;
            Ok(n>0)
        }).map_err(|e|e.to_string())
    }
}
