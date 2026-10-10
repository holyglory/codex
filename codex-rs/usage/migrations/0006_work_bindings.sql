CREATE TABLE work_bindings (
    event_id TEXT PRIMARY KEY NOT NULL CHECK (length(event_id) = 36),
    thread_id TEXT NOT NULL REFERENCES threads(id),
    native_project_id TEXT NOT NULL CHECK (length(native_project_id) BETWEEN 1 AND 256),
    workstream_id TEXT CHECK (workstream_id IS NULL OR length(workstream_id) BETWEEN 1 AND 256),
    outcome_id TEXT CHECK (outcome_id IS NULL OR length(outcome_id) BETWEEN 1 AND 256),
    experiment_ref TEXT CHECK (experiment_ref IS NULL OR length(experiment_ref) BETWEEN 3 AND 256),
    observed_at_ms INTEGER NOT NULL,
    provenance TEXT NOT NULL CHECK (provenance IN ('runtime_observed', 'unknown'))
) STRICT;

CREATE INDEX work_bindings_thread_observed_idx
    ON work_bindings(thread_id, observed_at_ms DESC, event_id);
CREATE INDEX work_bindings_project_workstream_observed_idx
    ON work_bindings(native_project_id, workstream_id, observed_at_ms, event_id);
CREATE INDEX work_bindings_observed_idx ON work_bindings(observed_at_ms, event_id);

CREATE TRIGGER work_bindings_no_update BEFORE UPDATE ON work_bindings
BEGIN SELECT RAISE(ABORT, 'work bindings are append-only'); END;
CREATE TRIGGER work_bindings_no_delete BEFORE DELETE ON work_bindings
BEGIN SELECT RAISE(ABORT, 'work bindings cannot be deleted'); END;
