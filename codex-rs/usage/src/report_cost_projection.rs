//! Compact component receipts retain the provider event boundary needed for pricing.
pub(super) const SCHEMA: &str = r#"
CREATE TABLE _usage_report_model_usage (
 model_request_id TEXT NOT NULL, source_event_id TEXT NOT NULL, repository_bucket TEXT NOT NULL,
 input_tokens INTEGER, input_tokens_at_ms INTEGER,
 cached_input_tokens INTEGER, cached_input_tokens_at_ms INTEGER,
 cache_write_tokens INTEGER, cache_write_tokens_at_ms INTEGER,
 output_tokens INTEGER, output_tokens_at_ms INTEGER,
 total_tokens INTEGER, total_tokens_at_ms INTEGER,
 reasoning_tokens INTEGER, reasoning_tokens_at_ms INTEGER,
 complete_mask INTEGER NOT NULL, PRIMARY KEY(model_request_id, source_event_id, repository_bucket)
) STRICT;
CREATE TRIGGER _usage_report_model_usage_insert AFTER INSERT ON token_observations
WHEN NEW.model_request_id IS NOT NULL AND NEW.measurement_provenance = 'provider_reported' AND NEW.category_path IN ('input_tokens', 'input_tokens_details.cached_tokens', 'input_tokens_details.cache_write_tokens', 'output_tokens', 'total_tokens', 'output_tokens_details.reasoning_tokens')
BEGIN
INSERT INTO _usage_report_model_usage(model_request_id, source_event_id, repository_bucket, input_tokens, cached_input_tokens, cache_write_tokens, output_tokens, total_tokens, reasoning_tokens, input_tokens_at_ms, cached_input_tokens_at_ms, cache_write_tokens_at_ms, output_tokens_at_ms, total_tokens_at_ms, reasoning_tokens_at_ms, complete_mask)
SELECT NEW.model_request_id,
 NEW.source_event_id,
 NEW.repository_bucket,
 CASE WHEN NEW.category_path = 'input_tokens' THEN NEW.token_count END,
 CASE WHEN NEW.category_path = 'input_tokens_details.cached_tokens' THEN NEW.token_count END,
 CASE WHEN NEW.category_path = 'input_tokens_details.cache_write_tokens' THEN NEW.token_count END,
 CASE WHEN NEW.category_path = 'output_tokens' THEN NEW.token_count END,
 CASE WHEN NEW.category_path = 'total_tokens' THEN NEW.token_count END,
 CASE WHEN NEW.category_path = 'output_tokens_details.reasoning_tokens' THEN NEW.token_count END,
 CASE WHEN NEW.category_path = 'input_tokens' THEN NEW.observed_at_ms END,
 CASE WHEN NEW.category_path = 'input_tokens_details.cached_tokens' THEN NEW.observed_at_ms END,
 CASE WHEN NEW.category_path = 'input_tokens_details.cache_write_tokens' THEN NEW.observed_at_ms END,
 CASE WHEN NEW.category_path = 'output_tokens' THEN NEW.observed_at_ms END,
 CASE WHEN NEW.category_path = 'total_tokens' THEN NEW.observed_at_ms END,
 CASE WHEN NEW.category_path = 'output_tokens_details.reasoning_tokens' THEN NEW.observed_at_ms END,
 CASE WHEN NEW.token_count IS NOT NULL AND NEW.coverage_state = 'complete' THEN (CASE NEW.category_path WHEN 'input_tokens' THEN 1 WHEN 'input_tokens_details.cached_tokens' THEN 2 WHEN 'input_tokens_details.cache_write_tokens' THEN 4 WHEN 'output_tokens' THEN 8 WHEN 'total_tokens' THEN 16 WHEN 'output_tokens_details.reasoning_tokens' THEN 32 ELSE 0 END) ELSE 0 END
WHERE 1 = 1
ON CONFLICT(model_request_id, source_event_id, repository_bucket) DO UPDATE SET
 input_tokens = COALESCE(excluded.input_tokens, _usage_report_model_usage.input_tokens),
 cached_input_tokens = COALESCE(excluded.cached_input_tokens, _usage_report_model_usage.cached_input_tokens),
 cache_write_tokens = COALESCE(excluded.cache_write_tokens, _usage_report_model_usage.cache_write_tokens),
 output_tokens = COALESCE(excluded.output_tokens, _usage_report_model_usage.output_tokens),
 total_tokens = COALESCE(excluded.total_tokens, _usage_report_model_usage.total_tokens),
 reasoning_tokens = COALESCE(excluded.reasoning_tokens, _usage_report_model_usage.reasoning_tokens),
 input_tokens_at_ms = COALESCE(excluded.input_tokens_at_ms, _usage_report_model_usage.input_tokens_at_ms),
 cached_input_tokens_at_ms = COALESCE(excluded.cached_input_tokens_at_ms, _usage_report_model_usage.cached_input_tokens_at_ms),
 cache_write_tokens_at_ms = COALESCE(excluded.cache_write_tokens_at_ms, _usage_report_model_usage.cache_write_tokens_at_ms),
 output_tokens_at_ms = COALESCE(excluded.output_tokens_at_ms, _usage_report_model_usage.output_tokens_at_ms),
 total_tokens_at_ms = COALESCE(excluded.total_tokens_at_ms, _usage_report_model_usage.total_tokens_at_ms),
 reasoning_tokens_at_ms = COALESCE(excluded.reasoning_tokens_at_ms, _usage_report_model_usage.reasoning_tokens_at_ms),
 complete_mask = _usage_report_model_usage.complete_mask | excluded.complete_mask;
