pub(crate) const SQL: &str = "
ALTER TABLE sessions ADD COLUMN action_generation INTEGER NOT NULL DEFAULT 0;
CREATE TABLE openapi_execution(id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL);
INSERT INTO openapi_execution(id,revision) VALUES(1,0);
CREATE TABLE confirmations(
 id TEXT PRIMARY KEY, task_id TEXT NOT NULL, session_id TEXT NOT NULL,
 requester TEXT NOT NULL, actor_key TEXT NOT NULL, approver TEXT, boot_id TEXT NOT NULL,
 catalog_revision TEXT NOT NULL, operation_revision TEXT NOT NULL, request_digest TEXT NOT NULL, state TEXT NOT NULL, revision INTEGER NOT NULL,
 data_epoch INTEGER NOT NULL, session_generation INTEGER NOT NULL,
 created_ms INTEGER NOT NULL, expires_ms INTEGER NOT NULL,
 continuation_written INTEGER NOT NULL DEFAULT 0,
 action_json TEXT NOT NULL, private_payload TEXT
);
CREATE INDEX confirmations_task ON confirmations(task_id);
CREATE INDEX confirmations_expiry ON confirmations(state,expires_ms);
";
