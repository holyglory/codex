-- Prospective facts only: never infer or populate snapshots for older operations.
CREATE TABLE operation_work_contexts (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id),
    native_project_id TEXT CHECK (native_project_id IS NULL OR length(native_project_id) BETWEEN 1 AND 256),
    workstream_id TEXT CHECK (workstream_id IS NULL OR length(workstream_id) BETWEEN 1 AND 256),
    outcome_id TEXT CHECK (outcome_id IS NULL OR length(outcome_id) BETWEEN 1 AND 256),
    experiment_ref TEXT CHECK (experiment_ref IS NULL OR length(experiment_ref) BETWEEN 3 AND 256),
    provenance TEXT NOT NULL CHECK (provenance IN ('runtime_observed', 'unknown')),
    CHECK (native_project_id IS NOT NULL OR
           (workstream_id IS NULL AND outcome_id IS NULL AND experiment_ref IS NULL)),
    CHECK ((native_project_id IS NULL) = (provenance = 'unknown'))
) STRICT;

CREATE INDEX operation_work_contexts_outcome_idx
    ON operation_work_contexts(outcome_id, workstream_id, operation_id);

CREATE TRIGGER operation_work_contexts_no_update BEFORE UPDATE ON operation_work_contexts
BEGIN SELECT RAISE(ABORT, 'operation work contexts are immutable'); END;
CREATE TRIGGER operation_work_contexts_no_delete BEFORE DELETE ON operation_work_contexts
BEGIN SELECT RAISE(ABORT, 'operation work contexts cannot be deleted'); END;
