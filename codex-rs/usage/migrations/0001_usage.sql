CREATE TABLE process_instances (
    id TEXT PRIMARY KEY NOT NULL,
    os_pid INTEGER NOT NULL CHECK (os_pid >= 0),
    started_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE process_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    process_id TEXT NOT NULL REFERENCES process_instances(id),
    event_kind TEXT NOT NULL CHECK (event_kind IN ('heartbeat', 'ended')),
    occurred_at_ms INTEGER NOT NULL,
    UNIQUE(process_id, event_kind, occurred_at_ms)
) STRICT;

CREATE UNIQUE INDEX process_events_one_end
    ON process_events(process_id)
    WHERE event_kind = 'ended';

CREATE TABLE taxonomy_versions (
    version INTEGER PRIMARY KEY NOT NULL CHECK (version > 0),
    schema_migration INTEGER NOT NULL CHECK (schema_migration > 0),
    mapping_key TEXT NOT NULL UNIQUE,
    supersedes_version INTEGER REFERENCES taxonomy_versions(version)
) STRICT;

INSERT INTO taxonomy_versions(
    version, schema_migration, mapping_key, supersedes_version
) VALUES (1, 1, 'builtin_v1', NULL);

CREATE TABLE threads (
    id TEXT PRIMARY KEY NOT NULL,
    parent_thread_id TEXT REFERENCES threads(id),
    source_kind TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE thread_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    event_kind TEXT NOT NULL,
    occurred_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE turns (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    account_profile_ref TEXT,
    created_at_ms INTEGER NOT NULL,
    UNIQUE(id, thread_id)
) STRICT;

CREATE TABLE turn_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    turn_id TEXT NOT NULL REFERENCES turns(id),
    event_kind TEXT NOT NULL,
    occurred_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE agents (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    parent_agent_id TEXT REFERENCES agents(id),
    role_kind TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    UNIQUE(id, thread_id)
) STRICT;

CREATE TABLE agent_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    agent_id TEXT NOT NULL REFERENCES agents(id),
    event_kind TEXT NOT NULL,
    occurred_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE operations (
    id TEXT PRIMARY KEY NOT NULL,
    process_id TEXT NOT NULL REFERENCES process_instances(id),
    thread_id TEXT,
    turn_id TEXT,
    agent_id TEXT,
    parent_operation_id TEXT REFERENCES operations(id),
    retry_of_operation_id TEXT REFERENCES operations(id),
    rework_of_operation_id TEXT REFERENCES operations(id),
    operation_kind TEXT NOT NULL CHECK (
        operation_kind IN (
            'model_request', 'local_tool', 'hosted_tool', 'activity_control'
        )
    ),
    started_at_ms INTEGER NOT NULL,
    taxonomy_version INTEGER NOT NULL REFERENCES taxonomy_versions(version),
    phase TEXT NOT NULL,
    activity TEXT NOT NULL,
    activity_state TEXT NOT NULL,
    attribution_provenance TEXT NOT NULL CHECK (
        attribution_provenance IN (
            'agent_declared', 'deterministic_classification',
            'inferred_classification', 'user_corrected', 'unknown'
        )
    ),
    UNIQUE(id, operation_kind),
    CHECK (turn_id IS NULL OR thread_id IS NOT NULL),
    CHECK (agent_id IS NULL OR thread_id IS NOT NULL),
    FOREIGN KEY (thread_id) REFERENCES threads(id),
    FOREIGN KEY (turn_id, thread_id) REFERENCES turns(id, thread_id),
    FOREIGN KEY (agent_id, thread_id) REFERENCES agents(id, thread_id)
) STRICT;

CREATE TABLE operation_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL REFERENCES operations(id),
    event_kind TEXT NOT NULL,
    terminal INTEGER NOT NULL CHECK (terminal IN (0, 1)),
    occurred_at_ms INTEGER NOT NULL,
    duration_ns INTEGER CHECK (duration_ns IS NULL OR duration_ns >= 0),
    error_category TEXT
) STRICT;

CREATE UNIQUE INDEX operation_events_one_terminal
    ON operation_events(operation_id)
    WHERE terminal = 1;

