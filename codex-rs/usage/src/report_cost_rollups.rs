//! Incremental standard-cost input groups. Prices are applied to compact
//! component totals so rate-card versions do not rewrite canonical observations.
//! Each component arrival atomically replaces its receipt's old contribution.
pub(super) const SCHEMA: &str = r#"
CREATE TABLE _usage_report_cost_totals(
 thread_id TEXT NOT NULL,
 repository_bucket TEXT NOT NULL,
 model TEXT NOT NULL,
 provider_kind TEXT NOT NULL,
 native_project_id TEXT NOT NULL,
 workstream_id TEXT NOT NULL,
 outcome_id TEXT NOT NULL,
 long_context INTEGER NOT NULL,
 complete INTEGER NOT NULL,
 input_tokens INTEGER NOT NULL,
 cached_input_tokens INTEGER NOT NULL,
 cache_write_tokens INTEGER NOT NULL,
 output_tokens INTEGER NOT NULL,
 total_tokens INTEGER NOT NULL,
 reasoning_tokens INTEGER NOT NULL,
 uncached_input_tokens INTEGER NOT NULL,
 observations INTEGER NOT NULL,
 aggregate_overflow INTEGER NOT NULL,
 PRIMARY KEY(thread_id,repository_bucket,model,provider_kind,native_project_id,workstream_id,outcome_id,long_context,complete)) STRICT;
CREATE INDEX _usage_report_cost_empty ON _usage_report_cost_totals(observations) WHERE observations = 0;
CREATE VIEW _usage_report_cost_contributions AS
SELECT receipt.model_request_id, receipt.source_event_id, receipt.repository_bucket,
 COALESCE(operation.thread_id,'') AS thread_id, request.model, request.provider_kind,
 COALESCE(context.native_project_id,'') AS native_project_id,
 COALESCE(context.workstream_id,'') AS workstream_id, COALESCE(context.outcome_id,'') AS outcome_id,
 COALESCE(receipt.input_tokens > 272000,0) AS long_context,
 COALESCE((receipt.complete_mask & 31) = 31 AND receipt.input_tokens IS NOT NULL
  AND receipt.cached_input_tokens IS NOT NULL AND receipt.cache_write_tokens IS NOT NULL
  AND receipt.output_tokens IS NOT NULL AND receipt.total_tokens IS NOT NULL
  AND receipt.cached_input_tokens <= receipt.input_tokens
  AND receipt.cache_write_tokens <= receipt.input_tokens - receipt.cached_input_tokens
  AND (receipt.reasoning_tokens IS NULL OR receipt.reasoning_tokens <= receipt.output_tokens),0) AS complete,
 COALESCE(receipt.input_tokens,0) AS input_tokens,
 COALESCE(receipt.cached_input_tokens,0) AS cached_input_tokens,
 COALESCE(receipt.cache_write_tokens,0) AS cache_write_tokens,
 COALESCE(receipt.output_tokens,0) AS output_tokens,
 COALESCE(receipt.total_tokens,0) AS total_tokens,
 COALESCE(receipt.reasoning_tokens,0) AS reasoning_tokens,
 CASE WHEN receipt.input_tokens >= receipt.cached_input_tokens AND receipt.input_tokens - receipt.cached_input_tokens >= receipt.cache_write_tokens
  THEN receipt.input_tokens - receipt.cached_input_tokens - receipt.cache_write_tokens ELSE 0 END AS uncached_input_tokens
