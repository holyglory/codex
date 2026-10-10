use crate::UsageApiEquivalentCost;
use crate::UsageStoreError;
use crate::UsageSummaryQuery;
use sqlx::QueryBuilder;
use sqlx::Sqlite;
use sqlx::SqliteConnection;
use sqlx::types::Json;
use std::collections::HashSet;

const CANONICAL: &str = r#"
CREATE TEMP TABLE _usage_cost_receipts AS
SELECT request.id AS model_request_id, token.source_event_id, token.repository_bucket,
MAX(CASE WHEN token.category_path = 'input_tokens' THEN token.token_count END) AS input_tokens,
MAX(CASE WHEN token.category_path = 'input_tokens' THEN token.observed_at_ms END) AS input_tokens_at_ms,
MAX(CASE WHEN token.category_path = 'input_tokens_details.cached_tokens' THEN token.token_count END) AS cached_input_tokens,
MAX(CASE WHEN token.category_path = 'input_tokens_details.cached_tokens' THEN token.observed_at_ms END) AS cached_input_tokens_at_ms,
MAX(CASE WHEN token.category_path = 'input_tokens_details.cache_write_tokens' THEN token.token_count END) AS cache_write_tokens,
MAX(CASE WHEN token.category_path = 'input_tokens_details.cache_write_tokens' THEN token.observed_at_ms END) AS cache_write_tokens_at_ms,
MAX(CASE WHEN token.category_path = 'output_tokens' THEN token.token_count END) AS output_tokens,
MAX(CASE WHEN token.category_path = 'output_tokens' THEN token.observed_at_ms END) AS output_tokens_at_ms,
MAX(CASE WHEN token.category_path = 'total_tokens' THEN token.token_count END) AS total_tokens,
MAX(CASE WHEN token.category_path = 'total_tokens' THEN token.observed_at_ms END) AS total_tokens_at_ms,
MAX(CASE WHEN token.category_path = 'output_tokens_details.reasoning_tokens' THEN token.token_count END) AS reasoning_tokens,
MAX(CASE WHEN token.category_path = 'output_tokens_details.reasoning_tokens' THEN token.observed_at_ms END) AS reasoning_tokens_at_ms,
SUM(CASE WHEN token.coverage_state = 'complete' AND token.token_count IS NOT NULL THEN CASE token.category_path WHEN 'input_tokens' THEN 1 WHEN 'input_tokens_details.cached_tokens' THEN 2 WHEN 'input_tokens_details.cache_write_tokens' THEN 4 WHEN 'output_tokens' THEN 8 WHEN 'total_tokens' THEN 16 WHEN 'output_tokens_details.reasoning_tokens' THEN 32 ELSE 0 END ELSE 0 END) AS complete_mask
FROM _usage_selected AS selected JOIN model_requests AS request ON request.operation_id = selected.id
JOIN token_observations AS token ON token.model_request_id = request.id
WHERE token.measurement_provenance = 'provider_reported' AND token.category_path IN ('input_tokens','input_tokens_details.cached_tokens','input_tokens_details.cache_write_tokens','output_tokens','total_tokens','output_tokens_details.reasoning_tokens')
GROUP BY request.id, token.source_event_id, token.repository_bucket;
CREATE INDEX temp._usage_cost_receipts_request ON _usage_cost_receipts(model_request_id);
"#;

