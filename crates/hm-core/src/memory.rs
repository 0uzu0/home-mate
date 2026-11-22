//! Household memory: short, user-confirmed notes, injected into the prompt.
//!
//! Modelled on xiaozhi's `mem_local_short` (the reference implementation in
//! `xiaoai-voice-entry/server/memory.py`): a few hundred short lines, full-text
//! injection, no vector store. The policy the user set (2026-11-13) is what
//! decides WHAT belongs here:
//!
//! * **Important events and the user's profile** — names, aliases, habits,
//!   preferences, household facts ("奶奶周日来吃饭", "主卧的窗帘叫主卧窗帘").
//! * **Never functional/operational state** — which service is configured,
//!   tool receipts, playback/volume, "the news source is X". Those live in
//!   config/state and change on their own; a speaker that "remembers" them
//!   goes stale and starts refusing things that now work.
//!
//! Everything is visible, editable and clearable in the console (the memory
//! card). Nothing is written by the model unless the user says so ("记住…"):
//! the `memory.remember` tool is the only voice-side writer, and it refuses
//! operational text by policy in its description.

mod scoped;

use crate::store::Store;
use std::sync::Arc;

/// Bounded like the reference: beyond this the list stops being "short-term".
pub const MAX_ITEMS: usize = 200;
/// One memory is one sentence; 200 characters is the reference's limit.
pub const MAX_TEXT: usize = 200;
/// How much memory reaches the prompt: pinned first, then newest.
const PROMPT_MAX_ITEMS: usize = 40;
const PROMPT_MAX_CHARS: usize = 2_400;