CREATE TABLE model_requests (
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
    FOREIGN KEY (operation_id, operation_kind)
        REFERENCES operations(id, operation_kind)
) STRICT;

CREATE TABLE tool_invocations (
    id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL UNIQUE,
    operation_kind TEXT NOT NULL CHECK (
        operation_kind IN ('local_tool', 'hosted_tool', 'activity_control')
    ),
    tool_kind TEXT NOT NULL,
    safe_tool_name TEXT NOT NULL,
    operation_family TEXT NOT NULL,
    observation_timing TEXT NOT NULL,
    covering_model_request_id TEXT REFERENCES model_requests(id),
    CHECK (
        (operation_kind = 'hosted_tool' AND covering_model_request_id IS NOT NULL) OR
        (operation_kind <> 'hosted_tool' AND covering_model_request_id IS NULL)
    ),
    FOREIGN KEY (operation_id, operation_kind)
        REFERENCES operations(id, operation_kind)
) STRICT;

CREATE TABLE token_observations (
    id TEXT PRIMARY KEY NOT NULL,
    model_request_id TEXT REFERENCES model_requests(id),
    tool_invocation_id TEXT REFERENCES tool_invocations(id),
    source_event_id TEXT NOT NULL CHECK (length(source_event_id) = 36),
    category_path TEXT NOT NULL CHECK (
        length(category_path) BETWEEN 1 AND 192
    ),
    token_count INTEGER CHECK (token_count IS NULL OR token_count >= 0),
    unit TEXT NOT NULL CHECK (unit = 'tokens'),
    measurement_provenance TEXT NOT NULL CHECK (
        measurement_provenance IN (
            'provider_reported', 'imported', 'unknown'
        )
    ),
    coverage_state TEXT NOT NULL,
    repository_bucket TEXT NOT NULL,
    observed_at_ms INTEGER NOT NULL,
    CHECK ((model_request_id IS NULL) <> (tool_invocation_id IS NULL)),
    CHECK (token_count IS NOT NULL OR coverage_state <> 'complete'),
    CHECK (
        (measurement_provenance = 'unknown' AND token_count IS NULL) OR
        measurement_provenance IN ('provider_reported', 'imported')
    )
) STRICT;

CREATE UNIQUE INDEX token_observations_model_source_category
    ON token_observations(model_request_id, source_event_id, category_path)
    WHERE model_request_id IS NOT NULL;

CREATE UNIQUE INDEX token_observations_tool_source_category
    ON token_observations(tool_invocation_id, source_event_id, category_path)
    WHERE tool_invocation_id IS NOT NULL;

CREATE TABLE tool_approval_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    tool_invocation_id TEXT NOT NULL REFERENCES tool_invocations(id),
    outcome TEXT NOT NULL CHECK (
        outcome IN ('not_required', 'approved', 'denied', 'timed_out', 'cancelled')
    ),
    provenance TEXT NOT NULL CHECK (
        provenance IN ('policy', 'cache', 'permission_hook', 'guardian', 'user', 'unknown')
    ),
    occurred_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE repositories (
    id TEXT PRIMARY KEY NOT NULL,
    identity_source TEXT NOT NULL CHECK (
        identity_source IN ('origin', 'git_common_dir', 'workspace')
    ),
    safe_display_label TEXT NOT NULL CHECK (
        length(safe_display_label) BETWEEN 1 AND 80
    ),
    created_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE repository_seen_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    repository_id TEXT NOT NULL REFERENCES repositories(id),
    occurred_at_ms INTEGER NOT NULL,
    UNIQUE(repository_id, occurred_at_ms)
) STRICT;

CREATE TABLE repository_alias_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    repository_id TEXT NOT NULL REFERENCES repositories(id),
    safe_alias TEXT NOT NULL CHECK (length(safe_alias) BETWEEN 1 AND 80),
    occurred_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE repository_merge_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    source_repository_id TEXT NOT NULL REFERENCES repositories(id),
    target_repository_id TEXT NOT NULL REFERENCES repositories(id),
    occurred_at_ms INTEGER NOT NULL,
    UNIQUE(source_repository_id),
    CHECK (source_repository_id <> target_repository_id)
) STRICT;