const SELECT: &str = r#"
normalized AS (SELECT COALESCE(request.model,'') AS model, COALESCE(request.provider_kind,'') AS provider_kind, receipt.complete_mask,
CASE WHEN (?1 IS NULL OR receipt.input_tokens_at_ms >= ?1) AND (?2 IS NULL OR receipt.input_tokens_at_ms < ?2) THEN receipt.input_tokens END AS input_tokens,
CASE WHEN (?1 IS NULL OR receipt.cached_input_tokens_at_ms >= ?1) AND (?2 IS NULL OR receipt.cached_input_tokens_at_ms < ?2) THEN receipt.cached_input_tokens END AS cached_input_tokens,
CASE WHEN (?1 IS NULL OR receipt.cache_write_tokens_at_ms >= ?1) AND (?2 IS NULL OR receipt.cache_write_tokens_at_ms < ?2) THEN receipt.cache_write_tokens END AS cache_write_tokens,
CASE WHEN (?1 IS NULL OR receipt.output_tokens_at_ms >= ?1) AND (?2 IS NULL OR receipt.output_tokens_at_ms < ?2) THEN receipt.output_tokens END AS output_tokens,
CASE WHEN (?1 IS NULL OR receipt.total_tokens_at_ms >= ?1) AND (?2 IS NULL OR receipt.total_tokens_at_ms < ?2) THEN receipt.total_tokens END AS total_tokens,
CASE WHEN (?1 IS NULL OR receipt.reasoning_tokens_at_ms >= ?1) AND (?2 IS NULL OR receipt.reasoning_tokens_at_ms < ?2) THEN receipt.reasoning_tokens END AS reasoning_tokens
FROM _usage_selected AS selected LEFT JOIN model_requests AS request ON request.operation_id = selected.id
LEFT JOIN "#;

const GROUP: &str = r#"
), classified AS (
 SELECT *, COALESCE(input_tokens > 272000, 0) AS long_context,
   COALESCE((complete_mask & 31) = 31 AND input_tokens IS NOT NULL AND cached_input_tokens IS NOT NULL
     AND cache_write_tokens IS NOT NULL AND output_tokens IS NOT NULL AND total_tokens IS NOT NULL
     AND cached_input_tokens <= input_tokens AND cache_write_tokens <= input_tokens - cached_input_tokens
     AND (reasoning_tokens IS NULL OR reasoning_tokens <= output_tokens), 0) AS complete
 FROM normalized
)
SELECT model, provider_kind, long_context, complete, COUNT(*) AS observations,
COALESCE(SUM(input_tokens),0) AS input_tokens,
COALESCE(SUM(cached_input_tokens),0) AS cached_input_tokens,
COALESCE(SUM(cache_write_tokens),0) AS cache_write_tokens,
COALESCE(SUM(output_tokens),0) AS output_tokens,
COALESCE(SUM(total_tokens),0) AS total_tokens,
COALESCE(SUM(reasoning_tokens),0) AS reasoning_tokens,
 COALESCE(SUM(CASE WHEN input_tokens >= cached_input_tokens AND input_tokens - cached_input_tokens >= cache_write_tokens THEN input_tokens - cached_input_tokens - cache_write_tokens END),0) AS uncached_input_tokens
FROM classified GROUP BY model, provider_kind, long_context, complete
"#;

const ROLLUPS: &str = r#"
SELECT model,provider_kind,long_context,complete,SUM(observations) AS observations,
 SUM(input_tokens) AS input_tokens,SUM(cached_input_tokens) AS cached_input_tokens,
 SUM(cache_write_tokens) AS cache_write_tokens,SUM(output_tokens) AS output_tokens,
 SUM(total_tokens) AS total_tokens,SUM(reasoning_tokens) AS reasoning_tokens,
 SUM(uncached_input_tokens) AS uncached_input_tokens
FROM _usage_report_cost_totals WHERE (?3 IS NULL OR thread_id = ?3)
GROUP BY model,provider_kind,long_context,complete
UNION ALL SELECT COALESCE(request.model,''), COALESCE(request.provider_kind,''), 0, 0, COUNT(*),0,0,0,0,0,0,0
FROM _usage_selected AS selected LEFT JOIN model_requests AS request ON request.operation_id = selected.id
WHERE selected.operation_kind = 'model_request' AND NOT EXISTS
 (SELECT 1 FROM _usage_report_model_usage AS receipt WHERE receipt.model_request_id = request.id)
GROUP BY request.model,request.provider_kind
"#;

