//! Compact hour/repository/thread/model/outcome/activity token rollups.
//! Unassigned historical outcomes remain empty; corrections move only recorded
//! contributions between classifications in the same fact transaction.
pub(super) const SCHEMA: &str = r#"
CREATE TABLE _usage_report_owner_dimensions(operation_id TEXT PRIMARY KEY NOT NULL, thread_id TEXT NOT NULL,model TEXT NOT NULL,provider_kind TEXT NOT NULL,native_project_id TEXT NOT NULL,workstream_id TEXT NOT NULL,outcome_id TEXT NOT NULL,phase TEXT NOT NULL,activity TEXT NOT NULL,provenance TEXT NOT NULL) STRICT;
CREATE TABLE _usage_report_dimension_tokens(hour_index INTEGER NOT NULL,repository_bucket TEXT NOT NULL,thread_id TEXT NOT NULL,model TEXT NOT NULL,provider_kind TEXT NOT NULL,native_project_id TEXT NOT NULL,workstream_id TEXT NOT NULL,outcome_id TEXT NOT NULL,phase TEXT NOT NULL,activity TEXT NOT NULL,provenance TEXT NOT NULL,category_path TEXT NOT NULL,measurement_provenance TEXT NOT NULL,coverage_state TEXT NOT NULL,measured_tokens INTEGER NOT NULL,unknown_observations INTEGER NOT NULL,observation_count INTEGER NOT NULL,aggregate_overflow INTEGER NOT NULL,PRIMARY KEY(hour_index,repository_bucket,thread_id,model,provider_kind,native_project_id,workstream_id,outcome_id,phase,activity,provenance,category_path,measurement_provenance,coverage_state)) STRICT;
CREATE INDEX _usage_report_dimension_thread ON _usage_report_dimension_tokens(thread_id);
CREATE INDEX _usage_report_dimension_empty ON _usage_report_dimension_tokens(observation_count) WHERE observation_count = 0;
CREATE TRIGGER _usage_report_dimension_token_insert AFTER INSERT ON token_observations WHEN NEW.category_path NOT GLOB 'attribution.items.*' BEGIN
INSERT OR IGNORE INTO _usage_report_owner_dimensions(operation_id,thread_id,model,provider_kind,native_project_id,workstream_id,outcome_id,phase,activity,provenance)
SELECT operation.id, COALESCE(operation.thread_id,''), COALESCE(request.model,''), COALESCE(request.provider_kind,''),
       COALESCE(context.native_project_id,''), COALESCE(context.workstream_id,''), COALESCE(context.outcome_id,''),
       COALESCE(effective.phase,operation.phase), COALESCE(effective.activity,operation.activity), COALESCE(effective.provenance,operation.attribution_provenance)
FROM operations AS operation LEFT JOIN model_requests AS request ON request.operation_id = operation.id
LEFT JOIN operation_work_contexts AS context ON context.operation_id = operation.id
LEFT JOIN effective_classification_events AS effective ON effective.operation_id = operation.id
WHERE operation.id = COALESCE((SELECT operation_id FROM model_requests WHERE id = NEW.model_request_id),(SELECT operation_id FROM tool_invocations WHERE id = NEW.tool_invocation_id));
INSERT INTO _usage_report_dimension_tokens(hour_index,repository_bucket,thread_id,model,provider_kind,native_project_id,workstream_id,outcome_id,phase,activity,provenance,category_path,measurement_provenance,coverage_state,measured_tokens,unknown_observations,observation_count,aggregate_overflow)
SELECT NEW.observed_at_ms / 3600000 - (NEW.observed_at_ms % 3600000 < 0),NEW.repository_bucket,owner.thread_id,owner.model,owner.provider_kind,owner.native_project_id,owner.workstream_id,owner.outcome_id,owner.phase,owner.activity,owner.provenance,NEW.category_path,NEW.measurement_provenance,NEW.coverage_state,COALESCE(NEW.token_count,0),NEW.token_count IS NULL,1,0 FROM _usage_report_owner_dimensions AS owner WHERE owner.operation_id = COALESCE((SELECT operation_id FROM model_requests WHERE id = NEW.model_request_id),(SELECT operation_id FROM tool_invocations WHERE id = NEW.tool_invocation_id))
ON CONFLICT(hour_index,repository_bucket,thread_id,model,provider_kind,native_project_id,workstream_id,outcome_id,phase,activity,provenance,category_path,measurement_provenance,coverage_state) DO UPDATE SET
 measured_tokens = CASE WHEN _usage_report_dimension_tokens.aggregate_overflow = 1 OR excluded.aggregate_overflow = 1
   OR _usage_report_dimension_tokens.measured_tokens > 9223372036854775807 - excluded.measured_tokens
   THEN _usage_report_dimension_tokens.measured_tokens ELSE _usage_report_dimension_tokens.measured_tokens + excluded.measured_tokens END,
 unknown_observations = _usage_report_dimension_tokens.unknown_observations + excluded.unknown_observations,
 observation_count = _usage_report_dimension_tokens.observation_count + excluded.observation_count,
 aggregate_overflow = _usage_report_dimension_tokens.aggregate_overflow = 1 OR excluded.aggregate_overflow = 1
   OR _usage_report_dimension_tokens.measured_tokens > 9223372036854775807 - excluded.measured_tokens;