FROM _usage_report_model_usage AS receipt
JOIN model_requests AS request ON request.id = receipt.model_request_id
JOIN operations AS operation ON operation.id = request.operation_id
LEFT JOIN operation_work_contexts AS context ON context.operation_id = operation.id;
CREATE TRIGGER _usage_report_cost_remove BEFORE UPDATE ON _usage_report_model_usage BEGIN
UPDATE _usage_report_cost_totals SET (input_tokens,cached_input_tokens,cache_write_tokens,output_tokens,total_tokens,reasoning_tokens,uncached_input_tokens,observations) = (
 SELECT  CASE WHEN aggregate_overflow = 1 THEN _usage_report_cost_totals.input_tokens ELSE _usage_report_cost_totals.input_tokens - contribution.input_tokens END,
 CASE WHEN aggregate_overflow = 1 THEN _usage_report_cost_totals.cached_input_tokens ELSE _usage_report_cost_totals.cached_input_tokens - contribution.cached_input_tokens END,
 CASE WHEN aggregate_overflow = 1 THEN _usage_report_cost_totals.cache_write_tokens ELSE _usage_report_cost_totals.cache_write_tokens - contribution.cache_write_tokens END,
 CASE WHEN aggregate_overflow = 1 THEN _usage_report_cost_totals.output_tokens ELSE _usage_report_cost_totals.output_tokens - contribution.output_tokens END,
 CASE WHEN aggregate_overflow = 1 THEN _usage_report_cost_totals.total_tokens ELSE _usage_report_cost_totals.total_tokens - contribution.total_tokens END,
 CASE WHEN aggregate_overflow = 1 THEN _usage_report_cost_totals.reasoning_tokens ELSE _usage_report_cost_totals.reasoning_tokens - contribution.reasoning_tokens END,
 CASE WHEN aggregate_overflow = 1 THEN _usage_report_cost_totals.uncached_input_tokens ELSE _usage_report_cost_totals.uncached_input_tokens - contribution.uncached_input_tokens END, observations - 1
 FROM _usage_report_cost_contributions AS contribution WHERE contribution.model_request_id = OLD.model_request_id AND contribution.source_event_id = OLD.source_event_id AND contribution.repository_bucket = OLD.repository_bucket AND _usage_report_cost_totals.thread_id = contribution.thread_id AND _usage_report_cost_totals.repository_bucket = contribution.repository_bucket AND _usage_report_cost_totals.model = contribution.model AND _usage_report_cost_totals.provider_kind = contribution.provider_kind AND _usage_report_cost_totals.native_project_id = contribution.native_project_id AND _usage_report_cost_totals.workstream_id = contribution.workstream_id AND _usage_report_cost_totals.outcome_id = contribution.outcome_id AND _usage_report_cost_totals.long_context = contribution.long_context AND _usage_report_cost_totals.complete = contribution.complete)
WHERE (thread_id,repository_bucket,model,provider_kind,native_project_id,workstream_id,outcome_id,long_context,complete) = (SELECT thread_id,repository_bucket,model,provider_kind,native_project_id,workstream_id,outcome_id,long_context,complete FROM _usage_report_cost_contributions WHERE model_request_id = OLD.model_request_id AND source_event_id = OLD.source_event_id AND repository_bucket = OLD.repository_bucket);
DELETE FROM _usage_report_cost_totals WHERE observations = 0;
END;
CREATE TRIGGER _usage_report_cost_add_insert AFTER INSERT ON _usage_report_model_usage BEGIN
INSERT INTO _usage_report_cost_totals(thread_id,repository_bucket,model,provider_kind,native_project_id,workstream_id,outcome_id,long_context,complete,input_tokens,cached_input_tokens,cache_write_tokens,output_tokens,total_tokens,reasoning_tokens,uncached_input_tokens,observations,aggregate_overflow)
SELECT thread_id,repository_bucket,model,provider_kind,native_project_id,workstream_id,outcome_id,long_context,complete,input_tokens,cached_input_tokens,cache_write_tokens,output_tokens,total_tokens,reasoning_tokens,uncached_input_tokens,1,0 FROM _usage_report_cost_contributions
WHERE model_request_id = NEW.model_request_id AND source_event_id = NEW.source_event_id AND repository_bucket = NEW.repository_bucket
ON CONFLICT(thread_id,repository_bucket,model,provider_kind,native_project_id,workstream_id,outcome_id,long_context,complete) DO UPDATE SET
 input_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.input_tokens > 9223372036854775807 - excluded.input_tokens THEN _usage_report_cost_totals.input_tokens ELSE _usage_report_cost_totals.input_tokens + excluded.input_tokens END,
 cached_input_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.cached_input_tokens > 9223372036854775807 - excluded.cached_input_tokens THEN _usage_report_cost_totals.cached_input_tokens ELSE _usage_report_cost_totals.cached_input_tokens + excluded.cached_input_tokens END,
 cache_write_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.cache_write_tokens > 9223372036854775807 - excluded.cache_write_tokens THEN _usage_report_cost_totals.cache_write_tokens ELSE _usage_report_cost_totals.cache_write_tokens + excluded.cache_write_tokens END,
 output_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.output_tokens > 9223372036854775807 - excluded.output_tokens THEN _usage_report_cost_totals.output_tokens ELSE _usage_report_cost_totals.output_tokens + excluded.output_tokens END,
 total_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.total_tokens > 9223372036854775807 - excluded.total_tokens THEN _usage_report_cost_totals.total_tokens ELSE _usage_report_cost_totals.total_tokens + excluded.total_tokens END,
 reasoning_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.reasoning_tokens > 9223372036854775807 - excluded.reasoning_tokens THEN _usage_report_cost_totals.reasoning_tokens ELSE _usage_report_cost_totals.reasoning_tokens + excluded.reasoning_tokens END,
 uncached_input_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.uncached_input_tokens > 9223372036854775807 - excluded.uncached_input_tokens THEN _usage_report_cost_totals.uncached_input_tokens ELSE _usage_report_cost_totals.uncached_input_tokens + excluded.uncached_input_tokens END,
 observations = _usage_report_cost_totals.observations + 1,
 aggregate_overflow = _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.input_tokens > 9223372036854775807 - excluded.input_tokens OR _usage_report_cost_totals.cached_input_tokens > 9223372036854775807 - excluded.cached_input_tokens OR _usage_report_cost_totals.cache_write_tokens > 9223372036854775807 - excluded.cache_write_tokens OR _usage_report_cost_totals.output_tokens > 9223372036854775807 - excluded.output_tokens OR _usage_report_cost_totals.total_tokens > 9223372036854775807 - excluded.total_tokens OR _usage_report_cost_totals.reasoning_tokens > 9223372036854775807 - excluded.reasoning_tokens OR _usage_report_cost_totals.uncached_input_tokens > 9223372036854775807 - excluded.uncached_input_tokens;
