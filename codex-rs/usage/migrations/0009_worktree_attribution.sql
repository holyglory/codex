CREATE TABLE worktrees (
    id TEXT PRIMARY KEY NOT NULL,
    repository_id TEXT NOT NULL REFERENCES repositories(id),
    safe_display_label TEXT NOT NULL CHECK (
        length(safe_display_label) BETWEEN 1 AND 80
    ),
    created_at_ms INTEGER NOT NULL
) STRICT;

CREATE INDEX worktrees_repository_idx ON worktrees(repository_id, id);

ALTER TABLE repository_attributions ADD COLUMN worktree_id TEXT REFERENCES worktrees(id);

CREATE INDEX repository_attributions_worktree_operation_idx
    ON repository_attributions(worktree_id, operation_id)
    WHERE worktree_id IS NOT NULL;

CREATE TRIGGER worktrees_no_update BEFORE UPDATE ON worktrees
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER worktrees_no_delete BEFORE DELETE ON worktrees
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;
