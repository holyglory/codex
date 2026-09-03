-- no-transaction
-- SQLite cannot alter a column CHECK constraint in place. Disable foreign-key enforcement only
-- on this migration connection, rebuild the two constrained parent tables in one immediate
-- transaction, verify the complete graph, and re-enable enforcement before returning to SQLx.
-- Rebuilding an already-upgraded table is intentionally safe so a crash after COMMIT but before
-- SQLx records the migration can retry without changing facts.
PRAGMA foreign_keys = OFF;
BEGIN IMMEDIATE;

DROP TABLE IF EXISTS turns_v4;
CREATE TABLE turns_v4 (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    account_profile_ref TEXT,
    created_at_ms INTEGER NOT NULL,
    account_auth_mode TEXT CHECK (
        account_auth_mode IS NULL OR account_auth_mode IN (
            'api_key', 'chatgpt', 'chatgpt_auth_tokens', 'headers',
            'agent_identity', 'personal_access_token', 'bedrock_api_key',
            'bedrock_access_keys'
        )
    ),
    UNIQUE(id, thread_id)
) STRICT;
INSERT INTO turns_v4(
    id, thread_id, account_profile_ref, created_at_ms, account_auth_mode
)
SELECT id, thread_id, account_profile_ref, created_at_ms, account_auth_mode FROM turns;
DROP TABLE turns;
ALTER TABLE turns_v4 RENAME TO turns;
CREATE TRIGGER turns_no_update BEFORE UPDATE ON turns
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER turns_no_delete BEFORE DELETE ON turns
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;

DROP TABLE IF EXISTS model_requests_v4;
CREATE TABLE model_requests_v4 (
    id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL UNIQUE,
    operation_kind TEXT NOT NULL DEFAULT 'model_request'
        CHECK (operation_kind = 'model_request'),
    provider_kind TEXT NOT NULL,
    model TEXT NOT NULL,
    transport_kind TEXT NOT NULL,
    attempt_number INTEGER NOT NULL CHECK (attempt_number > 0),
    account_profile_ref TEXT,
    client_origin TEXT NOT NULL,
    account_auth_mode TEXT CHECK (
        account_auth_mode IS NULL OR account_auth_mode IN (
            'api_key', 'chatgpt', 'chatgpt_auth_tokens', 'headers',
            'agent_identity', 'personal_access_token', 'bedrock_api_key',
            'bedrock_access_keys'
        )
    ),
    FOREIGN KEY (operation_id, operation_kind)
        REFERENCES operations(id, operation_kind)
) STRICT;
INSERT INTO model_requests_v4(
    id, operation_id, operation_kind, provider_kind, model, transport_kind,
    attempt_number, account_profile_ref, client_origin, account_auth_mode
)
SELECT id, operation_id, operation_kind, provider_kind, model, transport_kind,
       attempt_number, account_profile_ref, client_origin, account_auth_mode
FROM model_requests;
DROP TABLE model_requests;
ALTER TABLE model_requests_v4 RENAME TO model_requests;
CREATE TRIGGER model_requests_no_update BEFORE UPDATE ON model_requests
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER model_requests_no_delete BEFORE DELETE ON model_requests
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;

CREATE TEMP TABLE usage_v4_foreign_key_check (
    violation_count INTEGER NOT NULL CHECK (violation_count = 0)
);
INSERT INTO usage_v4_foreign_key_check
SELECT COUNT(*) FROM pragma_foreign_key_check;
DROP TABLE usage_v4_foreign_key_check;

COMMIT;
PRAGMA foreign_keys = ON;
