//! SQLite storage: config, secrets, sessions, messages, tasks, traces.
//!
//! One bundled SQLite file under the data directory (default `./data/`,
//! gitignored). All timestamps are epoch milliseconds. Migrations are plain
//! sequential SQL in this file — no migration framework needed at this size.

use crate::vault::{is_sealed, SecretVault};
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// M12a-1: `data/secret.key` could not be created/loaded, so nothing may
    /// be written that claims to be encrypted. Reads of plaintext rows and of
    /// already-sealed rows (when the key is there) still work.
    #[error("secret vault unavailable: {0}")]
    Vault(String),
    #[error("execution authority: {0}")]
    Authority(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

const MIGRATIONS: &[&str] = &[
    // v1: initial schema
    "CREATE TABLE config_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
     CREATE TABLE secrets (name TEXT PRIMARY KEY, value TEXT NOT NULL);
     CREATE TABLE sessions (
        id TEXT PRIMARY KEY, created_ms INTEGER NOT NULL, title TEXT NOT NULL DEFAULT '');
     CREATE TABLE messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
        role TEXT NOT NULL, content TEXT NOT NULL, ts_ms INTEGER NOT NULL);
     CREATE INDEX idx_messages_session ON messages(session_id, id);
     CREATE TABLE tasks (
        id TEXT PRIMARY KEY, session_id TEXT NOT NULL, status TEXT NOT NULL,
        created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL, detail TEXT NOT NULL DEFAULT '{}');
     CREATE TABLE traces (
        id TEXT PRIMARY KEY, session_id TEXT NOT NULL, task_id TEXT NOT NULL,
        source TEXT NOT NULL, input_text TEXT NOT NULL, goal TEXT NOT NULL DEFAULT '',
        status TEXT NOT NULL, created_ms INTEGER NOT NULL, duration_ms INTEGER NOT NULL DEFAULT 0);
     CREATE TABLE trace_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT, trace_id TEXT NOT NULL, seq INTEGER NOT NULL,
        kind TEXT NOT NULL, payload TEXT NOT NULL DEFAULT '{}', ts_ms INTEGER NOT NULL);
     CREATE INDEX idx_trace_events ON trace_events(trace_id, id);
     CREATE TABLE config_history (
        id INTEGER PRIMARY KEY AUTOINCREMENT, ts_ms INTEGER NOT NULL,
        actor TEXT NOT NULL, scope TEXT NOT NULL, summary TEXT NOT NULL);",
    // v2 (M3): restorable snapshot per history row. Secrets are NEVER part
    // of it — only whether a key was set at the time.
    "ALTER TABLE config_history ADD COLUMN payload TEXT NOT NULL DEFAULT '{}';",
    // v3 (R1f): the structured wire message for replay. `content` stays the
    // human-readable line (console); `wire` is what the model sees again —
    // assistant tool_calls and role=tool results, never `[调用 …]` prose.
    "ALTER TABLE messages ADD COLUMN wire TEXT;",
    // v4 (2026-11-13): household memory (xiaozhi mem_local_short semantics).
    // Short, user-confirmed notes only; the console owns CRUD. Pinned first.
    "CREATE TABLE memories (
        id TEXT PRIMARY KEY, text TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'fact',
        source TEXT NOT NULL DEFAULT 'console', pinned INTEGER NOT NULL DEFAULT 0,
        created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL);
     CREATE INDEX idx_memories_time ON memories(updated_ms DESC);",
    // v5 (M7-1, 2026-11-13): auto-extracted memory CANDIDATES. The model may
    // propose after a session ends, but nothing is written until a second
    // background pass confirms it. `status` is pending|confirmed|rejected;
    // rows are kept so the console (and the audit trail) can see what was
    // proposed and why it was refused.
    "CREATE TABLE memory_candidates (
        id TEXT PRIMARY KEY, text TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'fact',
        status TEXT NOT NULL DEFAULT 'pending', device TEXT NOT NULL DEFAULT '',
        session_id TEXT NOT NULL DEFAULT '', trigger TEXT NOT NULL DEFAULT '',
        reason TEXT NOT NULL DEFAULT '', source TEXT NOT NULL DEFAULT 'auto',
        created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL);
     CREATE INDEX idx_memory_candidates ON memory_candidates(status, created_ms DESC);",
    // v6 (2026-11-14): reminders. One row per reminder, owned by the DEVICE
    // whose turn created it (M12a: the speaker that was told is the speaker
    // that reminds). `fired_ms`/`cancelled_ms` are NULL while it is pending;
    // rows are kept after firing so the console can show what was said.
    "CREATE TABLE reminders (
        id INTEGER PRIMARY KEY AUTOINCREMENT, device TEXT NOT NULL,
        due_ms INTEGER NOT NULL, note TEXT NOT NULL,
        created_ms INTEGER NOT NULL, fired_ms INTEGER, cancelled_ms INTEGER);
     CREATE INDEX idx_reminders_due ON reminders(fired_ms, cancelled_ms, due_ms);",
    // v7 (M6, #23): long-term working memory, completely separate from the
    // 200-item household notes (`memories`). Keyed by
    // (scope_person, scope_device, key_or_topic) so per-person / per-device
    // project paths, progress, and active context persist across sessions.
    "CREATE TABLE working_memory (
        id TEXT PRIMARY KEY,
        scope_person TEXT NOT NULL DEFAULT '',
        scope_device TEXT NOT NULL DEFAULT '',
        key_or_topic TEXT NOT NULL,
        content TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        UNIQUE(scope_person, scope_device, key_or_topic)
    );
     CREATE INDEX idx_working_memory_scope ON working_memory(scope_person, scope_device, updated_ms DESC);",
    // v8 (W3, #78/#79/#80): scoped working memory lifecycle (pinned, LRU/LFU
    // access tracking, provenance) + offline-resilient shared memory outbox
    // and cached profile summaries.
    "ALTER TABLE working_memory ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE working_memory ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE working_memory ADD COLUMN last_accessed_ms INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE working_memory ADD COLUMN archived_at_ms INTEGER;
     ALTER TABLE working_memory ADD COLUMN written_by_device TEXT NOT NULL DEFAULT '';
     ALTER TABLE working_memory ADD COLUMN source_trace_id TEXT NOT NULL DEFAULT '';
     CREATE INDEX idx_working_memory_evict ON working_memory(pinned ASC, access_count ASC, last_accessed_ms ASC, updated_ms ASC, id ASC);
     CREATE TABLE IF NOT EXISTS shared_memory_outbox (
        id TEXT PRIMARY KEY,
        device_id TEXT NOT NULL DEFAULT '',
        room TEXT NOT NULL DEFAULT '',
        person_id TEXT NOT NULL DEFAULT '',
        trace_id TEXT NOT NULL DEFAULT '',
        content TEXT NOT NULL,
        created_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_shared_memory_outbox_created ON shared_memory_outbox(created_ms ASC, id);
     CREATE TABLE IF NOT EXISTS shared_profile_cache (
        id TEXT PRIMARY KEY,
        scope_person TEXT NOT NULL DEFAULT '',
        scope_device TEXT NOT NULL DEFAULT '',
        summary TEXT NOT NULL,
        updated_ms INTEGER NOT NULL,
        fetched_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_shared_profile_cache_scope ON shared_profile_cache(scope_person, scope_device, updated_ms DESC);",
    // v9 (W4b, #83): versioned Agent.md revisions + auditable USER.md entries.
    "CREATE TABLE IF NOT EXISTS agent_md_revisions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        version INTEGER NOT NULL,
        content TEXT NOT NULL,
        author TEXT NOT NULL DEFAULT 'console',
        note TEXT NOT NULL DEFAULT '',
        created_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_agent_md_revisions_ver ON agent_md_revisions(version DESC, id DESC);
     CREATE TABLE IF NOT EXISTS user_md_entries (
        id TEXT PRIMARY KEY,
        person_id TEXT NOT NULL DEFAULT '',
        key TEXT NOT NULL,
        content TEXT NOT NULL,
        source_device TEXT NOT NULL DEFAULT '',
        trace_id TEXT NOT NULL DEFAULT '',
        turn_ts INTEGER NOT NULL,
        rationale TEXT NOT NULL DEFAULT '',
        author TEXT NOT NULL DEFAULT 'console',
        status TEXT NOT NULL DEFAULT 'active',
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        reverted_ms INTEGER,
        reverted_by TEXT NOT NULL DEFAULT ''
     );
     CREATE INDEX IF NOT EXISTS idx_user_md_entries_status ON user_md_entries(status, updated_ms DESC);",
    // v10 (Luna): reviewed configuration plans and durable application state.
    crate::config_delivery::migration::MIGRATION_SQL,
    // v11: explicit memory visibility and durable session authority.
    crate::execution::migration::SQL,
    // v12: durable physical context identity; archives retain their own messages.
    crate::session::CONTEXT_MIGRATION,
    // v13: bounded destructive-action journal and execution revisions.
    crate::confirmation::migration::SQL,
    // v14 (#107, B-04): separate source_device provenance from scope_device/scope_person
    // visibility and record v2 observation/profile metadata.
    "ALTER TABLE shared_memory_outbox ADD COLUMN source_device TEXT NOT NULL DEFAULT '';
     ALTER TABLE shared_memory_outbox ADD COLUMN scope_device TEXT NOT NULL DEFAULT '';
     ALTER TABLE shared_memory_outbox ADD COLUMN scope_person TEXT NOT NULL DEFAULT '';
     ALTER TABLE shared_memory_outbox ADD COLUMN domain TEXT NOT NULL DEFAULT 'notes';
     ALTER TABLE shared_memory_outbox ADD COLUMN record_id TEXT NOT NULL DEFAULT '';
     ALTER TABLE shared_memory_outbox ADD COLUMN event_kind TEXT NOT NULL DEFAULT 'explicit_save';
     ALTER TABLE shared_memory_outbox ADD COLUMN dimension TEXT NOT NULL DEFAULT 'general';
     UPDATE shared_memory_outbox SET source_device = device_id WHERE source_device = '' AND device_id != '';
     UPDATE shared_memory_outbox SET scope_person = person_id WHERE scope_person = '' AND person_id != '';
     ALTER TABLE shared_profile_cache ADD COLUMN source_device TEXT NOT NULL DEFAULT '';
     ALTER TABLE shared_profile_cache ADD COLUMN version INTEGER NOT NULL DEFAULT 1;
     ALTER TABLE shared_profile_cache ADD COLUMN row_revision INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE shared_profile_cache ADD COLUMN dimension TEXT NOT NULL DEFAULT 'general';",
    // v15 (#110): household invitations and durable sequential fleet rollout runs.
    crate::config_delivery::migration::FLEET_MIGRATION_SQL,
];

/// Thread-safe wrapper over a single connection. M1 traffic is light; a
/// mutex is honest and avoids cross-thread SQLite surprises.
pub struct Store {
    conn: Mutex<Connection>,
    /// Cancellation → catalog gate → this gate → Store; never across await.
    pub(crate) openapi_gate: Mutex<()>,
    /// M12a-1: at-rest encryption for the `secrets` table. `Err` carries why
    /// the key is unusable — writes then refuse instead of silently storing
    /// plaintext under a name that implies otherwise.
    vault: std::result::Result<SecretVault, String>,
    /// Test-only opt-in allowing loopback (`127.0.0.1` / `::1`) endpoints for
    /// public-service configs (weather, news, search, openapi) when tests run
    /// against an in-process mock server. Always `false` in production.
    allow_loopback_public: std::sync::atomic::AtomicBool,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // The key lives beside the database (`data/secret.key`), so a DB copy
        // alone does not carry a usable credential set.
        let data_dir = path.parent().unwrap_or(Path::new("."));
        let vault = SecretVault::load_or_create(data_dir).map_err(|e| {
            let msg = e.to_string();
            tracing::error!(error = %msg, "secret vault unavailable; stored secrets cannot be written");
            msg
        });
        let store = Self {
            conn: Mutex::new(conn),
            openapi_gate: Mutex::new(()),
            vault,
            allow_loopback_public: std::sync::atomic::AtomicBool::new(false),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
            openapi_gate: Mutex::new(()),
            vault: Ok(SecretVault::ephemeral()),
            allow_loopback_public: std::sync::atomic::AtomicBool::new(false),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Allow loopback (`127.0.0.1` / `::1`) URLs for public outbound services
    /// (weather, news, search, openapi) in tests using a local mock server.
    pub fn allow_loopback_public_for_test(&self) {
        self.allow_loopback_public
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn allows_loopback_public(&self) -> bool {
        self.allow_loopback_public
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = crate::lock_or_recover(&self.conn, "store.conn.migrate");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_ms INTEGER NOT NULL)",
        )?;
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let v = (i + 1) as i64;
            Self::apply_migration(&mut conn, v, sql)?;
        }
        Ok(())
    }

    fn apply_migration(conn: &mut Connection, version: i64, sql: &str) -> Result<()> {
        // Schema changes and their version stamp must survive or roll back
        // together, including a process exit before SQLite commits the WAL.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Another startup may have committed this version while we waited for
        // SQLite's write lock. Read the version only after acquiring that lock.
        let current: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )?;
        if current >= version {
            return Ok(());
        }
        let (repaired_sql, skipped) = Self::repair_partial_legacy_schema(&tx, version, sql)?;
        if !repaired_sql.trim().is_empty() {
            tx.execute_batch(&repaired_sql)?;
        }
        if !skipped.is_empty() {
            tracing::warn!(
                version,
                repaired = ?skipped,
                "detected and repaired preexisting half-applied legacy schema"
            );
            let report = serde_json::json!({
                "version": version,
                "repaired_items": skipped,
                "repaired_ms": crate::id::now_ms(),
            });
            let _ = tx.execute(
                "INSERT INTO config_kv (key, value) VALUES ('db.migration_recovery', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [report.to_string()],
            );
        }
        tx.execute(
            "INSERT INTO schema_version (version, applied_ms) VALUES (?1, ?2)",
            rusqlite::params![version, crate::id::now_ms() as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Detect and skip DDL statements (`ALTER TABLE ... ADD COLUMN ...` or
    /// `CREATE INDEX <idx> ON ...`) that were already committed by a pre-PR#95
    /// non-transactional migration before crashing (`#104`). Genuine SQL errors
    /// on unapplied statements still execute and roll back the transaction.
    fn repair_partial_legacy_schema(
        tx: &rusqlite::Transaction<'_>,
        version: i64,
        sql: &str,
    ) -> Result<(String, Vec<String>)> {
        let mut kept = String::with_capacity(sql.len());
        let mut skipped = Vec::new();
        let mut start = 0usize;
        for (idx, _) in sql.match_indices(';') {
            let end = idx + 1;
            let chunk = &sql[start..end];
            let complete = std::ffi::CString::new(chunk)
                .ok()
                .is_some_and(|c| unsafe { rusqlite::ffi::sqlite3_complete(c.as_ptr()) } == 1);
            if !complete {
                continue;
            }
            start = end;
            let trimmed = chunk.trim();
            if let Some((table, col)) = Self::parse_alter_add_column(trimmed) {
                if Self::table_has_column(tx, table, col)? {
                    skipped.push(format!("v{version}:{table}.{col}"));
                    continue;
                }
            } else if let Some(index_name) = Self::parse_create_index_without_if_not_exists(trimmed)
            {
                if Self::index_exists(tx, index_name)? {
                    skipped.push(format!("v{version}:index.{index_name}"));
                    continue;
                }
            }
            kept.push_str(chunk);
        }
        if start < sql.len() {
            kept.push_str(&sql[start..]);
        }
        Ok((kept, skipped))
    }

    fn parse_alter_add_column(stmt: &str) -> Option<(&str, &str)> {
        let tokens: Vec<&str> = stmt.split_whitespace().collect();
        if tokens.len() >= 6
            && tokens[0].eq_ignore_ascii_case("ALTER")
            && tokens[1].eq_ignore_ascii_case("TABLE")
            && tokens[3].eq_ignore_ascii_case("ADD")
            && tokens[4].eq_ignore_ascii_case("COLUMN")
        {
            let table = tokens[2].trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            let col = tokens[5].trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            if !table.is_empty() && !col.is_empty() {
                return Some((table, col));
            }
        }
        None
    }

    fn parse_create_index_without_if_not_exists(stmt: &str) -> Option<&str> {
        let tokens: Vec<&str> = stmt.split_whitespace().collect();
        if tokens.len() >= 5
            && tokens[0].eq_ignore_ascii_case("CREATE")
            && tokens[1].eq_ignore_ascii_case("INDEX")
            && !tokens[2].eq_ignore_ascii_case("IF")
            && tokens[3].eq_ignore_ascii_case("ON")
        {
            let idx = tokens[2].trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            if !idx.is_empty() {
                return Some(idx);
            }
        }
        None
    }

    fn table_has_column(tx: &rusqlite::Transaction<'_>, table: &str, col: &str) -> Result<bool> {
        let pragma = format!("PRAGMA table_info({table})");
        let mut stmt = tx.prepare(&pragma)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name.eq_ignore_ascii_case(col) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn index_exists(tx: &rusqlite::Transaction<'_>, index_name: &str) -> Result<bool> {
        let exists: i64 = tx.query_row(
            "SELECT COUNT(1) FROM sqlite_master WHERE type = 'index' AND name = ?1",
            [index_name],
            |r| r.get(0),
        )?;
        Ok(exists > 0)
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = crate::lock_or_recover(&self.conn, "store.conn.with");
        f(&conn)
    }

    /// Highest applied migration (M8a: release rollback compares this with the
    /// schema a snapshot was taken at).
    pub fn schema_version(&self) -> Result<i64> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |r| r.get(0),
            )?)
        })
    }

    /// Consistent snapshot for release activation/rollback: checkpoint the WAL
    /// (so the copy includes committed frames), then `VACUUM INTO` (safe while
    /// this connection is live; the target must not exist). Returns the schema
    /// the snapshot was taken at.
    pub fn snapshot_to(&self, path: &Path) -> Result<i64> {
        let schema = self.schema_version()?;
        let p = path.to_string_lossy().to_string();
        self.with(|c| {
            let (_busy, _frames, _done): (i64, i64, i64) =
                c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
            let _ = std::fs::remove_file(path);
            c.execute("VACUUM INTO ?1", [&p])?;
            Ok(())
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(schema)
    }

    /// Retention + WAL maintenance (2026-11-13, roadmap §10): the database had
    /// no DELETE/VACUUM and a 4 MB WAL that grew week over week. Runs at
    /// startup and every few hours: rows older than `retention_days` go
    /// (messages, their sessions, tasks, traces and trace events), then the
    /// WAL is checkpointed and truncated so the file ceiling is bounded by the
    /// window, not by uptime. `retention_days == 0` keeps everything but still
    /// checkpoints. The report is persisted under `db.maintenance` and shown in
    /// `/api/v0/status`, so the applied window is observable, not a promise.
    pub fn maintain(&self, retention_days: u64) -> Result<serde_json::Value> {
        let now = crate::id::now_ms();
        let cutoff = now.saturating_sub(retention_days.saturating_mul(86_400_000));
        let mut out = serde_json::json!({
            "retention_days": retention_days,
            "ran_ms": now,
            "cutoff_ms": cutoff,
            "messages": 0, "sessions": 0, "tasks": 0, "traces": 0, "trace_events": 0,
            "wal_busy": 0, "wal_frames": 0, "wal_checkpointed": 0,
        });
        self.with(|c| {
            if retention_days > 0 {
                let cutoff_i64 = cutoff as i64;
                let messages = c.execute("DELETE FROM messages WHERE ts_ms < ?1", [cutoff_i64])?;
                let trace_events = c.execute(
                    "DELETE FROM trace_events WHERE trace_id IN (SELECT id FROM traces WHERE created_ms < ?1)",
                    [cutoff_i64],
                )?;
                let traces = c.execute("DELETE FROM traces WHERE created_ms < ?1", [cutoff_i64])?;
                let tasks = c.execute("DELETE FROM tasks WHERE updated_ms < ?1", [cutoff_i64])?;
                // A session goes only when it has no message left inside the
                // window (deleting the parent first would orphan them).
                let sessions = c.execute(
                    "DELETE FROM sessions WHERE created_ms < ?1
                     AND id NOT IN (SELECT DISTINCT session_id FROM messages)",
                    [cutoff_i64],
                )?;
                out["messages"] = messages.into();
                out["sessions"] = sessions.into();
                out["tasks"] = tasks.into();
                out["traces"] = traces.into();
                out["trace_events"] = trace_events.into();
            }
            // (busy, log frames, checkpointed frames). TRUNCATE is what keeps
            // the -wal file from staying at its high-water mark.
            let (busy, frames, done): (i64, i64, i64) =
                c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
            out["wal_busy"] = busy.into();
            out["wal_frames"] = frames.into();
            out["wal_checkpointed"] = done.into();
            Ok(())
        })?;
        Ok(out)
    }

    /// Clear user configuration, metadata, and non-secret runtime state.
    /// `secrets` is preserved unless `include_secrets` is explicitly true.
    pub fn factory_reset(&self, include_secrets: bool) -> Result<serde_json::Value> {
        self.with(|c| {
            let transaction = c.unchecked_transaction()?;
            let c = &transaction;
            // A pre-reset plan or rollback must never restore cleared data.
            let config_plans = c.execute("DELETE FROM config_plans", [])?;
            let config_applications = c.execute("DELETE FROM config_applications", [])?;
            c.execute("DELETE FROM config_application_blocks", [])?;
            c.execute("DELETE FROM config_outbox", [])?;
            c.execute("DELETE FROM config_household_invitations", [])?;
            c.execute("DELETE FROM config_fleet_targets", [])?;
            c.execute("DELETE FROM config_fleet_runs", [])?;
            c.execute("UPDATE config_head SET data_epoch=data_epoch+1, revision=revision+1, applying=0, household_id='', node_id='', layers_json='{}', resolved_json='{}' WHERE id=1", [])?;
            c.execute("DELETE FROM confirmations", [])?;
            c.execute("UPDATE openapi_execution SET revision=revision+1 WHERE id=1", [])?;
            let config_kv = c.execute("DELETE FROM config_kv", [])?;
            let memories = c.execute("DELETE FROM memories", [])?;
            let memory_candidates = c.execute("DELETE FROM memory_candidates", [])?;
            let working_memory = c.execute("DELETE FROM working_memory", [])?;
            let agent_md_revisions = c.execute("DELETE FROM agent_md_revisions", [])?;
            let user_md_entries = c.execute("DELETE FROM user_md_entries", [])?;
            let shared_profile_cache = c.execute("DELETE FROM shared_profile_cache", [])?;
            let shared_memory_outbox = c.execute("DELETE FROM shared_memory_outbox", [])?;
            let config_history = c.execute("DELETE FROM config_history", [])?;
            let reminders = c.execute("DELETE FROM reminders", [])?;
            let device_contexts = c.execute("DELETE FROM device_contexts", [])?;
            let messages = c.execute("DELETE FROM messages", [])?;
            let sessions = c.execute("DELETE FROM sessions", [])?;
            c.execute("DELETE FROM execution_sessions", [])?;
            let tasks = c.execute("DELETE FROM tasks", [])?;
            let trace_events = c.execute("DELETE FROM trace_events", [])?;
            let traces = c.execute("DELETE FROM traces", [])?;
            let secrets = if include_secrets {
                c.execute("DELETE FROM secrets", [])?
            } else {
                0
            };
            transaction.commit()?;
            Ok(serde_json::json!({
                "config_plans": config_plans,
                "config_applications": config_applications,
                "config_kv": config_kv,
                "memories": memories,
                "memory_candidates": memory_candidates,
                "working_memory": working_memory,
                "agent_md_revisions": agent_md_revisions,
                "user_md_entries": user_md_entries,
                "shared_profile_cache": shared_profile_cache,
                "shared_memory_outbox": shared_memory_outbox,
                "config_history": config_history,
                "reminders": reminders,
                "device_contexts": device_contexts,
                "messages": messages,
                "sessions": sessions,
                "tasks": tasks,
                "trace_events": trace_events,
                "traces": traces,
                "secrets": secrets,
            }))
        })
    }
}

// --- config / secrets ------------------------------------------------------

impl Store {
    pub fn config_get(&self, key: &str) -> Result<Option<String>> {
        self.with(|c| {
            use rusqlite::OptionalExtension;
            Ok(
                c.query_row("SELECT value FROM config_kv WHERE key=?1", [key], |r| {
                    r.get(0)
                })
                .optional()?,
            )
        })
    }

    pub fn config_set(&self, key: &str, value: &str) -> Result<()> {
        self.with(|c| {
            c.execute("INSERT INTO config_kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=?2",
                rusqlite::params![key, value])?;
            Ok(())
        })
    }

    pub fn config_delete(&self, key: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM config_kv WHERE key = ?1",
                rusqlite::params![key],
            )?;
            Ok(())
        })
    }

    /// Seal + write. Every secret the household configures goes through here,
    /// so "the row starts with `enc:v1:`" is a property of the table, not of
    /// each caller remembering to encrypt.
    pub fn secret_set(&self, name: &str, value: &str) -> Result<()> {
        let sealed = self.seal_secret(name, value)?;
        self.with(|c| {
            c.execute("INSERT INTO secrets (name, value) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET value=?2",
                rusqlite::params![name, sealed])?;
            Ok(())
        })
    }

    pub(crate) fn seal_secret(&self, name: &str, value: &str) -> Result<String> {
        self.vault
            .as_ref()
            .map_err(|e| StoreError::Vault(e.clone()))?
            .seal(name, value)
            .map_err(|e| StoreError::Vault(e.to_string()))
    }

    /// Decrypt on read. A pre-v0.26 plaintext row is returned unchanged; a
    /// sealed row that cannot be opened is counted and reported as "no secret"
    /// — a missing key must not take the config service (or a boot) down, but
    /// it must not pretend either, so `vault_status()` surfaces the count.
    pub fn secret_get(&self, name: &str) -> Result<Option<String>> {
        self.with(|c| self.secret_get_on(c, name))
    }

    pub(crate) fn secret_get_on(&self, c: &Connection, name: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let raw = c
            .query_row("SELECT value FROM secrets WHERE name=?1", [name], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        let Some(stored) = raw else { return Ok(None) };
        match self.vault.as_ref() {
            Ok(vault) => match vault.open(name, &stored) {
                Ok(v) => Ok(Some(v)),
                Err(e) => {
                    vault.note_undecryptable();
                    tracing::warn!(secret = name, error = %e, "stored secret could not be decrypted");
                    Ok(None)
                }
            },
            Err(reason) => {
                if is_sealed(&stored) {
                    tracing::warn!(secret = name, error = %reason, "stored secret is sealed but the vault is unavailable");
                    Ok(None)
                } else {
                    // Legacy plaintext stays readable even without a key, so a
                    // device whose key file was lost can still be repaired.
                    Ok(Some(stored))
                }
            }
        }
    }

    /// The stored row exactly as SQLite holds it (prefix check, diagnostics,
    /// tests). Never returned by the management API.
    pub fn secret_raw(&self, name: &str) -> Result<Option<String>> {
        self.with(|c| {
            use rusqlite::OptionalExtension;
            Ok(
                c.query_row("SELECT value FROM secrets WHERE name=?1", [name], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?,
            )
        })
    }

    /// Names of every stored secret (no values) — the backfill pass and the
    /// diagnostics card use it.
    pub fn secret_names(&self) -> Result<Vec<String>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT name FROM secrets ORDER BY name")?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// What the console/`/status` may know about at-rest encryption: how many
    /// rows are sealed, how many could not be opened, and whether the key is
    /// usable. Never the key, never a value.
    pub fn vault_status(&self) -> serde_json::Value {
        let names = self.secret_names().unwrap_or_default();
        let mut sealed = 0u64;
        let mut plaintext = 0u64;
        for name in &names {
            match self.secret_raw(name) {
                Ok(Some(v)) if is_sealed(&v) => sealed += 1,
                Ok(Some(_)) => plaintext += 1,
                _ => {}
            }
        }
        let (ok, undecryptable, key_path) = match self.vault.as_ref() {
            Ok(v) => (
                true,
                v.undecryptable(),
                v.key_path().map(|p| p.display().to_string()),
            ),
            Err(_) => (false, 0, None),
        };
        serde_json::json!({
            "ok": ok,
            "sealed": sealed,
            "plaintext": plaintext,
            "undecryptable": undecryptable,
            "key_path": key_path,
        })
    }

    /// Backfill (M12a-1, startup): seal every row that is still plaintext.
    /// The values were put there by older code paths; re-writing them here is
    /// what makes a device's existing credentials exportable and pushable.
    pub fn seal_plaintext_rows(&self) -> Result<Vec<String>> {
        let mut done = Vec::new();
        for name in self.secret_names()? {
            if let Ok(Some(raw)) = self.secret_raw(&name) {
                if !is_sealed(&raw) {
                    // `secret_get` returns the plaintext unchanged, then
                    // `secret_set` re-seals it.
                    if let Ok(Some(value)) = self.secret_get(&name) {
                        self.secret_set(&name, &value)?;
                        done.push(name);
                    }
                }
            }
        }
        Ok(done)
    }

    /// Remove a stored secret (the write-only counterpart of an empty
    /// `secret` field: "I want to clear this").
    pub fn secret_delete(&self, name: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM secrets WHERE name=?1", [name])?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod execution_tests;
#[cfg(test)]
mod migration_tests;

mod provider_config;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_keeps_the_window_and_checkpoints_the_wal() {
        let s = Store::open_in_memory().unwrap();
        let now = crate::id::now_ms() as i64;
        let old = (crate::id::now_ms().saturating_sub(30 * 86_400_000)) as i64;
        s.with(|c| {
            c.execute("INSERT INTO sessions (id, created_ms, title) VALUES ('s1', ?1, 'old')", [old])?;
            c.execute(
                "INSERT INTO messages (session_id, role, content, ts_ms) VALUES ('s1','user','old',?1)",
                [old],
            )?;
            c.execute(
                "INSERT INTO messages (session_id, role, content, ts_ms) VALUES ('s1','user','new',?1)",
                [now],
            )?;
            c.execute(
                "INSERT INTO traces (id, session_id, task_id, source, input_text, status, created_ms)
                 VALUES ('t1','s1','k','text','x','ok',?1)",
                [old],
            )?;
            c.execute(
                "INSERT INTO trace_events (trace_id, seq, kind, ts_ms) VALUES ('t1',0,'e',?1)",
                [old],
            )?;
            c.execute(
                "INSERT INTO tasks (id, session_id, status, created_ms, updated_ms) VALUES ('k','s1','done',?1,?1)",
                [old],
            )?;
            Ok(())
        })
        .unwrap();
        let report = s.maintain(14).unwrap();
        assert_eq!(report["retention_days"], 14);
        assert_eq!(report["messages"], 1, "{report}");
        assert_eq!(report["traces"], 1, "{report}");
        assert_eq!(report["trace_events"], 1, "{report}");
        assert_eq!(report["tasks"], 1, "{report}");
        // The session has a message inside the window, so it must survive.
        assert_eq!(report["sessions"], 0, "{report}");
        let left: i64 = s
            .with(|c| Ok(c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(left, 1, "only the row outside the window went");
        // A zero window keeps everything but still checkpoints.
        let kept = s.maintain(0).unwrap();
        assert_eq!(kept["messages"], 0);
        assert!(kept["wal_frames"].is_number());
    }

    #[test]
    fn migrations_run_and_config_roundtrips() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.config_get("services").unwrap().is_none());
        s.config_set("services", "{}").unwrap();
        assert_eq!(s.config_get("services").unwrap().as_deref(), Some("{}"));
        s.secret_set("llm.api_key", "x").unwrap();
        assert_eq!(s.secret_get("llm.api_key").unwrap().as_deref(), Some("x"));
    }

    #[test]
    fn secrets_are_sealed_at_rest_but_read_back_transparently() {
        let s = Store::open_in_memory().unwrap();
        s.secret_set("llm.api_key", "sk-live-value").unwrap();
        let raw = s.secret_raw("llm.api_key").unwrap().unwrap();
        assert!(raw.starts_with(crate::vault::SEALED_PREFIX), "{raw}");
        assert!(!raw.contains("sk-live-value"));
        assert_eq!(
            s.secret_get("llm.api_key").unwrap().as_deref(),
            Some("sk-live-value")
        );
        let status = s.vault_status();
        assert_eq!(status["sealed"], 1, "{status}");
        assert_eq!(status["plaintext"], 0, "{status}");
        assert_eq!(status["ok"], true, "{status}");
    }

    /// Rows written before v0.26 are plaintext; they must stay readable and
    /// must be sealed by the one-shot backfill, not by a background rewrite.
    #[test]
    fn legacy_plaintext_rows_read_and_the_backfill_seals_them() {
        let s = Store::open_in_memory().unwrap();
        s.with(|c| {
            c.execute(
                "INSERT INTO secrets (name, value) VALUES ('llm.api_key', 'legacy-plain')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            s.secret_get("llm.api_key").unwrap().as_deref(),
            Some("legacy-plain")
        );
        assert_eq!(s.vault_status()["plaintext"], 1);
        let sealed = s.seal_plaintext_rows().unwrap();
        assert_eq!(sealed, vec!["llm.api_key".to_string()]);
        assert!(s
            .secret_raw("llm.api_key")
            .unwrap()
            .unwrap()
            .starts_with(crate::vault::SEALED_PREFIX));
        assert_eq!(
            s.secret_get("llm.api_key").unwrap().as_deref(),
            Some("legacy-plain")
        );
        // Idempotent: a second pass finds nothing left to do.
        assert!(s.seal_plaintext_rows().unwrap().is_empty());
    }

    #[test]
    fn a_secret_survives_reopening_the_same_data_dir() {
        let dir = std::env::temp_dir().join(format!(
            "hm-store-vault-{}-{}",
            std::process::id(),
            crate::id::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let s = Store::open(&dir.join("home-mate.db")).unwrap();
            s.secret_set("tts.api_key", "sk-restart").unwrap();
        }
        {
            let s = Store::open(&dir.join("home-mate.db")).unwrap();
            assert_eq!(
                s.secret_get("tts.api_key").unwrap().as_deref(),
                Some("sk-restart"),
                "the key file is stable, so a restart keeps the credentials"
            );
            assert_eq!(s.vault_status()["ok"], true);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

mod openapi_config;
