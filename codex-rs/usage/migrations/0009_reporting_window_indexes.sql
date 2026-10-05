-- Keep finite performance reviews on indexed time seeks even while the
-- derived report cache is rebuilding. These indexes are also used by the
-- canonical fallback path for late observations and open operations.
CREATE INDEX operations_started_at_idx
    ON operations(started_at_ms, id);

CREATE INDEX token_observations_observed_at_idx
    ON token_observations(observed_at_ms, model_request_id, tool_invocation_id);

CREATE INDEX coverage_events_occurred_at_idx
    ON coverage_events(occurred_at_ms, operation_id);
