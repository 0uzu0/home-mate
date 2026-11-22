//! Auditable `USER.md` profile & preference store (`#83`, W4b-2).
//!
//! Stores structured `USER.md` entries with explicit provenance
//! (`source_device`, `trace_id`, `turn_ts`, `rationale`, `author`, `status`).
//! Active entries are injected into the system prompt scoped by `ScopeContext`;
//! Console users can add, edit, or revert any entry at any time.

mod scoped;

use crate::store::Store;
use crate::working_memory::ScopeContext;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const MAX_USER_MD_KEY_CHARS: usize = 120;
pub const MAX_USER_MD_CONTENT_CHARS: usize = 1_000;
pub const MAX_USER_MD_RATIONALE_CHARS: usize = 400;
const PROMPT_MAX_ENTRIES: usize = 30;
const PROMPT_MAX_CHARS: usize = 3_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserMdEntry {
    pub id: String,
    pub person_id: String,
    #[serde(default)]
    pub scope_device: String,
    pub key: String,
    pub content: String,
    pub source_device: String,
    pub trace_id: String,
    pub turn_ts: u64,
    pub rationale: String,
    pub author: String,
    pub status: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub reverted_ms: Option<u64>,
    pub reverted_by: String,
}

pub struct UserMdService {
    store: Arc<Store>,
}

