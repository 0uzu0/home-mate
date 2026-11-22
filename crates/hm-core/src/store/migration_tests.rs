//! File-backed migration rollback and real Store::open retry coverage.

use super::*;
use std::ffi::CString;

struct Fixture {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl Fixture {
    fn at_version(version: usize) -> Self {
        let dir = std::env::temp_dir().join(format!("hm-migration-{}", crate::id::next("test")));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("home-mate.db");
        let mut conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_ms INTEGER NOT NULL)",
        ).unwrap();
        for (i, sql) in MIGRATIONS.iter().take(version).enumerate() {
            Store::apply_migration(&mut conn, (i + 1) as i64, sql).unwrap();
        }
        if version > 0 {
            conn.execute(
                "INSERT INTO config_kv VALUES ('synthetic', 'preserved')",
                [],
            )
            .unwrap();
        }
        Self { dir, path }
    }

    fn assert_retry(&self, old_version: usize, expected_schema: &[(String, String)]) {
        let conn = Connection::open(&self.path).unwrap();
        let version: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version, old_version as i64);
        assert_eq!(schema(&conn), expected_schema);
        drop(conn);
        // Use the actual startup entry rather than only retrying the SQL helper.
        let store = Store::open(&self.path).unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
        if old_version > 0 {
            assert_eq!(
                store.config_get("synthetic").unwrap().as_deref(),
                Some("preserved")
            );
        }
        drop(store);
        assert_eq!(
            Store::open(&self.path).unwrap().schema_version().unwrap(),
            MIGRATIONS.len() as i64
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn schema(conn: &Connection) -> Vec<(String, String)> {
    conn.prepare("SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn statement_boundaries(sql: &str) -> Vec<usize> {
    let mut boundaries = vec![0];
    let mut start = 0;
    for (index, _) in sql.match_indices(';') {
        let end = index + 1;
        let candidate = CString::new(&sql[start..end]).expect("migration SQL contains no NUL");
        // SAFETY: candidate owns a valid NUL-terminated string for this entire
        // call. sqlite3_complete only reads it and does not retain the pointer.
        if unsafe { rusqlite::ffi::sqlite3_complete(candidate.as_ptr()) } == 1 {
            boundaries.push(end);
            start = end;
        }
    }
    assert!(
        sql[start..].trim().is_empty(),
        "migration ends with incomplete SQL"
    );
    boundaries
}

#[test]
fn sqlite_boundaries_keep_trigger_and_quoted_semicolons_inside_one_statement() {
    let sql = "CREATE TABLE source(value TEXT);
        CREATE TABLE audit(value TEXT);
        CREATE TRIGGER record_insert AFTER INSERT ON source BEGIN
            INSERT INTO audit VALUES ('value; embedded');
            UPDATE audit SET value = value || ';tail';
        END;
        INSERT INTO source VALUES ('trigger; input');";
    let boundaries = statement_boundaries(sql);
    let statements: Vec<&str> = boundaries
        .windows(2)
        .map(|pair| sql[pair[0]..pair[1]].trim())
        .collect();
    assert_eq!(statements.len(), 4);
    assert!(statements[2].starts_with("CREATE TRIGGER"));
    assert!(statements[2].ends_with("END;"));
    assert!(statements[2].contains("INSERT INTO audit VALUES ('value; embedded');"));
    assert!(statements[2].contains("UPDATE audit SET value = value || ';tail';"));
    let conn = Connection::open_in_memory().unwrap();
    for statement in statements {
        conn.execute_batch(statement).unwrap();
    }
    let actual: String = conn
        .query_row("SELECT value FROM audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(actual, "value; embedded;tail");
}

#[test]
fn migration_errors_at_every_statement_boundary_roll_back_and_startup_retries() {
    // SQLite determines boundaries, including complete trigger bodies. Never
    // inject into an unfinished statement and mistake its syntax error for the fault.
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        for (boundary, end) in statement_boundaries(sql).into_iter().enumerate() {
            let fixture = Fixture::at_version(i);
            let mut conn = Connection::open(&fixture.path).unwrap();
            let before = schema(&conn);
            let interrupted = format!(
                "{}\nSELECT * FROM injected_missing_migration_table;",
                &sql[..end]
            );
            let error = Store::apply_migration(&mut conn, (i + 1) as i64, &interrupted)
                .expect_err("injected statement must fail");
            assert!(
                matches!(&error,
                StoreError::Sql(rusqlite::Error::SqliteFailure(_, Some(message)))
                    if message == "no such table: injected_missing_migration_table"),
                "migration {} boundary {boundary}: unexpected failure: {error}",
                i + 1
            );
            drop(conn);
            fixture.assert_retry(i, &before);
        }
    }
}

#[test]
fn version_stamp_failure_rolls_back_schema_and_startup_retries() {
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        for timing in ["BEFORE", "AFTER"] {
            let fixture = Fixture::at_version(i);
            let mut conn = Connection::open(&fixture.path).unwrap();
            let before = schema(&conn);
            conn.execute_batch(&format!(
                "CREATE TEMP TRIGGER reject_version {timing} INSERT ON schema_version
            BEGIN SELECT RAISE(FAIL, 'injected version failure'); END;"
            ))
            .unwrap();
            assert!(Store::apply_migration(&mut conn, (i + 1) as i64, sql).is_err());
            drop(conn);
            fixture.assert_retry(i, &before);
        }
    }
}

#[test]
fn migration_version_query_errors_are_not_treated_as_a_fresh_database() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE schema_version (wrong_column INTEGER)")
        .unwrap();
    assert!(Store::apply_migration(&mut conn, 1, MIGRATIONS[0]).is_err());
    assert_eq!(
        schema(&conn).len(),
        1,
        "no schema SQL may run after a version query error"
    );
}