CREATE TABLE repository_attributions (
    event_id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL REFERENCES operations(id),
    repository_id TEXT REFERENCES repositories(id),
    attribution_kind TEXT NOT NULL CHECK (
        attribution_kind IN (
            'primary', 'observed_cwd', 'file_change', 'multi_repo', 'unknown'
        )
    ),
    provenance TEXT NOT NULL CHECK (
        provenance IN ('runtime_observed', 'imported', 'unknown')
    ),
    occurred_at_ms INTEGER NOT NULL,
    CHECK (
        (repository_id IS NOT NULL AND attribution_kind IN (
            'primary', 'observed_cwd', 'file_change'
        )) OR
        (repository_id IS NULL AND attribution_kind IN ('multi_repo', 'unknown'))
    )
) STRICT;

CREATE TABLE classification_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL REFERENCES operations(id),
    taxonomy_version INTEGER NOT NULL REFERENCES taxonomy_versions(version),
    phase TEXT NOT NULL,
    activity TEXT NOT NULL,
    activity_state TEXT NOT NULL,
    provenance TEXT NOT NULL CHECK (
        provenance IN (
            'agent_declared', 'deterministic_classification',
            'inferred_classification', 'user_corrected', 'unknown'
        )
    ),
    supersedes_event_id TEXT,
    occurred_at_ms INTEGER NOT NULL,
    UNIQUE(event_id, operation_id),
    FOREIGN KEY (supersedes_event_id, operation_id)
        REFERENCES classification_events(event_id, operation_id)
) STRICT;

CREATE UNIQUE INDEX classification_events_one_successor
    ON classification_events(supersedes_event_id)
    WHERE supersedes_event_id IS NOT NULL;

CREATE VIEW effective_classification_events AS
SELECT classification.*
FROM classification_events AS classification
WHERE NOT EXISTS (
    SELECT 1 FROM classification_events AS successor
    WHERE successor.supersedes_event_id = classification.event_id
);

CREATE TRIGGER token_observations_hosted_tool_source BEFORE INSERT ON token_observations
WHEN NEW.tool_invocation_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM tool_invocations AS tool
    WHERE tool.id = NEW.tool_invocation_id
      AND tool.operation_kind = 'hosted_tool'
      AND tool.covering_model_request_id IS NOT NULL
)
BEGIN SELECT RAISE(ABORT, 'token tool source lacks a covering model request'); END;

CREATE TABLE coverage_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT REFERENCES operations(id),
    scope_kind TEXT NOT NULL,
    coverage_state TEXT NOT NULL,
    reason_code TEXT,
    occurred_at_ms INTEGER NOT NULL
) STRICT;

CREATE INDEX operations_thread_turn_idx ON operations(thread_id, turn_id);
CREATE INDEX operations_agent_idx ON operations(agent_id);
CREATE INDEX process_events_process_idx ON process_events(process_id);
CREATE INDEX operation_events_operation_idx ON operation_events(operation_id);
CREATE INDEX token_observations_request_idx ON token_observations(model_request_id);
CREATE INDEX token_observations_tool_idx ON token_observations(tool_invocation_id);
CREATE INDEX repository_attributions_operation_idx
    ON repository_attributions(operation_id);
CREATE INDEX repository_seen_events_repository_idx
    ON repository_seen_events(repository_id, occurred_at_ms);
CREATE INDEX repository_alias_events_repository_idx
    ON repository_alias_events(repository_id, occurred_at_ms);
CREATE INDEX repository_merge_events_source_idx
    ON repository_merge_events(source_repository_id, occurred_at_ms);
CREATE INDEX classification_events_operation_idx ON classification_events(operation_id);
CREATE INDEX coverage_events_operation_idx ON coverage_events(operation_id);

CREATE TRIGGER agents_parent_matches_thread BEFORE INSERT ON agents
WHEN NEW.parent_agent_id IS NOT NULL AND NOT EXISTS (
    SELECT 1
    FROM threads AS child_thread
    JOIN agents AS parent_agent ON parent_agent.id = NEW.parent_agent_id
    WHERE child_thread.id = NEW.thread_id
      AND child_thread.parent_thread_id = parent_agent.thread_id
)
BEGIN SELECT RAISE(ABORT, 'usage agent parent does not own parent thread'); END;

