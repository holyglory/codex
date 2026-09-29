//! Source-owned hourly token contributions. Raw facts stay durable and exact
//! boundary hours are read from them; complete inner hours use these rollups.
pub(super) const SCHEMA: &str = r#"
CREATE TABLE _usage_report_token_hours (
 operation_id TEXT NOT NULL, hour_index INTEGER NOT NULL,
 category_path TEXT NOT NULL, repository_bucket TEXT NOT NULL,
 measurement_provenance TEXT NOT NULL, coverage_state TEXT NOT NULL,
 measured_tokens INTEGER NOT NULL, unknown_observations INTEGER NOT NULL,
 observation_count INTEGER NOT NULL, aggregate_overflow INTEGER NOT NULL,
 PRIMARY KEY(operation_id, hour_index, category_path, repository_bucket, measurement_provenance, coverage_state)
) STRICT;
CREATE INDEX _usage_report_token_hours_time ON _usage_report_token_hours(hour_index, operation_id);
CREATE TRIGGER _usage_report_token_hour_insert AFTER INSERT ON token_observations
WHEN NEW.category_path NOT GLOB 'attribution.items.*'
BEGIN
INSERT INTO _usage_report_token_hours(operation_id, hour_index, category_path, repository_bucket, measurement_provenance, coverage_state, measured_tokens, unknown_observations, observation_count, aggregate_overflow)
SELECT COALESCE(request.operation_id, tool.operation_id),
 NEW.observed_at_ms / 3600000 - (NEW.observed_at_ms % 3600000 < 0),
 NEW.category_path, NEW.repository_bucket, NEW.measurement_provenance, NEW.coverage_state,
 COALESCE(NEW.token_count, 0), NEW.token_count IS NULL, 1, 0
FROM (SELECT 1) LEFT JOIN model_requests AS request ON request.id = NEW.model_request_id
LEFT JOIN tool_invocations AS tool ON tool.id = NEW.tool_invocation_id WHERE 1 = 1
ON CONFLICT(operation_id, hour_index, category_path, repository_bucket, measurement_provenance, coverage_state)
DO UPDATE SET
 measured_tokens = CASE WHEN _usage_report_token_hours.aggregate_overflow = 1 OR _usage_report_token_hours.measured_tokens > 9223372036854775807 - excluded.measured_tokens
   THEN _usage_report_token_hours.measured_tokens ELSE _usage_report_token_hours.measured_tokens + excluded.measured_tokens END,
 unknown_observations = _usage_report_token_hours.unknown_observations + excluded.unknown_observations,
 observation_count = _usage_report_token_hours.observation_count + 1,
 aggregate_overflow = _usage_report_token_hours.aggregate_overflow = 1 OR _usage_report_token_hours.measured_tokens > 9223372036854775807 - excluded.measured_tokens;
END;
"#;

pub(super) const BACKFILL: &str = r#"
INSERT INTO _usage_report_token_hours(operation_id, hour_index, category_path, repository_bucket, measurement_provenance, coverage_state, measured_tokens, unknown_observations, observation_count, aggregate_overflow)
SELECT COALESCE(request.operation_id, tool.operation_id),
 token.observed_at_ms / 3600000 - (token.observed_at_ms % 3600000 < 0),
 token.category_path, token.repository_bucket, token.measurement_provenance, token.coverage_state,
 COALESCE(token.token_count, 0), token.token_count IS NULL, 1, 0
FROM token_observations AS token
LEFT JOIN model_requests AS request ON request.id = token.model_request_id
LEFT JOIN tool_invocations AS tool ON tool.id = token.tool_invocation_id
WHERE token.rowid > ?1 AND token.rowid <= ?2 AND token.category_path NOT GLOB 'attribution.items.*'
ON CONFLICT(operation_id, hour_index, category_path, repository_bucket, measurement_provenance, coverage_state)
DO UPDATE SET
 measured_tokens = CASE WHEN _usage_report_token_hours.aggregate_overflow = 1 OR _usage_report_token_hours.measured_tokens > 9223372036854775807 - excluded.measured_tokens
   THEN _usage_report_token_hours.measured_tokens ELSE _usage_report_token_hours.measured_tokens + excluded.measured_tokens END,
 unknown_observations = _usage_report_token_hours.unknown_observations + excluded.unknown_observations,
 observation_count = _usage_report_token_hours.observation_count + 1,
 aggregate_overflow = _usage_report_token_hours.aggregate_overflow = 1 OR _usage_report_token_hours.measured_tokens > 9223372036854775807 - excluded.measured_tokens;
"#;