impl UserMdService {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    pub fn list(&self, status_filter: Option<&str>) -> Result<Vec<UserMdEntry>, String> {
        let filter = status_filter
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "all")
            .map(str::to_string);
        self.store
            .with(|c| {
                let sql = if filter.is_some() {
                    "SELECT id, person_id, key, content, source_device, trace_id, turn_ts,
                            rationale, author, status, created_ms, updated_ms, reverted_ms, reverted_by, scope_device
                     FROM user_md_entries WHERE status = ?1 ORDER BY updated_ms DESC, id DESC"
                } else {
                    "SELECT id, person_id, key, content, source_device, trace_id, turn_ts,
                            rationale, author, status, created_ms, updated_ms, reverted_ms, reverted_by, scope_device
                     FROM user_md_entries ORDER BY updated_ms DESC, id DESC"
                };
                let mut stmt = c.prepare(sql)?;
                let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<UserMdEntry> {
                    Ok(UserMdEntry {
                        id: r.get(0)?,
                        person_id: r.get(1)?,
                        scope_device: r.get(14)?,
                        key: r.get(2)?,
                        content: r.get(3)?,
                        source_device: r.get(4)?,
                        trace_id: r.get(5)?,
                        turn_ts: r.get::<_, i64>(6)?.max(0) as u64,
                        rationale: r.get(7)?,
                        author: r.get(8)?,
                        status: r.get(9)?,
                        created_ms: r.get::<_, i64>(10)?.max(0) as u64,
                        updated_ms: r.get::<_, i64>(11)?.max(0) as u64,
                        reverted_ms: r.get::<_, Option<i64>>(12)?.map(|v| v.max(0) as u64),
                        reverted_by: r.get(13)?,
                    })
                };
                let mut out = Vec::new();
                if let Some(ref f) = filter {
                    for row in stmt.query_map([f], map_row)? {
                        out.push(row?);
                    }
                } else {
                    for row in stmt.query_map([], map_row)? {
                        out.push(row?);
                    }
                }
                Ok(out)
            })
            .map_err(|e| e.to_string())
    }

    pub fn upsert_console(
        &self,
        id: Option<&str>,
        person_id: &str,
        key: &str,
        content: &str,
        rationale: &str,
    ) -> Result<UserMdEntry, String> {
        let (person_id, key, content, rationale) =
            validate_fields(person_id, key, content, rationale)?;
        let now = crate::id::now_ms();
        let existing_id = id.map(str::trim).filter(|s| !s.is_empty());

        if let Some(entry_id) = existing_id {
            let updated = self
                .store
                .with(|c| {
                    Ok(c.execute(
                        "UPDATE user_md_entries
                         SET person_id = ?1, key = ?2, content = ?3, rationale = ?4,
                             author = 'console', status = 'active',
                             updated_ms = ?5, turn_ts = ?5,
                             reverted_ms = NULL, reverted_by = ''
                         WHERE id = ?6",
                        params![person_id, key, content, rationale, now as i64, entry_id],
                    )?)
                })
                .map_err(|e| e.to_string())?;
            if updated > 0 {
                return self
                    .get_by_id(entry_id)?
                    .ok_or_else(|| "updated entry missing".to_string());
            }
        }

        let new_id = existing_id
            .map(str::to_string)
            .unwrap_or_else(|| crate::id::next("umd"));
        self.insert_entry(UserMdEntry {
            id: new_id.clone(),
            person_id,
            scope_device: String::new(),
            key,
            content,
            source_device: "console".to_string(),
            trace_id: String::new(),
            turn_ts: now,
            rationale,
            author: "console".to_string(),
            status: "active".to_string(),
            created_ms: now,
            updated_ms: now,
            reverted_ms: None,
            reverted_by: String::new(),
        })?;
        self.get_by_id(&new_id)?
            .ok_or_else(|| "inserted entry missing".to_string())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn propose(
        &self,
        person_id: &str,
        key: &str,
        content: &str,
        source_device: &str,
        trace_id: &str,
        rationale: &str,
        author: &str,
    ) -> Result<UserMdEntry, String> {
        let (person_id, key, content, rationale) =
            validate_fields(person_id, key, content, rationale)?;
        let now = crate::id::now_ms();
        let entry_id = crate::id::next("umd");
        let author = {
            let t = author.trim();
            if t.is_empty() {
                "model"
            } else {
                t
            }
        };
        let entry = UserMdEntry {
            id: entry_id.clone(),
            person_id,
            scope_device: String::new(),
            key,
            content,
            source_device: source_device.trim().to_string(),
            trace_id: trace_id.trim().to_string(),
            turn_ts: now,
            rationale,
            author: author.to_string(),
            status: "active".to_string(),
            created_ms: now,
            updated_ms: now,
            reverted_ms: None,
            reverted_by: String::new(),
        };
        self.insert_entry(entry)?;
        self.get_by_id(&entry_id)?
            .ok_or_else(|| "proposed entry missing".to_string())
    }

    pub fn revert(&self, id: &str, actor: &str) -> Result<UserMdEntry, String> {
        let id = id.trim();
        if id.is_empty() {
            return Err("entry id is required".into());
        }
        let now = crate::id::now_ms();
        let actor = {
            let t = actor.trim();
            if t.is_empty() {
                "console"
            } else {
                t
            }
        };
        let changed = self
            .store
            .with(|c| {
                Ok(c.execute(
                    "UPDATE user_md_entries
                     SET status = 'reverted', reverted_ms = ?1, reverted_by = ?2, updated_ms = ?1
                     WHERE id = ?3",
                    params![now as i64, actor, id],
                )?)
            })
            .map_err(|e| e.to_string())?;
        if changed == 0 {
            return Err(format!("USER.md entry '{id}' not found"));
        }
        self.get_by_id(id)?
            .ok_or_else(|| format!("USER.md entry '{id}' not found"))
    }

    pub fn render_for_prompt_scoped(&self, scope: &ScopeContext) -> Option<String> {
        let entries = self.list(Some("active")).ok()?;
        let norm = scope.normalized();
        let matched: Vec<UserMdEntry> = entries
            .into_iter()
            .filter(|e| norm.allows(&e.person_id, &e.scope_device))
            .collect();
        if matched.is_empty() {
            return None;
        }
        let mut out = String::new();
        let mut used = 0usize;
        let mut count = 0usize;
        for e in matched {
            let scope_tag = if e.person_id.is_empty() {
                "household"
            } else {
                e.person_id.as_str()
            };
            let line = format!("- [{scope_tag}] {}: {}\n", e.key, e.content);
            let line_chars = line.chars().count();
            if count >= PROMPT_MAX_ENTRIES || (count > 0 && used + line_chars > PROMPT_MAX_CHARS) {
                break;
            }
            used += line_chars;
            count += 1;
            out.push_str(&line);
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    fn get_by_id(&self, id: &str) -> Result<Option<UserMdEntry>, String> {
        self.store
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT id, person_id, key, content, source_device, trace_id, turn_ts,
                            rationale, author, status, created_ms, updated_ms, reverted_ms, reverted_by, scope_device
                     FROM user_md_entries WHERE id = ?1",
                )?;
                let mut rows = stmt.query([id])?;
                if let Some(r) = rows.next()? {
                    Ok(Some(UserMdEntry {
                        id: r.get(0)?,
                        person_id: r.get(1)?,
                        scope_device: r.get(14)?,
                        key: r.get(2)?,
                        content: r.get(3)?,
                        source_device: r.get(4)?,
                        trace_id: r.get(5)?,
                        turn_ts: r.get::<_, i64>(6)?.max(0) as u64,
                        rationale: r.get(7)?,
                        author: r.get(8)?,
                        status: r.get(9)?,
                        created_ms: r.get::<_, i64>(10)?.max(0) as u64,
                        updated_ms: r.get::<_, i64>(11)?.max(0) as u64,
                        reverted_ms: r.get::<_, Option<i64>>(12)?.map(|v| v.max(0) as u64),
                        reverted_by: r.get(13)?,
                    }))
                } else {
                    Ok(None)
                }
            })
            .map_err(|e| e.to_string())
    }

    fn insert_entry(&self, entry: UserMdEntry) -> Result<(), String> {
        self.store
            .with(|c| {
                c.execute(
                    "INSERT INTO user_md_entries
                     (id, person_id, key, content, source_device, trace_id, turn_ts,
                      rationale, author, status, created_ms, updated_ms, reverted_ms, reverted_by)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        entry.id,
                        entry.person_id,
                        entry.key,
                        entry.content,
                        entry.source_device,
                        entry.trace_id,
                        entry.turn_ts as i64,
                        entry.rationale,
                        entry.author,
                        entry.status,
                        entry.created_ms as i64,
                        entry.updated_ms as i64,
                        entry.reverted_ms.map(|v| v as i64),
                        entry.reverted_by,
                    ],
                )?;
                Ok(())
            })
            .map_err(|e| e.to_string())
    }
}

fn validate_fields(
    person_id: &str,
    key: &str,
    content: &str,
    rationale: &str,
) -> Result<(String, String, String, String), String> {
    let person_id = person_id.trim().to_string();
    let key = key.trim().to_string();
    let content = content.trim().to_string();
    let rationale = rationale.trim().to_string();
    if key.is_empty() {
        return Err("key is required".into());
    }
    if content.is_empty() {
        return Err("content is required".into());
    }
    if key.chars().count() > MAX_USER_MD_KEY_CHARS {
        return Err(format!("key exceeds {MAX_USER_MD_KEY_CHARS} chars"));
    }
    if content.chars().count() > MAX_USER_MD_CONTENT_CHARS {
        return Err(format!("content exceeds {MAX_USER_MD_CONTENT_CHARS} chars"));
    }
    if rationale.chars().count() > MAX_USER_MD_RATIONALE_CHARS {
        return Err(format!(
            "rationale exceeds {MAX_USER_MD_RATIONALE_CHARS} chars"
        ));
    }
    Ok((person_id, key, content, rationale))
}