CREATE TRIGGER thread_events_no_update BEFORE UPDATE ON thread_events
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER thread_events_no_delete BEFORE DELETE ON thread_events
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER turn_events_no_update BEFORE UPDATE ON turn_events
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER turn_events_no_delete BEFORE DELETE ON turn_events
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER agent_events_no_update BEFORE UPDATE ON agent_events
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER agent_events_no_delete BEFORE DELETE ON agent_events
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER operation_events_no_update BEFORE UPDATE ON operation_events
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER operation_events_no_delete BEFORE DELETE ON operation_events
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER token_observations_no_update BEFORE UPDATE ON token_observations
BEGIN SELECT RAISE(ABORT, 'token observations are append-only'); END;
CREATE TRIGGER token_observations_no_delete BEFORE DELETE ON token_observations
BEGIN SELECT RAISE(ABORT, 'token observations cannot be deleted'); END;
CREATE TRIGGER tool_approval_events_no_update BEFORE UPDATE ON tool_approval_events
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER tool_approval_events_no_delete BEFORE DELETE ON tool_approval_events
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER repository_attributions_no_update BEFORE UPDATE ON repository_attributions
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER repository_attributions_no_delete BEFORE DELETE ON repository_attributions
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER repository_seen_events_no_update BEFORE UPDATE ON repository_seen_events
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER repository_seen_events_no_delete BEFORE DELETE ON repository_seen_events
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER repository_alias_events_no_update BEFORE UPDATE ON repository_alias_events
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER repository_alias_events_no_delete BEFORE DELETE ON repository_alias_events
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER repository_merge_events_no_update BEFORE UPDATE ON repository_merge_events
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER repository_merge_events_no_delete BEFORE DELETE ON repository_merge_events
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER classification_events_no_update BEFORE UPDATE ON classification_events
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER classification_events_no_delete BEFORE DELETE ON classification_events
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER coverage_events_no_update BEFORE UPDATE ON coverage_events
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER coverage_events_no_delete BEFORE DELETE ON coverage_events
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER process_events_no_update BEFORE UPDATE ON process_events
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER process_events_no_delete BEFORE DELETE ON process_events
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER process_events_no_heartbeat_after_end BEFORE INSERT ON process_events
WHEN NEW.event_kind = 'heartbeat' AND EXISTS (
    SELECT 1 FROM process_events
    WHERE process_id = NEW.process_id AND event_kind = 'ended'
)
AND NOT EXISTS (
    SELECT 1 FROM process_events
    WHERE process_id = NEW.process_id
      AND event_kind = 'heartbeat'
      AND occurred_at_ms = NEW.occurred_at_ms
)
BEGIN SELECT RAISE(ABORT, 'usage process has already ended'); END;
CREATE TRIGGER process_instances_no_update BEFORE UPDATE ON process_instances
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER process_instances_no_delete BEFORE DELETE ON process_instances
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;
CREATE TRIGGER taxonomy_versions_no_update BEFORE UPDATE ON taxonomy_versions
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER taxonomy_versions_no_delete BEFORE DELETE ON taxonomy_versions
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;
CREATE TRIGGER threads_no_update BEFORE UPDATE ON threads
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER threads_no_delete BEFORE DELETE ON threads
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;
CREATE TRIGGER turns_no_update BEFORE UPDATE ON turns
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER turns_no_delete BEFORE DELETE ON turns
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;
CREATE TRIGGER agents_no_update BEFORE UPDATE ON agents
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER agents_no_delete BEFORE DELETE ON agents
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;
CREATE TRIGGER operations_no_update BEFORE UPDATE ON operations
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER operations_no_delete BEFORE DELETE ON operations
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;
CREATE TRIGGER model_requests_no_update BEFORE UPDATE ON model_requests
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER model_requests_no_delete BEFORE DELETE ON model_requests
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;
CREATE TRIGGER tool_invocations_no_update BEFORE UPDATE ON tool_invocations
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER tool_invocations_no_delete BEFORE DELETE ON tool_invocations
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;
CREATE TRIGGER repositories_no_update BEFORE UPDATE ON repositories
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER repositories_no_delete BEFORE DELETE ON repositories
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;
