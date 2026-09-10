use super::PerformanceReviewQuery;
use sqlx::QueryBuilder;
use sqlx::Sqlite;

pub(super) enum ClassificationSource {
    Cache,
    Canonical,
}

pub(super) fn selection(
    query: &PerformanceReviewQuery,
    source: &ClassificationSource,
) -> QueryBuilder<Sqlite> {
    let mut builder =
        QueryBuilder::new("WITH scoped AS (SELECT operation.* FROM operations operation WHERE 1=1");
    if let Some(thread) = &query.thread_id {
        builder
            .push(" AND operation.thread_id = ")
            .push_bind(thread.as_str());
    }
    if let Some(repository) = &query.repository_id {
        builder.push(" AND operation.id IN (SELECT attribution.operation_id FROM repository_attributions attribution WHERE attribution.repository_id = ")
            .push_bind(repository.as_str()).push(")");
    }
    builder.push("), bounds AS (SELECT ")
        .push_bind(query.time_range.map_or(i64::MIN, super::super::report_math::UtcTimeRange::start_ms))
        .push(" AS lower_ms, ")
        .push_bind(query.time_range.map_or(i64::MAX, super::super::report_math::UtcTimeRange::end_ms))
        .push(" AS upper_ms), selected AS (SELECT operation.*, terminal.occurred_at_ms ended_at_ms, terminal.event_kind terminal_status,
            COALESCE(effective.phase, operation.phase) effective_phase,
            COALESCE(effective.activity, operation.activity) effective_activity,
            COALESCE(effective.activity_state, operation.activity_state) effective_state,");
    builder.push(match source {
        ClassificationSource::Cache => {
            "COALESCE(effective.attribution_provenance, operation.attribution_provenance)"
        }
        ClassificationSource::Canonical => {
            "COALESCE(effective.provenance, operation.attribution_provenance)"
        }
    });
    builder.push(" effective_provenance, tool.execution_role, tool.execution_group_id,
            CASE WHEN terminal.occurred_at_ms IS NOT NULL THEN
              MAX(0, MIN(terminal.occurred_at_ms, upper_ms) - MAX(operation.started_at_ms, lower_ms)) END interval_ms
            FROM scoped operation CROSS JOIN bounds
            LEFT JOIN operation_events terminal ON terminal.operation_id = operation.id AND terminal.terminal = 1
            LEFT JOIN tool_invocations tool ON tool.operation_id = operation.id ");
    builder.push(match source {
        ClassificationSource::Cache => "LEFT JOIN _usage_report_operations effective ON effective.operation_id = operation.id ",
        ClassificationSource::Canonical => "LEFT JOIN effective_classification_events effective ON effective.operation_id = operation.id ",
    });
    builder.push("WHERE operation.started_at_ms < upper_ms AND
        (terminal.occurred_at_ms IS NULL OR terminal.occurred_at_ms > lower_ms OR
         (terminal.occurred_at_ms = operation.started_at_ms AND operation.started_at_ms >= lower_ms))),
        effective AS (SELECT selected.* FROM selected WHERE NOT (
          COALESCE(execution_role, 'standalone') = 'wrapper' AND execution_group_id IS NOT NULL AND EXISTS (
            SELECT 1 FROM selected nested WHERE nested.execution_group_id = selected.execution_group_id
            AND nested.execution_role = 'nested')))");
    builder
}

pub(super) const TOKEN_FACTS: &str = ", token_facts AS (
    SELECT COALESCE(direct.operation_id, covered.operation_id, tool.operation_id) operation_id,
           token.source_event_id, token.category_path, token.measurement_provenance,
           MAX(token.token_count) token_count, COUNT(*) raw_count,
           MAX(token.token_count IS NULL) unknown_count,
           MAX(token.coverage_state <> 'complete') incomplete,
           (MIN(token.token_count) <> MAX(token.token_count) OR
            (COUNT(token.token_count) > 0 AND COUNT(token.token_count) < COUNT(*))) conflict
    FROM token_observations token
    LEFT JOIN model_requests direct ON direct.id = token.model_request_id
    LEFT JOIN tool_invocations tool ON tool.id = token.tool_invocation_id
    LEFT JOIN model_requests covered ON covered.id = tool.covering_model_request_id
    JOIN scoped owner ON owner.id = COALESCE(direct.operation_id, covered.operation_id, tool.operation_id)
    CROSS JOIN bounds
    WHERE token.observed_at_ms >= lower_ms AND token.observed_at_ms < upper_ms
      AND token.category_path NOT GLOB 'attribution.items.*'
    GROUP BY owner.id, token.source_event_id, token.category_path, token.measurement_provenance
)";

pub(super) const OPERATION_COVERAGE: &str = "SELECT
    (SELECT COUNT(*) FROM selected) raw_operations,
    COUNT(*) deduplicated_operations,
    COALESCE(SUM(interval_ms IS NULL), 0) unknown_operation_intervals,
    COALESCE(SUM(effective_provenance = 'unknown'), 0) unclassified_operations,
    COALESCE(SUM(NOT EXISTS (SELECT 1 FROM repository_attributions attribution
      WHERE attribution.operation_id = effective.id AND attribution.repository_id IS NOT NULL)), 0) operations_without_repository,
    COALESCE(SUM(execution_role IN ('wrapper', 'nested') AND execution_group_id IS NULL), 0) unlinked_tool_operations,
    COALESCE(SUM(operation_kind = 'model_request' AND NOT EXISTS (
      SELECT 1 FROM token_facts token WHERE token.operation_id = effective.id
        AND token.category_path = 'total_tokens' AND token.measurement_provenance = 'provider_reported'
        AND token.token_count IS NOT NULL AND COALESCE(token.conflict, 0) = 0)), 0) model_requests_without_provider_total,
    COALESCE(SUM(retry_of_operation_id IS NOT NULL), 0) retry_operations,
    COALESCE(SUM(rework_of_operation_id IS NOT NULL), 0) rework_operations,
    COALESCE(SUM(retry_of_operation_id IS NOT NULL OR rework_of_operation_id IS NOT NULL), 0) linked_operations,
    (SELECT COALESCE(SUM(raw_count), 0) FROM token_facts) raw_token_observations,
    (SELECT COUNT(*) FROM token_facts) deduplicated_token_observations,
    (SELECT COALESCE(SUM(unknown_count), 0) FROM token_facts) unknown_token_observations,
    (SELECT COALESCE(SUM(incomplete), 0) FROM token_facts) incomplete_token_observations,
    (SELECT COALESCE(SUM(conflict), 0) FROM token_facts) conflicting_token_observations,
    (SELECT COUNT(*) FROM coverage_events coverage CROSS JOIN bounds WHERE coverage.operation_id IS NULL
      AND coverage.occurred_at_ms >= lower_ms AND coverage.occurred_at_ms < upper_ms) unattributed_coverage_events_in_window
    FROM effective";

pub(super) const WAIT_CATEGORIES: &str = ", waits AS (
    SELECT effective_state category, interval_ms FROM effective
      WHERE effective_state IN ('user_wait', 'external_wait', 'blocked_wait')
    UNION ALL
    SELECT span.activity_state, CASE WHEN ended.occurred_at_ms IS NOT NULL THEN
      MAX(0, MIN(ended.occurred_at_ms, upper_ms) - MAX(span.started_at_ms, lower_ms)) END
    FROM activity_spans span JOIN effective ON effective.id = span.operation_id CROSS JOIN bounds
    LEFT JOIN activity_span_events ended ON ended.activity_span_id = span.id AND ended.event_kind = 'ended'
    WHERE effective.effective_state NOT IN ('user_wait', 'external_wait', 'blocked_wait')
      AND span.started_at_ms < upper_ms AND (ended.occurred_at_ms IS NULL OR ended.occurred_at_ms > lower_ms)
) SELECT category, COUNT(*) count, COALESCE(SUM(interval_ms), 0) measured_interval_sum_ms,
    SUM(interval_ms IS NULL) unknown_intervals FROM waits GROUP BY category ORDER BY category";
