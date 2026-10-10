CREATE TABLE project_identity_aliases (
    alias_id TEXT PRIMARY KEY NOT NULL,
    canonical_project_id TEXT NOT NULL,
    alias_kind TEXT NOT NULL,
    canonical_identity_id TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    last_seen_at_ms INTEGER NOT NULL,
    FOREIGN KEY(canonical_project_id) REFERENCES project_automations(project_id)
        ON DELETE CASCADE
);

CREATE INDEX project_identity_aliases_canonical
    ON project_identity_aliases(canonical_project_id);

CREATE INDEX project_identity_aliases_kind
    ON project_identity_aliases(alias_kind, canonical_identity_id);

CREATE TABLE project_identity_conflicts (
    conflict_id INTEGER PRIMARY KEY AUTOINCREMENT,
    alias_id TEXT NOT NULL,
    canonical_project_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    source_revision INTEGER NOT NULL,
    target_revision INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    resolved_at_ms INTEGER
);

CREATE UNIQUE INDEX project_identity_conflicts_open
    ON project_identity_conflicts(alias_id, canonical_project_id, source_revision, target_revision, reason)
    WHERE resolved_at_ms IS NULL;
