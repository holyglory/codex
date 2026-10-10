CREATE TABLE activity_declarations (
    thread_id TEXT PRIMARY KEY NOT NULL,
    active_phase TEXT,
    active_activity TEXT,
    active_rework_of_operation_id TEXT,
    staged_phase TEXT,
    staged_activity TEXT,
    staged_rework_of_operation_id TEXT,
    parent_inheritance_blocked INTEGER NOT NULL CHECK(parent_inheritance_blocked IN (0, 1)),
    updated_at_ms INTEGER NOT NULL
) STRICT;