END;
CREATE TRIGGER _usage_report_dimension_classification AFTER INSERT ON classification_events
WHEN EXISTS(SELECT 1 FROM effective_classification_events WHERE event_id = NEW.event_id)
BEGIN
UPDATE _usage_report_dimension_tokens SET
 measured_tokens = CASE WHEN aggregate_overflow = 1 THEN measured_tokens ELSE measured_tokens - (SELECT hours.measured_tokens FROM _usage_report_token_hours AS hours JOIN _usage_report_owner_dimensions AS owner ON owner.operation_id = hours.operation_id WHERE hours.operation_id = NEW.operation_id AND _usage_report_dimension_tokens.hour_index = hours.hour_index AND _usage_report_dimension_tokens.repository_bucket = hours.repository_bucket AND _usage_report_dimension_tokens.thread_id = owner.thread_id AND _usage_report_dimension_tokens.model = owner.model AND _usage_report_dimension_tokens.provider_kind = owner.provider_kind AND _usage_report_dimension_tokens.native_project_id = owner.native_project_id AND _usage_report_dimension_tokens.workstream_id = owner.workstream_id AND _usage_report_dimension_tokens.outcome_id = owner.outcome_id AND _usage_report_dimension_tokens.phase = owner.phase AND _usage_report_dimension_tokens.activity = owner.activity AND _usage_report_dimension_tokens.provenance = owner.provenance AND _usage_report_dimension_tokens.category_path = hours.category_path AND _usage_report_dimension_tokens.measurement_provenance = hours.measurement_provenance AND _usage_report_dimension_tokens.coverage_state = hours.coverage_state) END,
 unknown_observations = unknown_observations - (SELECT hours.unknown_observations FROM _usage_report_token_hours AS hours JOIN _usage_report_owner_dimensions AS owner ON owner.operation_id = hours.operation_id WHERE hours.operation_id = NEW.operation_id AND _usage_report_dimension_tokens.hour_index = hours.hour_index AND _usage_report_dimension_tokens.repository_bucket = hours.repository_bucket AND _usage_report_dimension_tokens.thread_id = owner.thread_id AND _usage_report_dimension_tokens.model = owner.model AND _usage_report_dimension_tokens.provider_kind = owner.provider_kind AND _usage_report_dimension_tokens.native_project_id = owner.native_project_id AND _usage_report_dimension_tokens.workstream_id = owner.workstream_id AND _usage_report_dimension_tokens.outcome_id = owner.outcome_id AND _usage_report_dimension_tokens.phase = owner.phase AND _usage_report_dimension_tokens.activity = owner.activity AND _usage_report_dimension_tokens.provenance = owner.provenance AND _usage_report_dimension_tokens.category_path = hours.category_path AND _usage_report_dimension_tokens.measurement_provenance = hours.measurement_provenance AND _usage_report_dimension_tokens.coverage_state = hours.coverage_state),
 observation_count = observation_count - (SELECT hours.observation_count FROM _usage_report_token_hours AS hours JOIN _usage_report_owner_dimensions AS owner ON owner.operation_id = hours.operation_id WHERE hours.operation_id = NEW.operation_id AND _usage_report_dimension_tokens.hour_index = hours.hour_index AND _usage_report_dimension_tokens.repository_bucket = hours.repository_bucket AND _usage_report_dimension_tokens.thread_id = owner.thread_id AND _usage_report_dimension_tokens.model = owner.model AND _usage_report_dimension_tokens.provider_kind = owner.provider_kind AND _usage_report_dimension_tokens.native_project_id = owner.native_project_id AND _usage_report_dimension_tokens.workstream_id = owner.workstream_id AND _usage_report_dimension_tokens.outcome_id = owner.outcome_id AND _usage_report_dimension_tokens.phase = owner.phase AND _usage_report_dimension_tokens.activity = owner.activity AND _usage_report_dimension_tokens.provenance = owner.provenance AND _usage_report_dimension_tokens.category_path = hours.category_path AND _usage_report_dimension_tokens.measurement_provenance = hours.measurement_provenance AND _usage_report_dimension_tokens.coverage_state = hours.coverage_state)