END;
"#;

pub(super) const BACKFILL: &str = r#"
INSERT INTO _usage_report_model_usage(model_request_id, source_event_id, repository_bucket, input_tokens, cached_input_tokens, cache_write_tokens, output_tokens, total_tokens, reasoning_tokens, input_tokens_at_ms, cached_input_tokens_at_ms, cache_write_tokens_at_ms, output_tokens_at_ms, total_tokens_at_ms, reasoning_tokens_at_ms, complete_mask)
SELECT token.model_request_id,
 token.source_event_id,
 token.repository_bucket,
 CASE WHEN token.category_path = 'input_tokens' THEN token.token_count END,
 CASE WHEN token.category_path = 'input_tokens_details.cached_tokens' THEN token.token_count END,
 CASE WHEN token.category_path = 'input_tokens_details.cache_write_tokens' THEN token.token_count END,
 CASE WHEN token.category_path = 'output_tokens' THEN token.token_count END,
 CASE WHEN token.category_path = 'total_tokens' THEN token.token_count END,
 CASE WHEN token.category_path = 'output_tokens_details.reasoning_tokens' THEN token.token_count END,
 CASE WHEN token.category_path = 'input_tokens' THEN token.observed_at_ms END,
 CASE WHEN token.category_path = 'input_tokens_details.cached_tokens' THEN token.observed_at_ms END,
 CASE WHEN token.category_path = 'input_tokens_details.cache_write_tokens' THEN token.observed_at_ms END,
 CASE WHEN token.category_path = 'output_tokens' THEN token.observed_at_ms END,
 CASE WHEN token.category_path = 'total_tokens' THEN token.observed_at_ms END,
 CASE WHEN token.category_path = 'output_tokens_details.reasoning_tokens' THEN token.observed_at_ms END,
 CASE WHEN token.token_count IS NOT NULL AND token.coverage_state = 'complete' THEN (CASE token.category_path WHEN 'input_tokens' THEN 1 WHEN 'input_tokens_details.cached_tokens' THEN 2 WHEN 'input_tokens_details.cache_write_tokens' THEN 4 WHEN 'output_tokens' THEN 8 WHEN 'total_tokens' THEN 16 WHEN 'output_tokens_details.reasoning_tokens' THEN 32 ELSE 0 END) ELSE 0 END
FROM token_observations AS token WHERE token.rowid > ?1 AND token.rowid <= ?2 AND token.model_request_id IS NOT NULL AND token.measurement_provenance = 'provider_reported' AND token.category_path IN ('input_tokens', 'input_tokens_details.cached_tokens', 'input_tokens_details.cache_write_tokens', 'output_tokens', 'total_tokens', 'output_tokens_details.reasoning_tokens')
ON CONFLICT(model_request_id, source_event_id, repository_bucket) DO UPDATE SET
 input_tokens = COALESCE(excluded.input_tokens, _usage_report_model_usage.input_tokens),
 cached_input_tokens = COALESCE(excluded.cached_input_tokens, _usage_report_model_usage.cached_input_tokens),
 cache_write_tokens = COALESCE(excluded.cache_write_tokens, _usage_report_model_usage.cache_write_tokens),
 output_tokens = COALESCE(excluded.output_tokens, _usage_report_model_usage.output_tokens),
 total_tokens = COALESCE(excluded.total_tokens, _usage_report_model_usage.total_tokens),
 reasoning_tokens = COALESCE(excluded.reasoning_tokens, _usage_report_model_usage.reasoning_tokens),
 input_tokens_at_ms = COALESCE(excluded.input_tokens_at_ms, _usage_report_model_usage.input_tokens_at_ms),
 cached_input_tokens_at_ms = COALESCE(excluded.cached_input_tokens_at_ms, _usage_report_model_usage.cached_input_tokens_at_ms),
 cache_write_tokens_at_ms = COALESCE(excluded.cache_write_tokens_at_ms, _usage_report_model_usage.cache_write_tokens_at_ms),
 output_tokens_at_ms = COALESCE(excluded.output_tokens_at_ms, _usage_report_model_usage.output_tokens_at_ms),
 total_tokens_at_ms = COALESCE(excluded.total_tokens_at_ms, _usage_report_model_usage.total_tokens_at_ms),
 reasoning_tokens_at_ms = COALESCE(excluded.reasoning_tokens_at_ms, _usage_report_model_usage.reasoning_tokens_at_ms),
 complete_mask = _usage_report_model_usage.complete_mask | excluded.complete_mask;
"#;