END;
CREATE TRIGGER _usage_report_cost_add_update AFTER UPDATE ON _usage_report_model_usage BEGIN
INSERT INTO _usage_report_cost_totals(thread_id,repository_bucket,model,provider_kind,native_project_id,workstream_id,outcome_id,long_context,complete,input_tokens,cached_input_tokens,cache_write_tokens,output_tokens,total_tokens,reasoning_tokens,uncached_input_tokens,observations,aggregate_overflow)
SELECT thread_id,repository_bucket,model,provider_kind,native_project_id,workstream_id,outcome_id,long_context,complete,input_tokens,cached_input_tokens,cache_write_tokens,output_tokens,total_tokens,reasoning_tokens,uncached_input_tokens,1,0 FROM _usage_report_cost_contributions
WHERE model_request_id = NEW.model_request_id AND source_event_id = NEW.source_event_id AND repository_bucket = NEW.repository_bucket
ON CONFLICT(thread_id,repository_bucket,model,provider_kind,native_project_id,workstream_id,outcome_id,long_context,complete) DO UPDATE SET
 input_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.input_tokens > 9223372036854775807 - excluded.input_tokens THEN _usage_report_cost_totals.input_tokens ELSE _usage_report_cost_totals.input_tokens + excluded.input_tokens END,
 cached_input_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.cached_input_tokens > 9223372036854775807 - excluded.cached_input_tokens THEN _usage_report_cost_totals.cached_input_tokens ELSE _usage_report_cost_totals.cached_input_tokens + excluded.cached_input_tokens END,
 cache_write_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.cache_write_tokens > 9223372036854775807 - excluded.cache_write_tokens THEN _usage_report_cost_totals.cache_write_tokens ELSE _usage_report_cost_totals.cache_write_tokens + excluded.cache_write_tokens END,
 output_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.output_tokens > 9223372036854775807 - excluded.output_tokens THEN _usage_report_cost_totals.output_tokens ELSE _usage_report_cost_totals.output_tokens + excluded.output_tokens END,
 total_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.total_tokens > 9223372036854775807 - excluded.total_tokens THEN _usage_report_cost_totals.total_tokens ELSE _usage_report_cost_totals.total_tokens + excluded.total_tokens END,
 reasoning_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.reasoning_tokens > 9223372036854775807 - excluded.reasoning_tokens THEN _usage_report_cost_totals.reasoning_tokens ELSE _usage_report_cost_totals.reasoning_tokens + excluded.reasoning_tokens END,
 uncached_input_tokens = CASE WHEN _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.uncached_input_tokens > 9223372036854775807 - excluded.uncached_input_tokens THEN _usage_report_cost_totals.uncached_input_tokens ELSE _usage_report_cost_totals.uncached_input_tokens + excluded.uncached_input_tokens END,
 observations = _usage_report_cost_totals.observations + 1,
 aggregate_overflow = _usage_report_cost_totals.aggregate_overflow = 1 OR _usage_report_cost_totals.input_tokens > 9223372036854775807 - excluded.input_tokens OR _usage_report_cost_totals.cached_input_tokens > 9223372036854775807 - excluded.cached_input_tokens OR _usage_report_cost_totals.cache_write_tokens > 9223372036854775807 - excluded.cache_write_tokens OR _usage_report_cost_totals.output_tokens > 9223372036854775807 - excluded.output_tokens OR _usage_report_cost_totals.total_tokens > 9223372036854775807 - excluded.total_tokens OR _usage_report_cost_totals.reasoning_tokens > 9223372036854775807 - excluded.reasoning_tokens OR _usage_report_cost_totals.uncached_input_tokens > 9223372036854775807 - excluded.uncached_input_tokens;
END;
"#;
