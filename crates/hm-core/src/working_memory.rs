//! Long-term working memory (`working_memory` table).
//!
//! Completely separate from the 200-item household notes in [`crate::memory`].
//! Working memory stores project paths, last progress, active work context,
//! and other multi-session work items keyed by `(scope_person, scope_device,
//! key_or_topic)`. It does not share capacity, validation, or confirmation
//! queues with household notes.

mod scoped;

use crate::store::Store;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const MAX_KEY_LEN: usize = 120;
pub const MAX_CONTENT_LEN: usize = 4_000;
pub const MAX_ITEMS: usize = 500;
const PROMPT_MAX_ITEMS: usize = 20;
const PROMPT_MAX_CHARS: usize = 3_000;

/// Explicit scope passed by voice / API / cron callers into prompt assembly
/// and working memory filtering (`#78`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ScopeContext {
    pub device_id: Option<String>,
    pub room: Option<String>,
    pub person_id: Option<String>,
}

impl ScopeContext {
    pub fn allows(&self, person: &str, device: &str) -> bool {
        let norm = self.normalized();
        (person.is_empty() || norm.person_id.as_deref() == Some(person))
            && (device.is_empty() || norm.device_id.as_deref() == Some(device))
    }
    pub fn normalized(&self) -> Self {
        let norm = |o: &Option<String>| {
            o.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Self {
            device_id: norm(&self.device_id),
            room: norm(&self.room),
            person_id: norm(&self.person_id),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct WorkingMemoryItem {
    pub id: String,
    pub scope_person: String,
    pub scope_device: String,
    pub key_or_topic: String,
    pub content: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub access_count: u64,
    #[serde(default)]
    pub last_accessed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at_ms: Option<u64>,
    #[serde(default)]
    pub written_by_device: String,
    #[serde(default)]
    pub source_trace_id: String,
}

pub struct WorkingMemoryStore {
    store: Arc<Store>,
    evicted_total: AtomicU64,
}

const SELECT_COLS: &str =
    "id, scope_person, scope_device, key_or_topic, content, created_ms, updated_ms, \
     pinned, access_count, last_accessed_ms, archived_at_ms, written_by_device, source_trace_id";

impl WorkingMemoryStore {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self {
            store,
            evicted_total: AtomicU64::new(0),
        })
    }

    pub fn evicted_total(&self) -> u64 {
        self.evicted_total.load(Ordering::Relaxed)
    }

    /// Clear in-memory counters on factory reset.
    pub fn reset(&self) {
        self.evicted_total.store(0, Ordering::Relaxed);
    }

    /// Create or update a working memory item keyed by `(scope_person, scope_device, key_or_topic)`.
    pub fn set(
        &self,
        scope_person: &str,
        scope_device: &str,
        key_or_topic: &str,
        content: &str,
    ) -> Result<WorkingMemoryItem, String> {
        self.set_with_provenance(
            scope_person,
            scope_device,
            key_or_topic,
            content,
            None,
            None,
            None,
        )
    }

    /// Create or update a working memory item with optional pin state and
    /// write provenance (`written_by_device`, `source_trace_id`).
    #[allow(clippy::too_many_arguments)]
    pub fn set_with_provenance(
        &self,
        scope_person: &str,
        scope_device: &str,
        key_or_topic: &str,
        content: &str,
        pinned: Option<bool>,
        written_by_device: Option<&str>,
        source_trace_id: Option<&str>,
    ) -> Result<WorkingMemoryItem, String> {
        let person = normalize_scope(scope_person);
        let device = normalize_scope(scope_device);
        let key = normalize_key(key_or_topic)?;
        let content = normalize_content(content)?;
        let now = crate::id::now_ms();
        let writer_dev = written_by_device.map(str::trim).unwrap_or("");
        let trace_id = source_trace_id.map(str::trim).unwrap_or("");

        if let Some(mut existing) = self.get_raw(&person, &device, &key)? {
            existing.content = content;
            existing.updated_ms = now;
            if let Some(p) = pinned {
                existing.pinned = p;
            }
            if !writer_dev.is_empty() {
                existing.written_by_device = writer_dev.to_string();
            }
            if !trace_id.is_empty() {
                existing.source_trace_id = trace_id.to_string();
            }
            let row = existing.clone();
            self.store
                .with(|c| {
                    c.execute(
                        "UPDATE working_memory
                         SET content=?2, updated_ms=?3, pinned=?4, written_by_device=?5, source_trace_id=?6
                         WHERE id=?1",
                        rusqlite::params![
                            row.id,
                            row.content,
                            row.updated_ms as i64,
                            if row.pinned { 1i64 } else { 0i64 },
                            row.written_by_device,
                            row.source_trace_id,
                        ],
                    )?;
                    Ok(())
                })
                .map_err(|e| e.to_string())?;
            return Ok(existing);
        }

        let (total, epoch) = self
            .store
            .with(|c| {
                let total: i64 =
                    c.query_row("SELECT COUNT(*) FROM working_memory", [], |r| r.get(0))?;
                let epoch: i64 =
                    c.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| {
                        r.get(0)
                    })?;
                Ok((total.max(0) as usize, epoch))
            })
            .map_err(|e| e.to_string())?;
        if total >= MAX_ITEMS {
            self.evict_one_at_epoch(now, epoch)?;
        }

        let item = WorkingMemoryItem {
            id: crate::id::next("WM"),
            scope_person: person,
            scope_device: device,
            key_or_topic: key,
            content,
            created_ms: now,
            updated_ms: now,
            pinned: pinned.unwrap_or(false),
            access_count: 0,
            last_accessed_ms: now,
            archived_at_ms: None,
            written_by_device: writer_dev.to_string(),
            source_trace_id: trace_id.to_string(),
        };
        let row = item.clone();
        self.store
            .with(|c| {
                c.execute(
                    "INSERT INTO working_memory
                     (id, scope_person, scope_device, key_or_topic, content, created_ms, updated_ms,
                      pinned, access_count, last_accessed_ms, archived_at_ms, written_by_device, source_trace_id)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    rusqlite::params![
                        row.id,
                        row.scope_person,
                        row.scope_device,
                        row.key_or_topic,
                        row.content,
                        row.created_ms as i64,
                        row.updated_ms as i64,
                        if row.pinned { 1i64 } else { 0i64 },
                        row.access_count as i64,
                        row.last_accessed_ms as i64,
                        row.archived_at_ms.map(|v| v as i64),
                        row.written_by_device,
                        row.source_trace_id,
                    ],
                )?;
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        Ok(item)
    }

    /// Evict the lowest-utility unpinned item (`pinned ASC, access_count ASC,
    /// last_accessed_ms ASC, updated_ms ASC`). Checks `data_epoch` and
    /// `cfg.enabled && cfg.upload_enabled` inside the same `store.with(...)`
    /// transaction before inserting into `shared_memory_outbox` and deleting.
    pub(crate) fn evict_one_at_epoch(&self, now: u64, expected_epoch: i64) -> Result<(), String> {
        let evicted = self
            .store
            .with(|c| {
                use rusqlite::OptionalExtension;
                let tx = c.unchecked_transaction()?;
                let current_epoch: i64 =
                    tx.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| r.get(0))?;
                if current_epoch != expected_epoch {
                    return Err(crate::store::StoreError::Authority(
                        "working_memory epoch changed".into(),
                    ));
                }

                let sql = format!(
                    "SELECT {SELECT_COLS} FROM working_memory
                     WHERE pinned = 0
                     ORDER BY pinned ASC, access_count ASC, last_accessed_ms ASC, updated_ms ASC, id ASC
                     LIMIT 1"
                );
                let victim = tx.query_row(&sql, [], row_to_item).optional()?;
                let Some(victim) = victim else {
                    return Ok(false);
                };

                let shared_cfg: crate::config::SharedMemoryConfig = tx
                    .query_row(
                        "SELECT value FROM config_kv WHERE key=?1",
                        [crate::config::SHARED_MEMORY_KEY],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .and_then(|raw| serde_json::from_str(&raw).ok())
                    .unwrap_or_default();

                if shared_cfg.enabled && shared_cfg.upload_enabled {
                    let source_dev = if victim.written_by_device.is_empty() {
                        victim.scope_device.as_str()
                    } else {
                        victim.written_by_device.as_str()
                    };
                    let obs_id = crate::id::next("SMO");
                    let archived_content = format!(
                        "[archived working_memory: {}] {}",
                        victim.key_or_topic, victim.content
                    );
                    let truncated = crate::shared_memory::truncate_observation_content(&archived_content);
                    let (count, total_bytes): (i64, i64) = tx.query_row(
                        "SELECT COUNT(*), COALESCE(SUM(LENGTH(CAST(content AS BLOB))), 0) FROM shared_memory_outbox",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    if (count as usize) < crate::shared_memory::OUTBOX_MAX_ENTRIES
                        && (total_bytes as usize).saturating_add(truncated.len())
                            <= crate::shared_memory::OUTBOX_MAX_BYTES
                    {
                        tx.execute(
                            "INSERT INTO shared_memory_outbox
                             (id, device_id, room, person_id, trace_id, content, created_ms,
                              source_device, scope_device, scope_person, domain, record_id, event_kind, dimension)
                             VALUES (?1,?2,'',?3,?4,?5,?6,?2,?7,?3,'workmem',?8,'capacity_eviction','general')",
                            rusqlite::params![
                                obs_id,
                                source_dev,
                                victim.scope_person,
                                victim.source_trace_id,
                                truncated,
                                now as i64,
                                victim.scope_device,
                                victim.id,
                            ],
                        )?;
                    }
                }

                tx.execute("DELETE FROM working_memory WHERE id=?1", [&victim.id])?;
                tx.commit()?;
                Ok(true)
            })
            .map_err(|e| e.to_string())?;

        if !evicted {
            return Err(format!(
                "工作记忆已满且全部 {MAX_ITEMS} 条均已置顶锁定，请先取消置顶或删除不再需要的条目"
            ));
        }
        self.evicted_total.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn set_pinned(&self, id: &str, pinned: bool) -> Result<Option<WorkingMemoryItem>, String> {
        let n = self
            .store
            .with(|c| {
                Ok(c.execute(
                    "UPDATE working_memory SET pinned=?2 WHERE id=?1",
                    rusqlite::params![id, if pinned { 1i64 } else { 0i64 }],
                )?)
            })
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(None);
        }
        self.get_by_id(id)
    }

    pub fn pinned_count(&self) -> Result<usize, String> {
        self.store
            .with(|c| {
                let n: i64 = c.query_row(
                    "SELECT COUNT(*) FROM working_memory WHERE pinned != 0",
                    [],
                    |r| r.get(0),
                )?;
                Ok(n.max(0) as usize)
            })
            .map_err(|e| e.to_string())
    }

    pub fn get(
        &self,
        scope_person: &str,
        scope_device: &str,
        key_or_topic: &str,
    ) -> Result<Option<WorkingMemoryItem>, String> {
        let item = self.get_raw(scope_person, scope_device, key_or_topic)?;
        if let Some(ref found) = item {
            self.touch_items(std::slice::from_ref(&found.id));
        }
        Ok(item)
    }

    fn get_raw(
        &self,
        scope_person: &str,
        scope_device: &str,
        key_or_topic: &str,
    ) -> Result<Option<WorkingMemoryItem>, String> {
        let person = normalize_scope(scope_person);
        let device = normalize_scope(scope_device);
        let key = key_or_topic.trim();
        if key.is_empty() {
            return Ok(None);
        }
        self.store
            .with(|c| {
                use rusqlite::OptionalExtension;
                let sql = format!(
                    "SELECT {SELECT_COLS} FROM working_memory
                     WHERE scope_person=?1 AND scope_device=?2 AND key_or_topic=?3"
                );
                Ok(
                    c.query_row(&sql, rusqlite::params![person, device, key], row_to_item)
                        .optional()?,
                )
            })
            .map_err(|e| e.to_string())
    }

    pub fn get_by_id(&self, id: &str) -> Result<Option<WorkingMemoryItem>, String> {
        self.store
            .with(|c| {
                use rusqlite::OptionalExtension;
                let sql = format!("SELECT {SELECT_COLS} FROM working_memory WHERE id=?1");
                Ok(c.query_row(&sql, [id], row_to_item).optional()?)
            })
            .map_err(|e| e.to_string())
    }

    /// List working memory items, pinned & newest updated first.
    /// `None` means no filter on that scope dimension; `Some("")` matches unscoped/global items.
    pub fn list(
        &self,
        scope_person: Option<&str>,
        scope_device: Option<&str>,
    ) -> Result<Vec<WorkingMemoryItem>, String> {
        let items = self.list_all().map_err(|e| e.to_string())?;
        let person_filter = scope_person.map(normalize_scope);
        let device_filter = scope_device.map(normalize_scope);
        Ok(items
            .into_iter()
            .filter(|item| {
                person_filter
                    .as_ref()
                    .is_none_or(|p| item.scope_person == *p)
                    && device_filter
                        .as_ref()
                        .is_none_or(|d| item.scope_device == *d)
            })
            .collect())
    }

    pub fn delete(
        &self,
        scope_person: &str,
        scope_device: &str,
        key_or_topic: &str,
    ) -> Result<bool, String> {
        let person = normalize_scope(scope_person);
        let device = normalize_scope(scope_device);
        let key = key_or_topic.trim();
        let n = self
            .store
            .with(|c| {
                Ok(c.execute(
                    "DELETE FROM working_memory
                     WHERE scope_person=?1 AND scope_device=?2 AND key_or_topic=?3",
                    rusqlite::params![person, device, key],
                )?)
            })
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    pub fn delete_by_id(&self, id: &str) -> Result<bool, String> {
        let n = self
            .store
            .with(|c| Ok(c.execute("DELETE FROM working_memory WHERE id=?1", [id])?))
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    pub fn clear(&self) -> Result<usize, String> {
        self.clear_scoped(None, None)
    }

    pub fn clear_scoped(
        &self,
        scope_person: Option<&str>,
        scope_device: Option<&str>,
    ) -> Result<usize, String> {
        let person = scope_person.map(normalize_scope).filter(|s| !s.is_empty());
        let device = scope_device.map(normalize_scope).filter(|s| !s.is_empty());
        self.store
            .with(|c| {
                let n = match (person.as_deref(), device.as_deref()) {
                    (Some(p), Some(d)) => c.execute(
                        "DELETE FROM working_memory WHERE scope_person=?1 AND scope_device=?2",
                        rusqlite::params![p, d],
                    )?,
                    (Some(p), None) => c.execute(
                        "DELETE FROM working_memory WHERE scope_person=?1",
                        rusqlite::params![p],
                    )?,
                    (None, Some(d)) => c.execute(
                        "DELETE FROM working_memory WHERE scope_device=?1",
                        rusqlite::params![d],
                    )?,
                    (None, None) => c.execute("DELETE FROM working_memory", [])?,
                };
                Ok(n)
            })
            .map_err(|e| e.to_string())
    }

    /// Render relevant working memory items for the system prompt using an
    /// explicit [`ScopeContext`] privacy boundary (`#78`):
    /// - Global entries (`scope_person == "" && scope_device == ""`): visible to all.
    /// - Device entries (`!scope_device.is_empty()`): visible ONLY when
    ///   `scope.device_id.as_deref() == Some(item.scope_device.as_str())`.
    /// - Person entries (`!scope_person.is_empty()`): visible ONLY when
    ///   `scope.person_id.as_deref() == Some(item.scope_person.as_str())`.
    pub fn render_for_prompt_scoped(&self, scope: &ScopeContext) -> Option<String> {
        let all = self.list_all().ok()?;
        let norm = scope.normalized();
        let matched: Vec<WorkingMemoryItem> = all
            .into_iter()
            .filter(|item| norm.allows(&item.scope_person, &item.scope_device))
            .collect();
        if matched.is_empty() {
            return None;
        }
        let mut out = String::new();
        let mut used = 0usize;
        let mut count = 0usize;
        let mut touched_ids = Vec::new();
        for item in matched {
            let scope_tag = match (item.scope_person.is_empty(), item.scope_device.is_empty()) {
                (true, true) => String::new(),
                (false, true) => format!(" (人: {})", item.scope_person),
                (true, false) => format!(" (设备: {})", item.scope_device),
                (false, false) => {
                    format!(" (人: {}, 设备: {})", item.scope_person, item.scope_device)
                }
            };
            let line = format!("- [{}{}] {}\n", item.key_or_topic, scope_tag, item.content);
            let line_chars = line.chars().count();
            if count >= PROMPT_MAX_ITEMS || (count > 0 && used + line_chars > PROMPT_MAX_CHARS) {
                break;
            }
            used += line_chars;
            count += 1;
            touched_ids.push(item.id);
            out.push_str(&line);
        }
        if out.is_empty() {
            None
        } else {
            self.touch_items(&touched_ids);
            Some(out)
        }
    }

    /// Convenience wrapper delegating to [`Self::render_for_prompt_scoped`].
    pub fn render_for_prompt(
        &self,
        scope_person: Option<&str>,
        scope_device: Option<&str>,
    ) -> Option<String> {
        let scope = ScopeContext {
            person_id: scope_person.map(str::to_string),
            device_id: scope_device.map(str::to_string),
            room: None,
        };
        self.render_for_prompt_scoped(&scope)
    }

    fn touch_items(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let now = crate::id::now_ms() as i64;
        let _ = self.store.with(|c| {
            for id in ids {
                c.execute(
                    "UPDATE working_memory
                     SET access_count = access_count + 1, last_accessed_ms = ?2
                     WHERE id = ?1",
                    rusqlite::params![id, now],
                )?;
            }
            Ok(())
        });
    }

    fn list_all(&self) -> crate::store::Result<Vec<WorkingMemoryItem>> {
        self.store.with(|c| {
            let sql = format!(
                "SELECT {SELECT_COLS} FROM working_memory ORDER BY pinned DESC, updated_ms DESC, id"
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([], row_to_item)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }
}

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorkingMemoryItem> {
    Ok(WorkingMemoryItem {
        id: r.get(0)?,
        scope_person: r.get(1)?,
        scope_device: r.get(2)?,
        key_or_topic: r.get(3)?,
        content: r.get(4)?,
        created_ms: r.get::<_, i64>(5)? as u64,
        updated_ms: r.get::<_, i64>(6)? as u64,
        pinned: r.get::<_, i64>(7)? != 0,
        access_count: r.get::<_, i64>(8)?.max(0) as u64,
        last_accessed_ms: r.get::<_, i64>(9)?.max(0) as u64,
        archived_at_ms: r.get::<_, Option<i64>>(10)?.map(|v| v.max(0) as u64),
        written_by_device: r.get(11)?,
        source_trace_id: r.get(12)?,
    })
}

fn normalize_scope(s: &str) -> String {
    s.trim().to_string()
}

fn normalize_key(key: &str) -> Result<String, String> {
    let k = key.split_whitespace().collect::<Vec<_>>().join(" ");
    if k.is_empty() {
        return Err("working memory key_or_topic cannot be empty".into());
    }
    if k.chars().count() > MAX_KEY_LEN {
        return Err(format!(
            "working memory key_or_topic cannot exceed {MAX_KEY_LEN} chars"
        ));
    }
    Ok(k)
}

fn normalize_content(content: &str) -> Result<String, String> {
    let c = content.trim().to_string();
    if c.is_empty() {
        return Err("working memory content cannot be empty".into());
    }
    if c.chars().count() > MAX_CONTENT_LEN {
        return Err(format!(
            "working memory content cannot exceed {MAX_CONTENT_LEN} chars"
        ));
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigService;
    use crate::memory::MemoryService;

    fn store() -> Arc<Store> {
        Arc::new(Store::open_in_memory().unwrap())
    }

    #[test]
    fn crud_and_scoping() {
        let s = WorkingMemoryStore::new(store());
        assert!(s.render_for_prompt(None, None).is_none());

        // Set global, per-person, and per-device items with same key.
        let g = s
            .set("", "", "project_path", "/Users/dev/home-mate-b")
            .unwrap();
        let p = s
            .set("alice", "", "project_path", "/Users/alice/workspace")
            .unwrap();
        let _d = s
            .set(
                "",
                "dot2-a",
                "last_progress",
                "Finished M6 schema and store",
            )
            .unwrap();
        let pd = s
            .set(
                "bob",
                "dot2-b",
                "last_progress",
                "Debugging gateway reconnect",
            )
            .unwrap();

        assert_ne!(g.id, p.id);
        assert_eq!(s.list(None, None).unwrap().len(), 4);
        assert_eq!(s.list(Some("alice"), None).unwrap().len(), 1);
        assert_eq!(s.list(None, Some("dot2-a")).unwrap().len(), 1);

        // Upsert updates content and preserves id/created_ms.
        let updated = s
            .set("", "", "project_path", "/Users/dev/home-mate-b-v2")
            .unwrap();
        assert_eq!(updated.id, g.id);
        assert_eq!(updated.created_ms, g.created_ms);
        assert_eq!(updated.content, "/Users/dev/home-mate-b-v2");

        // Prompt rendering filters by device and person while keeping global items,
        // and NEVER leaks person-scoped or other-device entries.
        let global_only = s
            .render_for_prompt_scoped(&ScopeContext::default())
            .unwrap();
        assert!(global_only.contains("/Users/dev/home-mate-b-v2"));
        assert!(!global_only.contains("/Users/alice/workspace"));
        assert!(!global_only.contains("Finished M6 schema and store"));
        assert!(!global_only.contains("Debugging gateway reconnect"));

        let dev_a_prompt = s.render_for_prompt(None, Some("dot2-a")).unwrap();
        assert!(dev_a_prompt.contains("/Users/dev/home-mate-b-v2"));
        assert!(dev_a_prompt.contains("Finished M6 schema and store"));
        assert!(!dev_a_prompt.contains("Debugging gateway reconnect"));
        assert!(!dev_a_prompt.contains("/Users/alice/workspace"));

        let bob_b_prompt = s.render_for_prompt(Some("bob"), Some("dot2-b")).unwrap();
        assert!(bob_b_prompt.contains("Debugging gateway reconnect"));
        assert!(!bob_b_prompt.contains("alice"));

        assert!(s.delete("", "dot2-a", "last_progress").unwrap());
        assert!(s.get("", "dot2-a", "last_progress").unwrap().is_none());
        assert!(s.delete_by_id(&pd.id).unwrap());
        assert_eq!(s.clear().unwrap(), 2);
        assert!(s.list(None, None).unwrap().is_empty());
    }

    #[test]
    fn scope_context_privacy_boundary() {
        let s = WorkingMemoryStore::new(store());
        s.set("", "", "household_wifi", "home-5g").unwrap();
        s.set("alice", "", "secret_diary", "alice-only-note")
            .unwrap();
        s.set("", "dot2-a", "speaker_eq", "living-room-eq").unwrap();

        // person_id == None, device_id == None -> ONLY global.
        let anon = s
            .render_for_prompt_scoped(&ScopeContext::default())
            .unwrap();
        assert!(anon.contains("home-5g"));
        assert!(!anon.contains("alice-only-note"));
        assert!(!anon.contains("living-room-eq"));

        // person_id == Some("bob") -> global only, never alice.
        let bob = s
            .render_for_prompt_scoped(&ScopeContext {
                person_id: Some("bob".into()),
                device_id: None,
                room: None,
            })
            .unwrap();
        assert!(bob.contains("home-5g"));
        assert!(!bob.contains("alice-only-note"));
        assert!(!bob.contains("living-room-eq"));

        // person_id == Some("alice"), device_id == Some("dot2-a") -> all three.
        let alice = s
            .render_for_prompt_scoped(&ScopeContext {
                person_id: Some("alice".into()),
                device_id: Some("dot2-a".into()),
                room: Some("living-room".into()),
            })
            .unwrap();
        assert!(alice.contains("home-5g"));
        assert!(alice.contains("alice-only-note"));
        assert!(alice.contains("living-room-eq"));
    }

    #[test]
    fn eviction_preserves_pinned_and_fails_when_all_pinned() {
        let db = store();
        let s = WorkingMemoryStore::new(db.clone());

        // Enable shared memory so evicted item is enqueued to outbox.
        ConfigService::new(db.clone())
            .save_shared_memory(
                crate::config::SharedMemoryConfig {
                    enabled: true,
                    nas_base_url: "http://127.0.0.1:9999".into(),
                    upload_enabled: true,
                },
                "test",
            )
            .unwrap();

        let pinned_item = s.set("", "", "pinned-0", "keep me").unwrap();
        s.set_pinned(&pinned_item.id, true).unwrap();

        for i in 1..MAX_ITEMS {
            s.set("", "", &format!("item-{i}"), &format!("val-{i}"))
                .unwrap();
        }
        assert_eq!(s.list(None, None).unwrap().len(), MAX_ITEMS);
        assert_eq!(s.evicted_total(), 0);

        // Adding 501st item evicts lowest-utility unpinned item and preserves pinned.
        s.set("", "", "item-500", "val-500").unwrap();
        assert_eq!(s.list(None, None).unwrap().len(), MAX_ITEMS);
        assert_eq!(s.evicted_total(), 1);
        assert!(s.get("", "", "pinned-0").unwrap().is_some());

        let outbox = crate::shared_memory::SharedMemoryService::new(db.clone())
            .list_outbox(10)
            .unwrap();
        assert_eq!(outbox.len(), 1);
        assert!(outbox[0].content.contains("archived working_memory"));

        // Pin all 500 items -> 501st write returns a clean error.
        for item in s.list(None, None).unwrap() {
            s.set_pinned(&item.id, true).unwrap();
        }
        let err = s.set("", "", "overflow", "should fail").unwrap_err();
        assert!(err.contains("置顶") || err.contains("已满"), "{err}");
    }

    #[test]
    fn longer_than_200_chars_and_isolated_from_household_notes() {
        let db = store();
        let wm = WorkingMemoryStore::new(db.clone());
        let hm = MemoryService::new(db);

        // Working memory accepts > 200 chars (household notes reject > 200).
        let long_text = "x".repeat(500);
        assert!(hm.add(&long_text, "fact", "console", false).is_err());
        let item = wm
            .set("alice", "dot2-a", "active_context", &long_text)
            .unwrap();
        assert_eq!(item.content.chars().count(), 500);

        // Filling working memory does not affect household notes' 200-item cap.
        for i in 0..205 {
            wm.set("", "", &format!("topic-{i}"), &format!("detail-{i}"))
                .unwrap();
        }
        assert!(hm.list().unwrap().is_empty());
        hm.add("奶奶周日来吃饭", "event", "console", false).unwrap();
        assert_eq!(hm.list().unwrap().len(), 1);
    }

    #[test]
    fn evict_one_respects_upload_enabled_and_epoch_fence() {
        let db = store();
        let s = WorkingMemoryStore::new(db.clone());
        let cfg = ConfigService::new(db.clone());

        // 1. enabled=true, upload_enabled=false -> eviction must NOT enqueue into outbox.
        cfg.save_shared_memory(
            crate::config::SharedMemoryConfig {
                enabled: true,
                nas_base_url: "http://127.0.0.1:9999".into(),
                upload_enabled: false,
            },
            "test",
        )
        .unwrap();

        for i in 0..MAX_ITEMS {
            s.set("", "", &format!("item-{i}"), &format!("val-{i}"))
                .unwrap();
        }
        s.set("", "", "item-overflow", "val-overflow").unwrap();
        assert_eq!(s.evicted_total(), 1);
        let outbox = crate::shared_memory::SharedMemoryService::new(db.clone())
            .list_outbox(10)
            .unwrap();
        assert!(
            outbox.is_empty(),
            "upload_enabled=false must prevent evict_one from inserting into outbox"
        );

        // 2. Capture pre-reset epoch, enable upload, then factory_reset + s.reset().
        cfg.save_shared_memory(
            crate::config::SharedMemoryConfig {
                enabled: true,
                nas_base_url: "http://127.0.0.1:9999".into(),
                upload_enabled: true,
            },
            "test",
        )
        .unwrap();
        let old_epoch: i64 = db
            .with(|c| {
                Ok(
                    c.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();

        db.factory_reset(false).unwrap();
        s.reset();
        assert_eq!(s.evicted_total(), 0);

        // Re-enable shared memory and add a post-reset item; a stale evict_one
        // carrying old_epoch must be rejected without touching outbox or working_memory.
        cfg.save_shared_memory(
            crate::config::SharedMemoryConfig {
                enabled: true,
                nas_base_url: "http://127.0.0.1:9999".into(),
                upload_enabled: true,
            },
            "test",
        )
        .unwrap();
        s.set("", "", "post-reset", "fresh").unwrap();
        let err = s
            .evict_one_at_epoch(crate::id::now_ms(), old_epoch)
            .unwrap_err();
        assert!(err.contains("epoch"), "unexpected error: {err}");
        let outbox_after = crate::shared_memory::SharedMemoryService::new(db)
            .list_outbox(10)
            .unwrap();
        assert!(outbox_after.is_empty());
        assert_eq!(s.evicted_total(), 0);
        assert!(s.get("", "", "post-reset").unwrap().is_some());
    }
}
