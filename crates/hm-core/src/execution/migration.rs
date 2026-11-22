pub(crate) const SQL: &str = "
CREATE TABLE execution_sessions(session_id TEXT PRIMARY KEY, authority TEXT NOT NULL, data_epoch INTEGER NOT NULL);
ALTER TABLE memories ADD COLUMN scope_person TEXT NOT NULL DEFAULT '';
ALTER TABLE memories ADD COLUMN scope_device TEXT NOT NULL DEFAULT '';
ALTER TABLE memory_candidates ADD COLUMN scope_person TEXT NOT NULL DEFAULT '';
ALTER TABLE memory_candidates ADD COLUMN scope_device TEXT NOT NULL DEFAULT '';
ALTER TABLE memory_candidates ADD COLUMN data_epoch INTEGER NOT NULL DEFAULT -1;
UPDATE memory_candidates SET status='rejected', reason='legacy_scope_unverified' WHERE status='pending';
ALTER TABLE user_md_entries ADD COLUMN scope_device TEXT NOT NULL DEFAULT '';
";
