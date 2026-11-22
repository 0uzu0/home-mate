//! Schema owned by Store's ordered migration list. Never initialize it here.

pub const MIGRATION_SQL: &str = r#"
CREATE TABLE config_head (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    revision INTEGER NOT NULL DEFAULT 0,
    data_epoch INTEGER NOT NULL DEFAULT 0,
    applying INTEGER NOT NULL DEFAULT 0 CHECK(applying IN (0, 1)),
    household_id TEXT NOT NULL DEFAULT '',
    node_id TEXT NOT NULL DEFAULT '',
    layers_json TEXT NOT NULL DEFAULT '{}',
    resolved_json TEXT NOT NULL DEFAULT '{}'
);
INSERT INTO config_head(id) VALUES (1);
CREATE TABLE config_plans (
    plan_id TEXT PRIMARY KEY, principal TEXT NOT NULL, data_epoch INTEGER NOT NULL,
    created_ms INTEGER NOT NULL, expires_ms INTEGER NOT NULL,
    plan_hash TEXT NOT NULL, expected_revision INTEGER NOT NULL,
    baseline_hash TEXT NOT NULL, package_json TEXT NOT NULL,
    before_json TEXT NOT NULL, after_json TEXT NOT NULL,
    layers_json TEXT NOT NULL, diff_json TEXT NOT NULL
);
CREATE TABLE config_applications (
    application_id TEXT NOT NULL, principal TEXT NOT NULL,
    data_epoch INTEGER NOT NULL, plan_hash TEXT NOT NULL,
    revision INTEGER NOT NULL, created_ms INTEGER NOT NULL,
    before_json TEXT NOT NULL, after_json TEXT NOT NULL,
    result_json TEXT NOT NULL,
    PRIMARY KEY(principal, application_id)
);
CREATE TABLE config_application_blocks (
    application_id TEXT NOT NULL, principal TEXT NOT NULL,
    block TEXT NOT NULL, state TEXT NOT NULL,
    PRIMARY KEY(principal, application_id, block)
);
CREATE TABLE config_outbox (
    application_id TEXT NOT NULL, principal TEXT NOT NULL,
    data_epoch INTEGER NOT NULL, payload_json TEXT NOT NULL,
    state TEXT NOT NULL,
    PRIMARY KEY(principal, application_id)
);
CREATE INDEX config_plans_expiry ON config_plans(expires_ms);
CREATE INDEX config_applications_time ON config_applications(created_ms DESC);
CREATE TRIGGER config_revision_kv_insert AFTER INSERT ON config_kv
WHEN NEW.key != 'api_tokens'
BEGIN UPDATE config_head SET revision = revision + 1 WHERE id = 1 AND applying = 0; END;
CREATE TRIGGER config_revision_kv_update AFTER UPDATE ON config_kv
WHEN (OLD.value IS NOT NEW.value OR OLD.key IS NOT NEW.key)
 AND NOT (OLD.key = 'api_tokens' AND NEW.key = 'api_tokens')
BEGIN UPDATE config_head SET revision = revision + 1 WHERE id = 1 AND applying = 0; END;
CREATE TRIGGER config_revision_kv_delete AFTER DELETE ON config_kv
WHEN OLD.key != 'api_tokens'
BEGIN UPDATE config_head SET revision = revision + 1 WHERE id = 1 AND applying = 0; END;
CREATE TRIGGER config_revision_persona_insert AFTER INSERT ON agent_md_revisions
BEGIN UPDATE config_head SET revision = revision + 1 WHERE id = 1 AND applying = 0; END;
CREATE TRIGGER config_revision_persona_update AFTER UPDATE ON agent_md_revisions
BEGIN UPDATE config_head SET revision = revision + 1 WHERE id = 1 AND applying = 0; END;
CREATE TRIGGER config_revision_persona_delete AFTER DELETE ON agent_md_revisions
BEGIN UPDATE config_head SET revision = revision + 1 WHERE id = 1 AND applying = 0; END;
CREATE TRIGGER config_revision_secret_insert AFTER INSERT ON secrets
BEGIN UPDATE config_head SET revision = revision + 1 WHERE id = 1 AND applying = 0; END;
CREATE TRIGGER config_revision_secret_update AFTER UPDATE ON secrets
WHEN OLD.value IS NOT NEW.value
BEGIN UPDATE config_head SET revision = revision + 1 WHERE id = 1 AND applying = 0; END;
CREATE TRIGGER config_revision_secret_delete AFTER DELETE ON secrets
BEGIN UPDATE config_head SET revision = revision + 1 WHERE id = 1 AND applying = 0; END;
"#;

pub const FLEET_MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS config_household_invitations (
    invitation_id TEXT PRIMARY KEY,
    data_epoch INTEGER NOT NULL,
    household_id TEXT NOT NULL,
    expected_node_id TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    membership_generation INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL,
    consumed_ms INTEGER,
    consumed_by_node_id TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS config_household_invitations_expiry ON config_household_invitations(expires_ms);
CREATE TABLE IF NOT EXISTS config_fleet_runs (
    run_id TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    data_epoch INTEGER NOT NULL,
    household_id TEXT NOT NULL,
    package_json TEXT NOT NULL,
    package_hash TEXT NOT NULL,
    state TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    error_json TEXT
);
CREATE INDEX IF NOT EXISTS config_fleet_runs_time ON config_fleet_runs(created_ms DESC);
CREATE TABLE IF NOT EXISTS config_fleet_targets (
    run_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    ord INTEGER NOT NULL,
    base_url TEXT NOT NULL,
    plan_id TEXT NOT NULL DEFAULT '',
    plan_hash TEXT NOT NULL DEFAULT '',
    expected_revision INTEGER NOT NULL DEFAULT 0,
    application_id TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL,
    result_json TEXT NOT NULL DEFAULT '{}',
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY(run_id, node_id)
);
"#;
