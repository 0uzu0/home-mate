use super::*;
use std::sync::Arc;

#[test]
fn v10_scope_migration_preserves_archive_and_rejects_unverified_candidates() {
    let dir = std::env::temp_dir().join(format!("luna-b01-migrate-{}", crate::id::next("test")));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("test.db");
    let mut conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_version(version INTEGER PRIMARY KEY,applied_ms INTEGER NOT NULL)",
    )
    .unwrap();
    for (i, sql) in MIGRATIONS.iter().take(10).enumerate() {
        Store::apply_migration(&mut conn, i as i64 + 1, sql).unwrap();
    }
    let now = crate::id::now_ms() as i64;
    conn.execute("INSERT INTO memories(id,text,created_ms,updated_ms) VALUES ('legacy-note','LEGACY_SHARED',?1,?1)",[now]).unwrap();
    conn.execute("INSERT INTO memory_candidates(id,text,device,created_ms,updated_ms) VALUES ('legacy-candidate','UNVERIFIED_CANDIDATE','B',?1,?1)",[now]).unwrap();
    conn.execute("INSERT INTO user_md_entries(id,person_id,key,content,source_device,turn_ts,created_ms,updated_ms) VALUES ('legacy-user','alice','tea','ALICE_RESTRICTED','B',?1,?1,?1)",[now]).unwrap();
    conn.execute(
        "INSERT INTO sessions(id,created_ms) VALUES ('dev:B',?1)",
        [now],
    )
    .unwrap();
    conn.execute("INSERT INTO messages(session_id,role,content,ts_ms) VALUES ('dev:B','user','LEGACY_PRIVATE_HISTORY',?1)",[now]).unwrap();
    drop(conn);
    let store = Arc::new(Store::open(&path).unwrap());
    let memory = crate::memory::MemoryService::new(store.clone());
    let note = memory.list().unwrap().pop().unwrap();
    assert_eq!((&*note.scope_person, &*note.scope_device), ("", ""));
    let candidate = memory.candidates(None).unwrap().pop().unwrap();
    assert_eq!(
        (&*candidate.status, &*candidate.reason),
        ("rejected", "legacy_scope_unverified")
    );
    assert!(memory.confirm_candidate(&candidate.id).is_err());
    let users = crate::user_md::UserMdService::new(store.clone());
    let user = users.list(None).unwrap().pop().unwrap();
    assert_eq!(
        (&*user.person_id, &*user.scope_device, &*user.source_device),
        ("alice", "", "B")
    );
    let anonymous = crate::execution::Authority::anonymous(store.clone(), None).unwrap();
    assert!(anonymous
        .bind_session("dev:B")
        .unwrap_err()
        .contains("SESSION_SCOPE_UNVERIFIED"));
    let sessions = crate::session::SessionStore::new(store.clone());
    let new = sessions.device_session_id("B").unwrap();
    assert_ne!(new, "dev:B");
    assert!(sessions.messages(&new).unwrap().is_empty());
    assert_eq!(
        sessions.messages("dev:B").unwrap()[0]["content"],
        "LEGACY_PRIVATE_HISTORY"
    );
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn conservative_headless_session_binding_survives_real_store_reopen() {
    let dir = std::env::temp_dir().join(format!("luna-b01-session-{}", crate::id::next("test")));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("test.db");
    let session = {
        let store = Arc::new(Store::open(&path).unwrap());
        let sessions = crate::session::SessionStore::new(store.clone());
        let session = sessions.create().unwrap();
        crate::execution::Authority::anonymous(store, None)
            .unwrap()
            .bind_session(&session)
            .unwrap();
        sessions
            .append(&session, "user", "HEADLESS_CONTINUITY")
            .unwrap();
        session
    };
    let store = Arc::new(Store::open(&path).unwrap());
    let authority = crate::execution::Authority::anonymous(store.clone(), None).unwrap();
    authority.bind_session(&session).unwrap();
    let sessions = crate::session::SessionStore::new(store.clone());
    assert_eq!(
        sessions.messages(&session).unwrap()[0]["content"],
        "HEADLESS_CONTINUITY"
    );
    let other = crate::execution::Authority::internal(
        store,
        crate::working_memory::ScopeContext {
            person_id: Some("alice".into()),
            device_id: None,
            room: None,
        },
    )
    .unwrap();
    assert!(other.bind_session(&session).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}