const TOOLS: &str = r#"
UNION ALL SELECT '', 'unpriced_tool', 0, 0, COUNT(*),
COALESCE(SUM(input_tokens),0),
COALESCE(SUM(cached_input_tokens),0),
COALESCE(SUM(cache_write_tokens),0),
COALESCE(SUM(output_tokens),0),
COALESCE(SUM(total_tokens),0),
COALESCE(SUM(reasoning_tokens),0),
COALESCE(SUM(CASE WHEN input_tokens >= cached_input_tokens AND input_tokens - cached_input_tokens >= cache_write_tokens THEN input_tokens - cached_input_tokens - cache_write_tokens END),0)
FROM (SELECT tool.id, token.source_event_id, token.repository_bucket,
MAX(CASE WHEN token.category_path = 'input_tokens' THEN token.token_count END) AS input_tokens,
MAX(CASE WHEN token.category_path = 'input_tokens_details.cached_tokens' THEN token.token_count END) AS cached_input_tokens,
MAX(CASE WHEN token.category_path = 'input_tokens_details.cache_write_tokens' THEN token.token_count END) AS cache_write_tokens,
MAX(CASE WHEN token.category_path = 'output_tokens' THEN token.token_count END) AS output_tokens,
MAX(CASE WHEN token.category_path = 'total_tokens' THEN token.token_count END) AS total_tokens,
MAX(CASE WHEN token.category_path = 'output_tokens_details.reasoning_tokens' THEN token.token_count END) AS reasoning_tokens
FROM _usage_selected AS selected JOIN tool_invocations AS tool ON tool.operation_id = selected.id
JOIN token_observations AS token ON token.tool_invocation_id = tool.id
WHERE token.category_path NOT GLOB 'attribution.items.*'
AND (?1 IS NULL OR token.observed_at_ms >= ?1) AND (?2 IS NULL OR token.observed_at_ms < ?2)
AND NOT EXISTS (SELECT 1 FROM "#;

const COVERED: &str = r#" AS parent_receipt JOIN model_requests AS parent ON parent.id = parent_receipt.model_request_id
JOIN _usage_selected AS parent_selected ON parent_selected.id = parent.operation_id
WHERE parent.id = tool.covering_model_request_id AND parent_receipt.source_event_id = token.source_event_id
AND parent_receipt.repository_bucket = token.repository_bucket AND token.measurement_provenance = 'provider_reported'
AND parent_receipt.total_tokens = (SELECT total.token_count FROM token_observations AS total
    WHERE total.tool_invocation_id = tool.id AND total.source_event_id = token.source_event_id
    AND total.category_path = 'total_tokens' AND total.measurement_provenance = 'provider_reported')
AND (?1 IS NULL OR parent_receipt.total_tokens_at_ms >= ?1) AND (?2 IS NULL OR parent_receipt.total_tokens_at_ms < ?2))
"#;

pub(super) async fn cost(
    connection: &mut SqliteConnection,
    query: &UsageSummaryQuery,
    family: Option<&HashSet<String>>,
    source: super::ReportSource,
) -> Result<UsageApiEquivalentCost, UsageStoreError> {
    sqlx::query("DROP TABLE IF EXISTS temp._usage_cost_receipts")
        .execute(&mut *connection)
        .await
        .map_err(super::database_error)?;
    let table = match source {
        super::ReportSource::Canonical => {
            sqlx::raw_sql(CANONICAL)
                .execute(&mut *connection)
                .await
                .map_err(super::database_error)?;
            "_usage_cost_receipts"
        }
        super::ReportSource::CachedAll | super::ReportSource::CachedScoped => {
            "_usage_report_model_usage"
        }
    };
    let compact = source != super::ReportSource::Canonical
        && query.time_range.is_none()
        && family.is_none()
        && query.account_profile_ref.is_none();
    let mut sql = QueryBuilder::<Sqlite>::new("WITH bounds AS (SELECT ");
    sql.push_bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .push(" AS start_ms, ")
        .push_bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .push(" AS end_ms");
    if compact {
        let overflow: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM _usage_report_cost_totals WHERE aggregate_overflow = 1 AND (?1 IS NULL OR thread_id = ?1))")
            .bind(query.thread_id.as_ref().map(crate::ThreadId::as_str)).fetch_one(&mut *connection).await.map_err(super::database_error)?;
        if overflow {
            return Err(UsageStoreError::AggregateOverflow);
        }
        sql.push(", ")
            .push_bind(query.thread_id.as_ref().map(crate::ThreadId::as_str))
            .push(" AS thread_id) ")
            .push(ROLLUPS);
    } else {
        sql.push("), ");
        sql.push(SELECT)
            .push(table)
            .push(" AS receipt ON receipt.model_request_id = request.id");
        if let Some(family) = family {
            sql.push(" AND receipt.repository_bucket IN (SELECT value FROM json_each(")
                .push_bind(Json(family.iter().collect::<Vec<_>>()))
                .push("))");
        }
        if query.time_range.is_some() {
            sql.push(" AND (");
            sql.push("(receipt.input_tokens_at_ms >= ?1 AND receipt.input_tokens_at_ms < ?2)");
            sql.push(" OR ");
            sql.push("(receipt.cached_input_tokens_at_ms >= ?1 AND receipt.cached_input_tokens_at_ms < ?2)");
            sql.push(" OR ");
            sql.push("(receipt.cache_write_tokens_at_ms >= ?1 AND receipt.cache_write_tokens_at_ms < ?2)");
            sql.push(" OR ");
            sql.push("(receipt.output_tokens_at_ms >= ?1 AND receipt.output_tokens_at_ms < ?2)");
            sql.push(" OR ");
            sql.push("(receipt.total_tokens_at_ms >= ?1 AND receipt.total_tokens_at_ms < ?2)");
            sql.push(" OR ");
            sql.push(
                "(receipt.reasoning_tokens_at_ms >= ?1 AND receipt.reasoning_tokens_at_ms < ?2)",
            );
            sql.push(")");
        }
        sql.push(" WHERE selected.operation_kind = 'model_request'");
        sql.push(GROUP);
    }
    sql.push(TOOLS).push(table).push(COVERED);
    if let Some(family) = family {
        sql.push(" AND token.repository_bucket IN (SELECT value FROM json_each(")
            .push_bind(Json(family.iter().collect::<Vec<_>>()))
            .push("))");
    }
    sql.push(" GROUP BY tool.id, token.source_event_id, token.repository_bucket) HAVING COUNT(*) > 0 ORDER BY model, provider_kind, long_context, complete LIMIT 16385");
    let rows = sql
        .build()
        .fetch_all(connection)
        .await
        .map_err(super::database_error)?;
    if rows.len() > super::MAX_GROUPS {
        return Err(UsageStoreError::ReportTooLarge);
    }
    crate::report_cost::aggregate(rows)
}
