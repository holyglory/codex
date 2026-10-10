CREATE TABLE alarms (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    dedupe_key TEXT NOT NULL,
    spec_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('armed','due','delivered','acknowledged','cancelled','expired')),
    created_at_ms INTEGER NOT NULL,
    due_at_ms INTEGER,
    active_target_ms INTEGER,
    expires_at_ms INTEGER,
    delivered_at_ms INTEGER,
    acknowledged_at_ms INTEGER,
    UNIQUE(thread_id, scope_key, dedupe_key)
);
CREATE INDEX alarms_due ON alarms(state, due_at_ms);
CREATE TABLE alarm_work (
    thread_id TEXT PRIMARY KEY,
    accumulated_ms INTEGER NOT NULL DEFAULT 0,
    observed_at_ms INTEGER NOT NULL,
    active_count INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE alarm_work_operations (
    operation_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    eligible INTEGER NOT NULL,
    waits INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE alarm_operation_results (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    operation_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    result_json TEXT NOT NULL,
    observed_at_ms INTEGER NOT NULL,
    UNIQUE(thread_id, operation_id)
);
-- Retain byte-complete legacy clock state for export; disable execution, never
-- manufacture a review or delivery receipt during migration.
CREATE TABLE project_automation_exports (
    project_id TEXT PRIMARY KEY,
    state_json TEXT NOT NULL
);
INSERT INTO project_automation_exports(project_id, state_json)
    SELECT project_id, state_json FROM project_automations;
UPDATE project_automations SET next_deadline_at_ms = NULL;
DELETE FROM event_subscription_pending_wakes WHERE event_source = 'codex.project';