#[test]
fn already_committed_migration_is_rechecked_under_the_write_lock() {
    let fixture = Fixture::at_version(7);
    let mut first = Connection::open(&fixture.path).unwrap();
    let mut second = Connection::open(&fixture.path).unwrap();
    Store::apply_migration(&mut first, 8, MIGRATIONS[7]).unwrap();
    // The second opener's prior state was v7; its retry must not rerun ALTER.
    Store::apply_migration(&mut second, 8, MIGRATIONS[7]).unwrap();
    drop(first);
    drop(second);
    assert_eq!(
        Store::open(&fixture.path)
            .unwrap()
            .schema_version()
            .unwrap(),
        MIGRATIONS.len() as i64
    );
}

#[test]
fn uncommitted_schema_and_version_are_rolled_back_on_connection_close() {
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let fixture = Fixture::at_version(i);
        let conn = Connection::open(&fixture.path).unwrap();
        let before = schema(&conn);
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        conn.execute_batch(sql).unwrap();
        conn.execute(
            "INSERT INTO schema_version VALUES (?1, 0)",
            [(i + 1) as i64],
        )
        .unwrap();
        // Close with no COMMIT/ROLLBACK, after both DDL and the version stamp.
        drop(conn);
        fixture.assert_retry(i, &before);
    }
}

#[test]
fn preexisting_half_applied_legacy_v8_and_v14_schemas_are_diagnosed_and_recovered_by_store_open() {
    // 1. Simulate a pre-PR#95 crash midway through v8 (schema_version == 7, but
    //    `pinned`, `access_count`, and `idx_working_memory_evict` were already committed).
    let fixture = Fixture::at_version(7);
    {
        let conn = Connection::open(&fixture.path).unwrap();
        conn.execute_batch(
            "ALTER TABLE working_memory ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE working_memory ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE working_memory ADD COLUMN last_accessed_ms INTEGER NOT NULL DEFAULT 0;
             CREATE INDEX idx_working_memory_evict ON working_memory(pinned ASC, access_count ASC, last_accessed_ms ASC, updated_ms ASC, id ASC);",
        )
        .unwrap();
        // Also simulate a partial v14 column once v8 creates shared_memory_outbox:
        // we first verify Store::open repairs v8 and upgrades all the way to current schema.
    }
    let store = Store::open(&fixture.path).unwrap();
    assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
    assert_eq!(
        store.config_get("synthetic").unwrap().as_deref(),
        Some("preserved")
    );
    let diag_v8: serde_json::Value =
        serde_json::from_str(&store.config_get("db.migration_recovery").unwrap().unwrap()).unwrap();
    assert_eq!(diag_v8["version"], 8);
    let items_v8 = diag_v8["repaired_items"].as_array().unwrap();
    assert!(items_v8.iter().any(|v| v == "v8:working_memory.pinned"));
    assert!(items_v8
        .iter()
        .any(|v| v == "v8:index.idx_working_memory_evict"));
    drop(store);

    // 2. Simulate a partial v14 legacy schema at schema_version == 13.
    let fixture13 = Fixture::at_version(13);
    {
        let conn = Connection::open(&fixture13.path).unwrap();
        conn.execute_batch(
            "ALTER TABLE shared_memory_outbox ADD COLUMN source_device TEXT NOT NULL DEFAULT '';
             ALTER TABLE shared_profile_cache ADD COLUMN source_device TEXT NOT NULL DEFAULT '';",
        )
        .unwrap();
    }
    let store13 = Store::open(&fixture13.path).unwrap();
    assert_eq!(store13.schema_version().unwrap(), MIGRATIONS.len() as i64);
    let diag_v14: serde_json::Value = serde_json::from_str(
        &store13
            .config_get("db.migration_recovery")
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(diag_v14["version"], 14);
    let items_v14 = diag_v14["repaired_items"].as_array().unwrap();
    assert!(items_v14
        .iter()
        .any(|v| v == "v14:shared_memory_outbox.source_device"));
    assert!(items_v14
        .iter()
        .any(|v| v == "v14:shared_profile_cache.source_device"));
}
