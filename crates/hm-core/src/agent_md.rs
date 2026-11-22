//! Versioned `Agent.md` persona & guidance store (`#83`, W4b-1).
//!
//! Stores editable `Agent.md` content in SQLite with a bounded revision
//! history (`MAX_REVISIONS`), hot-reloaded on the next conversation turn
//! without restarting Core. Strictly Console/API-managed: no LLM tool may
//! modify `Agent.md`.

use crate::store::Store;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const MAX_AGENT_MD_CHARS: usize = 16_000;
pub const MAX_AGENT_MD_REVISIONS: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMdRevision {
    pub id: i64,
    pub version: u64,
    pub content: String,
    pub author: String,
    pub note: String,
    pub created_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMdSnapshot {
    pub content: String,
    pub version: u64,
    pub updated_ms: u64,
    pub revisions: Vec<AgentMdRevision>,
}

pub struct AgentMdService {
    store: Arc<Store>,
}

impl AgentMdService {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    pub fn current_content(&self) -> String {
        self.store
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT content FROM agent_md_revisions ORDER BY version DESC, id DESC LIMIT 1",
                )?;
                let mut rows = stmt.query([])?;
                if let Some(row) = rows.next()? {
                    Ok(row.get::<_, String>(0)?)
                } else {
                    Ok(String::new())
                }
            })
            .unwrap_or_default()
    }

    pub fn snapshot(&self) -> Result<AgentMdSnapshot, String> {
        let revisions = self
            .store
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT id, version, content, author, note, created_ms
                     FROM agent_md_revisions
                     ORDER BY version DESC, id DESC
                     LIMIT ?1",
                )?;
                let rows = stmt.query_map([MAX_AGENT_MD_REVISIONS as i64], |r| {
                    Ok(AgentMdRevision {
                        id: r.get(0)?,
                        version: r.get::<_, i64>(1)?.max(0) as u64,
                        content: r.get(2)?,
                        author: r.get(3)?,
                        note: r.get(4)?,
                        created_ms: r.get::<_, i64>(5)?.max(0) as u64,
                    })
                })?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                Ok(out)
            })
            .map_err(|e| e.to_string())?;

        let (content, version, updated_ms) = revisions
            .first()
            .map(|r| (r.content.clone(), r.version, r.created_ms))
            .unwrap_or_else(|| (String::new(), 0, 0));

        Ok(AgentMdSnapshot {
            content,
            version,
            updated_ms,
            revisions,
        })
    }

    pub fn update(
        &self,
        content: &str,
        author: &str,
        note: &str,
    ) -> Result<AgentMdSnapshot, String> {
        let char_count = content.chars().count();
        if char_count > MAX_AGENT_MD_CHARS {
            return Err(format!(
                "Agent.md exceeds maximum length ({char_count} > {MAX_AGENT_MD_CHARS} chars)"
            ));
        }
        let author = {
            let trimmed = author.trim();
            if trimmed.is_empty() {
                "console"
            } else {
                trimmed
            }
        };
        let note = note.trim();
        let now = crate::id::now_ms();

        self.store
            .with(|c| {
                let max_ver: i64 = c.query_row(
                    "SELECT COALESCE(MAX(version), 0) FROM agent_md_revisions",
                    [],
                    |r| r.get(0),
                )?;
                let next_ver = max_ver + 1;
                c.execute(
                    "INSERT INTO agent_md_revisions (version, content, author, note, created_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![next_ver, content, author, note, now as i64],
                )?;
                // Keep bounded revision history (latest MAX_AGENT_MD_REVISIONS rows).
                c.execute(
                    "DELETE FROM agent_md_revisions
                     WHERE id NOT IN (
                         SELECT id FROM agent_md_revisions
                         ORDER BY version DESC, id DESC
                         LIMIT ?1
                     )",
                    params![MAX_AGENT_MD_REVISIONS as i64],
                )?;
                Ok(())
            })
            .map_err(|e| e.to_string())?;

        self.snapshot()
    }

    pub fn rollback(
        &self,
        revision_id: i64,
        author: &str,
        note: Option<&str>,
    ) -> Result<AgentMdSnapshot, String> {
        let target = self
            .store
            .with(|c| {
                let mut stmt =
                    c.prepare("SELECT id, version, content FROM agent_md_revisions WHERE id = ?1")?;
                let mut rows = stmt.query([revision_id])?;
                if let Some(r) = rows.next()? {
                    Ok(Some((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?.max(0) as u64,
                        r.get::<_, String>(2)?,
                    )))
                } else {
                    Ok(None)
                }
            })
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("Agent.md revision {revision_id} not found"))?;

        let default_note = format!("Rollback to v{}", target.1);
        let eff_note = note
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&default_note);
        self.update(&target.2, author, eff_note)
    }
}
