CREATE TABLE project_automations (
    project_id TEXT PRIMARY KEY NOT NULL,
    subscription_id TEXT NOT NULL UNIQUE REFERENCES event_subscriptions(id),
    revision INTEGER NOT NULL,
    next_deadline_at_ms INTEGER,
    state_json TEXT NOT NULL
);
CREATE INDEX project_automation_deadline ON project_automations(next_deadline_at_ms)
    WHERE next_deadline_at_ms IS NOT NULL;
CREATE TABLE project_automation_history (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    at_ms INTEGER NOT NULL,
    command_json TEXT NOT NULL
);
CREATE INDEX project_automation_history_project ON project_automation_history(project_id, sequence);
CREATE TABLE project_event_observations (
    ordinal INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL,
    source_sequence TEXT NOT NULL,
    event_json TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    UNIQUE(source, source_sequence)
);
CREATE INDEX project_event_observations_source ON project_event_observations(source, ordinal);
CREATE TABLE project_review_workers (
    project_id TEXT NOT NULL REFERENCES project_automations(project_id),
    job_id TEXT NOT NULL,
    worker_thread_id TEXT NOT NULL UNIQUE,
    claimed_at_ms INTEGER NOT NULL,
    PRIMARY KEY(project_id, job_id)
);