WHERE EXISTS(SELECT 1 FROM _usage_report_token_hours AS hours JOIN _usage_report_owner_dimensions AS owner ON owner.operation_id = hours.operation_id WHERE hours.operation_id = NEW.operation_id AND _usage_report_dimension_tokens.hour_index = hours.hour_index AND _usage_report_dimension_tokens.repository_bucket = hours.repository_bucket AND _usage_report_dimension_tokens.thread_id = owner.thread_id AND _usage_report_dimension_tokens.model = owner.model AND _usage_report_dimension_tokens.provider_kind = owner.provider_kind AND _usage_report_dimension_tokens.native_project_id = owner.native_project_id AND _usage_report_dimension_tokens.workstream_id = owner.workstream_id AND _usage_report_dimension_tokens.outcome_id = owner.outcome_id AND _usage_report_dimension_tokens.phase = owner.phase AND _usage_report_dimension_tokens.activity = owner.activity AND _usage_report_dimension_tokens.provenance = owner.provenance AND _usage_report_dimension_tokens.category_path = hours.category_path AND _usage_report_dimension_tokens.measurement_provenance = hours.measurement_provenance AND _usage_report_dimension_tokens.coverage_state = hours.coverage_state);
DELETE FROM _usage_report_dimension_tokens WHERE observation_count = 0;
UPDATE _usage_report_owner_dimensions SET phase = NEW.phase, activity = NEW.activity, provenance = NEW.provenance WHERE operation_id = NEW.operation_id;
INSERT INTO _usage_report_dimension_tokens(hour_index,repository_bucket,thread_id,model,provider_kind,native_project_id,workstream_id,outcome_id,phase,activity,provenance,category_path,measurement_provenance,coverage_state,measured_tokens,unknown_observations,observation_count,aggregate_overflow)
SELECT hours.hour_index,hours.repository_bucket,owner.thread_id,owner.model,owner.provider_kind,owner.native_project_id,owner.workstream_id,owner.outcome_id,owner.phase,owner.activity,owner.provenance,hours.category_path,hours.measurement_provenance,hours.coverage_state,hours.measured_tokens,hours.unknown_observations,hours.observation_count,hours.aggregate_overflow FROM _usage_report_token_hours AS hours
JOIN _usage_report_owner_dimensions AS owner ON owner.operation_id = hours.operation_id WHERE hours.operation_id = NEW.operation_id
ON CONFLICT(hour_index,repository_bucket,thread_id,model,provider_kind,native_project_id,workstream_id,outcome_id,phase,activity,provenance,category_path,measurement_provenance,coverage_state) DO UPDATE SET
 measured_tokens = CASE WHEN _usage_report_dimension_tokens.aggregate_overflow = 1 OR excluded.aggregate_overflow = 1
   OR _usage_report_dimension_tokens.measured_tokens > 9223372036854775807 - excluded.measured_tokens
   THEN _usage_report_dimension_tokens.measured_tokens ELSE _usage_report_dimension_tokens.measured_tokens + excluded.measured_tokens END,
 unknown_observations = _usage_report_dimension_tokens.unknown_observations + excluded.unknown_observations,
 observation_count = _usage_report_dimension_tokens.observation_count + excluded.observation_count,
 aggregate_overflow = _usage_report_dimension_tokens.aggregate_overflow = 1 OR excluded.aggregate_overflow = 1
   OR _usage_report_dimension_tokens.measured_tokens > 9223372036854775807 - excluded.measured_tokens;
