CREATE TABLE model_request_context_sources (
    model_request_id TEXT PRIMARY KEY NOT NULL REFERENCES model_requests(id),
    policy_estimated_tokens INTEGER NOT NULL CHECK (policy_estimated_tokens >= 0),
    conversation_estimated_tokens INTEGER NOT NULL CHECK (conversation_estimated_tokens >= 0),
    tool_output_estimated_tokens INTEGER NOT NULL CHECK (tool_output_estimated_tokens >= 0),
    estimator TEXT NOT NULL CHECK (estimator = 'approx_model_visible_v1'),
    observed_at_ms INTEGER NOT NULL
) STRICT;

ALTER TABLE tool_invocations ADD COLUMN execution_group_id TEXT;
ALTER TABLE tool_invocations ADD COLUMN execution_role TEXT NOT NULL DEFAULT 'standalone'
    CHECK (execution_role IN ('standalone', 'wrapper', 'nested'));

CREATE INDEX model_request_context_sources_observed_idx
    ON model_request_context_sources(observed_at_ms);
CREATE INDEX tool_invocations_execution_group_idx
    ON tool_invocations(execution_group_id);
CREATE INDEX threads_parent_idx ON threads(parent_thread_id);
CREATE INDEX operations_thread_started_idx
    ON operations(thread_id, started_at_ms);
CREATE INDEX token_observations_total_provider_observed_idx
    ON token_observations(observed_at_ms)
    WHERE category_path = 'total_tokens'
      AND measurement_provenance = 'provider_reported';

CREATE TRIGGER model_request_context_sources_no_update
BEFORE UPDATE ON model_request_context_sources
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;

CREATE TRIGGER model_request_context_sources_no_delete
BEFORE DELETE ON model_request_context_sources
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;
