use super::*;
use crate::execution::Authority;

impl UserMdService {
    pub fn propose_authorized(
        &self,
        authority: &Authority,
        person: Option<&str>,
        key: &str,
        content: &str,
        trace: &str,
        rationale: &str,
    ) -> Result<UserMdEntry, String> {
        let (person, device) = authority
            .write_target(person, None)
            .ok_or("profile destination unavailable")?;
        let (person, key, content, rationale) = validate_fields(&person, key, content, rationale)?;
        if rationale.is_empty() {
            return Err("rationale required".into());
        }
        let now = crate::id::now_ms();
        let id = crate::id::next("umd");
        authority.mutate(&self.store,|c| {
            let tx=c.unchecked_transaction()?;
            authority.check_in(&tx)?;
            tx.execute("INSERT INTO user_md_entries(id,person_id,scope_device,key,content,source_device,trace_id,turn_ts,rationale,author,status,created_ms,updated_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'model','active',?8,?8)",params![id,person,device,key,content,authority.physical_device_id().unwrap_or("console"),trace,now as i64,rationale])?;
            tx.commit()?; Ok(())
        }).map_err(|e|e.to_string())?;
        self.get_by_id(&id)?
            .ok_or_else(|| "saved profile missing".into())
    }
}