END;
"#;

pub(super) const OWNERS: &str = r#"
INSERT OR IGNORE INTO _usage_report_owner_dimensions(operation_id,thread_id,model,provider_kind,native_project_id,workstream_id,outcome_id,phase,activity,provenance)
SELECT operation.id, COALESCE(operation.thread_id,''), COALESCE(request.model,''), COALESCE(request.provider_kind,''),
       COALESCE(context.native_project_id,''), COALESCE(context.workstream_id,''), COALESCE(context.outcome_id,''),
       COALESCE(effective.phase,operation.phase), COALESCE(effective.activity,operation.activity), COALESCE(effective.provenance,operation.attribution_provenance)
FROM operations AS operation LEFT JOIN model_requests AS request ON request.operation_id = operation.id
LEFT JOIN operation_work_contexts AS context ON context.operation_id = operation.id
LEFT JOIN effective_classification_events AS effective ON effective.operation_id = operation.id
WHERE operation.id IN (SELECT COALESCE(request.operation_id,tool.operation_id) FROM token_observations AS token
LEFT JOIN model_requests AS request ON request.id = token.model_request_id LEFT JOIN tool_invocations AS tool ON tool.id = token.tool_invocation_id
WHERE token.rowid > ?1 AND token.rowid <= ?2 AND token.category_path NOT GLOB 'attribution.items.*');
"#;

pub(super) const BACKFILL: &str = r#"
INSERT INTO _usage_report_dimension_tokens(hour_index,repository_bucket,thread_id,model,provider_kind,native_project_id,workstream_id,outcome_id,phase,activity,provenance,category_path,measurement_provenance,coverage_state,measured_tokens,unknown_observations,observation_count,aggregate_overflow)
SELECT token.observed_at_ms / 3600000 - (token.observed_at_ms % 3600000 < 0),token.repository_bucket,owner.thread_id,owner.model,owner.provider_kind,owner.native_project_id,owner.workstream_id,owner.outcome_id,owner.phase,owner.activity,owner.provenance,token.category_path,token.measurement_provenance,token.coverage_state,COALESCE(token.token_count,0),token.token_count IS NULL,1,0 FROM token_observations AS token
LEFT JOIN model_requests AS request ON request.id = token.model_request_id LEFT JOIN tool_invocations AS tool ON tool.id = token.tool_invocation_id
JOIN _usage_report_owner_dimensions AS owner ON owner.operation_id = COALESCE(request.operation_id,tool.operation_id)
WHERE token.rowid > ?1 AND token.rowid <= ?2 AND token.category_path NOT GLOB 'attribution.items.*'
ON CONFLICT(hour_index,repository_bucket,thread_id,model,provider_kind,native_project_id,workstream_id,outcome_id,phase,activity,provenance,category_path,measurement_provenance,coverage_state) DO UPDATE SET
 measured_tokens = CASE WHEN _usage_report_dimension_tokens.aggregate_overflow = 1 OR excluded.aggregate_overflow = 1
   OR _usage_report_dimension_tokens.measured_tokens > 9223372036854775807 - excluded.measured_tokens
   THEN _usage_report_dimension_tokens.measured_tokens ELSE _usage_report_dimension_tokens.measured_tokens + excluded.measured_tokens END,
 unknown_observations = _usage_report_dimension_tokens.unknown_observations + excluded.unknown_observations,
 observation_count = _usage_report_dimension_tokens.observation_count + excluded.observation_count,
 aggregate_overflow = _usage_report_dimension_tokens.aggregate_overflow = 1 OR excluded.aggregate_overflow = 1
   OR _usage_report_dimension_tokens.measured_tokens > 9223372036854775807 - excluded.measured_tokens;
"#;