pub const KINDS: [&str; 4] = ["fact", "preference", "event", "alias"];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryItem {
    pub id: String,
    #[serde(default)]
    pub scope_person: String,
    #[serde(default)]
    pub scope_device: String,
    pub text: String,
    /// fact | preference | event | alias
    pub kind: String,
    /// console | voice | import | auto
    pub source: String,
    pub pinned: bool,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// M7-1 (2026-11-13): a candidate the model proposed at session end. It is
/// NOT a memory: `status` says whether the background confirmation accepted
/// it, and only `confirmed` rows have a memory item behind them.
///
/// Budget (review: "确认/删除/作用域/预算"): at most [`MAX_PENDING`] pending
/// rows (older ones expire after [`CANDIDATE_TTL_MS`]), at most
/// [`MAX_PER_SESSION`] proposals per session, and the same text as an existing
/// memory or pending candidate is refused (no duplicates).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryCandidate {
    pub id: String,
    #[serde(default)]
    pub scope_person: String,
    #[serde(default)]
    pub scope_device: String,
    pub text: String,
    pub kind: String,
    /// pending | confirmed | rejected
    pub status: String,
    /// Scope: which device's conversation produced it ("" = console/unknown).
    pub device: String,
    pub session_id: String,
    #[serde(default)]
    pub data_epoch: u64,
    /// Why the extractor proposed it (its short reason), or why the
    /// confirmer refused it.
    pub reason: String,
    /// "auto" (background extraction) | "console"
    pub source: String,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// Pending candidates are an inbox, not storage: bounded and self-expiring.
pub const MAX_PENDING: usize = 20;
pub const MAX_PER_SESSION: usize = 3;
pub const CANDIDATE_TTL_MS: u64 = 7 * 86_400_000;

/// Operational state must never become a household memory (this module's
/// policy). The extractor prompt says so; this cheap deterministic gate
/// enforces the obvious cases before the confirmation pass sees them.
const OPERATIONAL_MARKERS: [&str; 18] = [
    "已配置",
    "还没配置",
    "未配置",
    "当前音量",
    "音量是",
    "正在播放",
    "现在播放",
    "固件",
    "端口",
    "密钥",
    "令牌",
    "设备在线",
    "设备离线",
    "服务已",
    "版本是",
    "唤醒词是",
    "http://",
    "https://",
];

fn looks_operational(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    OPERATIONAL_MARKERS
        .iter()
        .any(|m| t.contains(&m.to_ascii_lowercase()))
}

pub struct MemoryService {
    store: Arc<Store>,
}

impl MemoryService {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self { store })
    }

    pub fn list(&self) -> crate::store::Result<Vec<MemoryItem>> {
        self.store.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, text, kind, source, pinned, created_ms, updated_ms, scope_person, scope_device
                 FROM memories ORDER BY pinned DESC, updated_ms DESC, id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(MemoryItem {
                    scope_person: r.get(7)?,
                    scope_device: r.get(8)?,
                    id: r.get(0)?,
                    text: r.get(1)?,
                    kind: r.get(2)?,
                    source: r.get(3)?,
                    pinned: r.get::<_, i64>(4)? != 0,
                    created_ms: r.get::<_, i64>(5)? as u64,
                    updated_ms: r.get::<_, i64>(6)? as u64,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    /// Validate and store one note. A near-identical existing note is updated
    /// instead of duplicated (the reference's "refresh the wording" rule);
    /// a full store refuses instead of silently dropping the oldest note.
    pub fn add(
        &self,
        text: &str,
        kind: &str,
        source: &str,
        pinned: bool,
    ) -> Result<MemoryItem, String> {
        let text = normalize(text);
        if text.is_empty() {
            return Err("记忆不能为空".into());
        }
        if text.chars().count() > MAX_TEXT {
            return Err(format!("一条记忆不要超过 {MAX_TEXT} 字"));
        }
        let kind = normalize_kind(kind)?;
        let source = match source {
            // "auto" = a background candidate the confirmer accepted (M7-1);
            // the console shows it differently from what a person typed.
            "voice" | "import" | "auto" => source,
            _ => "console",
        };
        let existing = self
            .list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|m| m.text == text && m.scope_person.is_empty() && m.scope_device.is_empty());
        let now = crate::id::now_ms();
        if let Some(mut item) = existing {
            item.updated_ms = now;
            item.kind = kind.to_string();
            item.source = source.to_string();
            if pinned {
                item.pinned = true;
            }
            let item2 = item.clone();
            self.store
                .with(|c| {
                    c.execute(
                        "UPDATE memories SET text=?2, kind=?3, source=?4, pinned=?5, updated_ms=?6 WHERE id=?1",
                        rusqlite::params![item2.id, item2.text, item2.kind, item2.source, item2.pinned as i64, item2.updated_ms as i64],
                    )?;
                    Ok(())
                })
                .map_err(|e| e.to_string())?;
            return Ok(item);
        }
        let count = self.list().map_err(|e| e.to_string())?.len();
        if count >= MAX_ITEMS {
            return Err(format!("记忆已满（{MAX_ITEMS} 条）：先在后台删除几条"));
        }
        let item = MemoryItem {
            scope_person: String::new(),
            scope_device: String::new(),
            id: crate::id::next("M"),
            text,
            kind: kind.to_string(),
            source: source.to_string(),
            pinned,
            created_ms: now,
            updated_ms: now,
        };
        let it = item.clone();
        self.store
            .with(|c| {
                c.execute(
                    "INSERT INTO memories (id, text, kind, source, pinned, created_ms, updated_ms)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    rusqlite::params![
                        it.id,
                        it.text,
                        it.kind,
                        it.source,
                        it.pinned as i64,
                        it.created_ms as i64,
                        it.updated_ms as i64
                    ],
                )?;
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        Ok(item)
    }

    pub fn update(
        &self,
        id: &str,
        text: Option<&str>,
        pinned: Option<bool>,
    ) -> Result<Option<MemoryItem>, String> {
        let Some(mut item) = self
            .list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|m| m.id == id)
        else {
            return Ok(None);
        };
        if let Some(t) = text {
            let t = normalize(t);
            if t.is_empty() {
                return Err("记忆不能为空".into());
            }
            if t.chars().count() > MAX_TEXT {
                return Err(format!("一条记忆不要超过 {MAX_TEXT} 字"));
            }
            item.text = t;
        }
        if let Some(p) = pinned {
            item.pinned = p;
        }
        item.updated_ms = crate::id::now_ms();
        let it = item.clone();
        self.store
            .with(|c| {
                c.execute(
                    "UPDATE memories SET text=?2, pinned=?3, updated_ms=?4 WHERE id=?1",
                    rusqlite::params![it.id, it.text, it.pinned as i64, it.updated_ms as i64],
                )?;
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        Ok(Some(item))
    }

    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let n = self
            .store
            .with(|c| Ok(c.execute("DELETE FROM memories WHERE id=?1", [id])?))
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    pub fn clear(&self) -> Result<usize, String> {
        self.store
            .with(|c| Ok(c.execute("DELETE FROM memories", [])?))
            .map_err(|e| e.to_string())
    }

    // --- M7-1 (2026-11-13): auto-extraction candidates ---------------------
    //
    // The session-end extractor is NOT a writer: it can only create pending
    // candidates. `confirm_candidate` is the only path from a candidate to a
    // memory, and the background confirmer calls it explicitly.

    /// Store one proposal. Refused (not silently dropped) when it is
    /// operational state, too long, a duplicate, past the per-session budget,
    /// or the inbox is full — the caller records the reason.
    pub fn add_candidate(
        &self,
        text: &str,
        kind: &str,
        device: &str,
        session_id: &str,
        trigger: &str,
        reason: &str,
    ) -> Result<MemoryCandidate, String> {
        let text = normalize(text);
        if text.is_empty() {
            return Err("候选为空".into());
        }
        if text.chars().count() > MAX_TEXT {
            return Err(format!("一条记忆不要超过 {MAX_TEXT} 字"));
        }
        let kind = normalize_kind(kind)?;
        if looks_operational(&text) {
            return Err("这是运行状态/配置，不是家庭记忆".into());
        }
        let now = crate::id::now_ms();
        self.expire_candidates();
        let existing = self.list().map_err(|e| e.to_string())?;
        if existing.iter().any(|m| m.text == text) {
            return Err("这条记忆已经存在".into());
        }
        let pending = self
            .candidates(Some("pending"))
            .map_err(|e| e.to_string())?;
        if pending.iter().any(|c| c.text == text) {
            return Err("这条候选已经在等待确认".into());
        }
        if pending.len() >= MAX_PENDING {
            return Err(format!("候选太多（{MAX_PENDING} 条待确认）：先在后台处理"));
        }
        if !session_id.is_empty() {
            let seen = self
                .candidates(None)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|c| c.session_id == session_id)
                .count();
            if seen >= MAX_PER_SESSION {
                return Err(format!("本轮已经提了 {MAX_PER_SESSION} 条候选"));
            }
        }
        let cand = MemoryCandidate {
            scope_person: String::new(),
            scope_device: String::new(),
            id: crate::id::next("MC"),
            text,
            kind: kind.to_string(),
            status: "pending".into(),
            device: device.to_string(),
            session_id: session_id.to_string(),
            data_epoch: self
                .store
                .with(|c| {
                    Ok(
                        c.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| {
                            r.get::<_, i64>(0)
                        })?,
                    )
                })
                .map_err(|e| e.to_string())? as u64,
            reason: normalize(reason),
            source: "auto".into(),
            created_ms: now,
            updated_ms: now,
        };
        let c = cand.clone();
        self.store
            .with(|conn| {
                conn.execute(
                    "INSERT INTO memory_candidates
                     (id, text, kind, status, device, session_id, trigger, reason, source, created_ms, updated_ms, data_epoch)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    rusqlite::params![
                        c.id, c.text, c.kind, c.status, c.device, c.session_id,
                        trigger, c.reason, c.source, c.created_ms as i64, c.updated_ms as i64, c.data_epoch as i64
                    ],
                )?;
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        Ok(cand)
    }

    /// List candidates, newest first. `status` = pending|confirmed|rejected
    /// (None = all). Also prunes the table so it stays bounded: pending rows
    /// older than the TTL and resolved rows beyond the retention window go.
    pub fn candidates(&self, status: Option<&str>) -> Result<Vec<MemoryCandidate>, String> {
        let now = crate::id::now_ms();
        let cutoff = now.saturating_sub(CANDIDATE_TTL_MS);
        self.store
            .with(|c| {
                c.execute(
                    "DELETE FROM memory_candidates WHERE status='pending' AND created_ms < ?1",
                    [cutoff as i64],
                )?;
                // Keep the audit table bounded independently of uptime.
                c.execute(
                    "DELETE FROM memory_candidates WHERE status != 'pending' AND updated_ms < ?1",
                    [now.saturating_sub(CANDIDATE_TTL_MS) as i64],
                )?;
                c.execute(
                    "DELETE FROM memory_candidates WHERE id IN (
                        SELECT id FROM memory_candidates ORDER BY created_ms DESC LIMIT -1 OFFSET 200
                     )",
                    [],
                )?;
                let sql = if status.is_some() {
                    "SELECT id, text, kind, status, device, session_id, reason, source, created_ms, updated_ms, scope_person, scope_device, data_epoch
                     FROM memory_candidates WHERE status=?1 ORDER BY created_ms DESC"
                } else {
                    "SELECT id, text, kind, status, device, session_id, reason, source, created_ms, updated_ms, scope_person, scope_device, data_epoch
                     FROM memory_candidates ORDER BY created_ms DESC"
                };
                let args: Vec<&str> = status.into_iter().collect();
                let mut stmt = c.prepare(sql)?;
                let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
                    Ok(MemoryCandidate {
                        scope_person: r.get(10)?,
                        scope_device: r.get(11)?,
                        data_epoch: r.get::<_,i64>(12)?.max(0) as u64,
                        id: r.get(0)?,
                        text: r.get(1)?,
                        kind: r.get(2)?,
                        status: r.get(3)?,
                        device: r.get(4)?,
                        session_id: r.get(5)?,
                        reason: r.get(6)?,
                        source: r.get(7)?,
                        created_ms: r.get::<_, i64>(8)? as u64,
                        updated_ms: r.get::<_, i64>(9)? as u64,
                    })
                })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(|e| e.to_string())
    }

    /// The ONLY way a candidate becomes a memory. Returns the memory item it
    /// produced; a pending row that fails validation stays pending (visible),
    /// never silently accepted.
    pub fn confirm_candidate(&self, id: &str) -> Result<MemoryItem, String> {
        let cand = self
            .candidates(None)?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| "候选不存在".to_string())?;
        if cand.status != "pending" {
            return Err(format!("候选已经是 {} 状态", cand.status));
        }
        self.confirm_scoped_candidate(&cand)
    }

    pub fn reject_candidate(&self, id: &str, reason: &str) -> Result<(), String> {
        let n = self
            .store
            .with(|c| {
                Ok(c.execute(
                    "UPDATE memory_candidates SET status='rejected', reason=?2, updated_ms=?3
                     WHERE id=?1 AND status='pending'",
                    rusqlite::params![id, normalize(reason), crate::id::now_ms() as i64],
                )?)
            })
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("没有可拒绝的待确认候选".into());
        }
        Ok(())
    }

    pub fn delete_candidate(&self, id: &str) -> Result<bool, String> {
        let n = self
            .store
            .with(|c| Ok(c.execute("DELETE FROM memory_candidates WHERE id=?1", [id])?))
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    /// Drop expired pending rows now (also called by `candidates`).
    pub fn expire_candidates(&self) {
        let cutoff = crate::id::now_ms().saturating_sub(CANDIDATE_TTL_MS);
        let _ = self.store.with(|c| {
            c.execute(
                "DELETE FROM memory_candidates WHERE status='pending' AND created_ms < ?1",
                [cutoff as i64],
            )?;
            Ok(())
        });
    }

    /// The prompt block. `None` when empty (an empty heading invites the model
    /// to fill it). Pinned first, then newest; bounded by items AND characters
    /// so a full store cannot push the capability list out of the prompt.
    pub fn render_for_prompt(&self) -> Option<String> {
        self.render_for_prompt_scoped(&crate::working_memory::ScopeContext::default())
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_kind(kind: &str) -> Result<&'static str, String> {
    let k = kind.trim().to_ascii_lowercase();
    for allowed in KINDS {
        if k == allowed {
            return Ok(allowed);
        }
    }
    // The model may say "preference" in Chinese; map the two obvious ones.
    match k.as_str() {
        "偏好" => Ok("preference"),
        "事件" => Ok("event"),
        "别名" | "纠正" => Ok("alias"),
        "事实" => Ok("fact"),
        _ => Err(format!("kind must be one of {}", KINDS.join("|"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc() -> Arc<MemoryService> {
        MemoryService::new(Arc::new(Store::open_in_memory().unwrap()))
    }

    #[test]
    fn add_list_update_delete_clear() {
        let s = svc();
        let a = s
            .add("  奶奶周日来吃饭 ", "event", "console", false)
            .unwrap();
        assert_eq!(a.text, "奶奶周日来吃饭");
        let b = s.add("我不喝冰的", "preference", "voice", true).unwrap();
        assert!(b.pinned);
        let list = s.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, b.id, "pinned first");
        // Same text (whitespace-normalised) refreshes instead of duplicating.
        let again = s.add("奶奶周日来吃饭", "event", "voice", false).unwrap();
        assert_eq!(again.id, a.id);
        assert_eq!(s.list().unwrap().len(), 2);
        assert!(s
            .update(&b.id, Some("我更不喝冰的"), Some(false))
            .unwrap()
            .is_some());
        assert!(s.delete(&b.id).unwrap());
        assert_eq!(s.clear().unwrap(), 1);
        assert!(s.list().unwrap().is_empty());
    }

    #[test]
    fn the_bounds_and_the_prompt_block() {
        let s = svc();
        assert!(s.add("", "fact", "console", false).is_err());
        assert!(s
            .add(&"字".repeat(MAX_TEXT + 1), "fact", "console", false)
            .is_err());
        assert!(s.add("x", "not-a-kind", "console", false).is_err());
        assert!(s.render_for_prompt().is_none(), "empty memory is no block");
        for i in 0..50 {
            s.add(&format!("第{i}条"), "fact", "console", i < 3)
                .unwrap();
        }
        let block = s.render_for_prompt().unwrap();
        let lines = block.lines().filter(|l| l.starts_with("- ")).count();
        assert!(lines <= PROMPT_MAX_ITEMS, "{lines}");
        assert!(block.chars().count() <= PROMPT_MAX_CHARS + 200, "bounded");
        assert!(block.contains("第0条"), "pinned are first: {block}");
    }
}
