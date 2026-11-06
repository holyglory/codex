//! Content-free performance evidence. Missing facts stay explicitly unknown.
use serde_json::Value;
use serde_json::json;

use crate::Result;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::reader_query::query_error;
use crate::report_query::ReportQuery;
use crate::review_storage::query_rows;

impl UsageReader {
    /// Query one exact interval from its indexed operation partitions and token
    /// observation fragments. Timing sums describe overlap, never active time.
    pub fn performance_review(&self, range: TimeRange, filter: &UsageFilter) -> Result<Value> {
        let query = ReportQuery::new(self, range, filter)?;
        let start = range.start_ms();
        let end = range.end_ms();
        let clipped = format!(
            "CASE WHEN started_at_ms IS NOT NULL AND ended_at_ms IS NOT NULL \
             THEN GREATEST(0, LEAST(ended_at_ms::HUGEINT, {end}) - \
             GREATEST(started_at_ms::HUGEINT, {start})) ELSE NULL END"
        );
        let operation_counts = query_rows::<Value>(&query.connection, &format!(
            "SELECT count(*)::UBIGINT operations, \
             count(*) FILTER (WHERE kind = 'model_request')::UBIGINT model_requests, \
             count(*) FILTER (WHERE kind = 'tool')::UBIGINT tools, \
             count(*) FILTER (WHERE kind = 'model_warmup')::UBIGINT warmups, \
             count(*) FILTER (WHERE NOT terminal_observed)::UBIGINT unfinished, \
             count(*) FILTER (WHERE started_at_ms IS NULL OR ended_at_ms IS NULL)::UBIGINT unknown_intervals, \
             count(*) FILTER (WHERE phase IS NULL OR activity IS NULL)::UBIGINT unclassified, \
             count(*) FILTER (WHERE repository_id IS NULL)::UBIGINT without_repository, \
             COALESCE(sum({clipped}),0)::UBIGINT interval_sum_ms FROM ({})", query.operations
        ), 1)?.remove(0);
        let operations = query_rows::<Value>(
            &query.connection,
            &format!(
                "SELECT kind category, \
             count(*)::UBIGINT count, COALESCE(sum({clipped}),0)::UBIGINT \"measuredIntervalSumMs\", \
             count(*) FILTER (WHERE started_at_ms IS NULL OR ended_at_ms IS NULL)::UBIGINT \"unknownIntervals\" \
             FROM ({}) GROUP BY category ORDER BY category",
                query.operations
            ),
            128,
        )?;
        let token_predicate = "event = 'token_observation' OR provider_total_tokens IS NOT NULL OR input_tokens <> 0 OR output_tokens <> 0 OR cached_input_tokens <> 0 OR reasoning_output_tokens <> 0";
        let token_counts = query_rows::<Value>(
            &query.connection,
            &format!(
                "SELECT count(*)::UBIGINT observations, \
             count(*) FILTER (WHERE provider_total_tokens IS NULL)::UBIGINT unknown_totals, \
             COALESCE(sum(provider_total_tokens),0)::UBIGINT provider_total, \
             count(*) FILTER (WHERE coverage IS DISTINCT FROM 'recorded')::UBIGINT incomplete \
             FROM ({}) WHERE {token_predicate}",
                query.contributions
            ),
            1,
        )?
        .remove(0);
        let mut tokens = Vec::new();
        for (category, column) in [
            ("total_tokens", "provider_total_tokens"),
            ("input_tokens", "input_tokens"),
            ("cached_input_tokens", "cached_input_tokens"),
            ("output_tokens", "output_tokens"),
            ("reasoning_output_tokens", "reasoning_output_tokens"),
            ("cache_write_input_tokens", "cache_write_input_tokens"),
        ] {
            let (measured, observations, unknown): (i64, u64, u64) = query.connection.query_row(&format!(
                "SELECT COALESCE(sum({column}),0)::BIGINT, count(*)::UBIGINT, \
                 count(*) FILTER (WHERE {column} IS NULL)::UBIGINT FROM ({}) WHERE {token_predicate}", query.contributions
            ), [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).map_err(query_error)?;
            tokens.push(json!({"category": category, "provenance": "provider_reported",
                "measuredTokens": measured, "exactTokens": (unknown == 0 && observations > 0).then_some(measured),
                "observations": observations, "unknownObservations": unknown}));
        }
        let by_activity = query_rows::<Value>(
            &query.connection,
            &format!(
                "WITH response_links AS (SELECT response_id, min(operation_id) operation_id \
             FROM ({ops}) WHERE response_id IS NOT NULL GROUP BY response_id HAVING count(*) = 1), \
             attributed AS (SELECT f.*, COALESCE(d.operation_id, l.operation_id) linked_operation \
             FROM ({facts}) f LEFT JOIN ({ops}) d ON f.operation_id = d.operation_id \
             LEFT JOIN response_links l ON f.response_id = l.response_id) \
             SELECT COALESCE(c.phase,o.phase,f.phase,'unknown') phase, \
                    COALESCE(c.activity,o.activity,f.activity,'unknown') activity, \
                    COALESCE(c.classification,o.classification,f.classification,'unknown') provenance, \
                    COALESCE(sum(f.provider_total_tokens),0)::UBIGINT \"measuredTokens\", \
                    count(*) FILTER (WHERE f.provider_total_tokens IS NULL)::UBIGINT \"unknownObservations\" \
             FROM attributed f LEFT JOIN ({ops}) o ON f.linked_operation = o.operation_id \
             LEFT JOIN selected_classifications c ON COALESCE(f.linked_operation,f.operation_id) = c.operation_id \
             WHERE f.event = 'token_observation' OR f.provider_total_tokens IS NOT NULL \
             GROUP BY ALL ORDER BY phase,activity,provenance",
                ops = query.operations,
                facts = query.contributions
            ),
            128,
        )?;
        let gaps = query_rows::<Value>(
            &query.connection,
            &format!(
                "SELECT COALESCE(coverage,'unknown') state, count(*)::UBIGINT count, \
             COALESCE(sum(missing_observations),0)::UBIGINT \"missingObservations\" \
             FROM ({}) WHERE event = 'coverage_gap' OR missing_observations IS NOT NULL GROUP BY state ORDER BY state",
                query.contributions
            ),
            32,
        )?;
        let links = query_rows::<Value>(
            &query.connection,
            &format!(
                "SELECT operation_id \"operationId\", started_at_ms \"startedAtMs\", \
             NULL::VARCHAR \"retryOfOperationId\", rework_of_operation_id \"reworkOfOperationId\" \
             FROM ({}) WHERE rework_of_operation_id IS NOT NULL ORDER BY started_at_ms,operation_id LIMIT 17",
                query.operations
            ),
            17,
        )?;
        let rework_operations: u64 = query
            .connection
            .query_row(
                &format!(
                    "SELECT count(*)::UBIGINT FROM ({}) WHERE rework_of_operation_id IS NOT NULL",
                    query.operations
                ),
                [],
                |row| row.get(0),
            )
            .map_err(query_error)?;
        let model_without_total: u64 = query.connection.query_row(&format!(
            "SELECT count(*)::UBIGINT FROM ({}) o WHERE o.kind = 'model_request' AND NOT EXISTS \
             (SELECT 1 FROM ({}) c WHERE c.provider_total_tokens IS NOT NULL AND \
              (c.operation_id = o.operation_id OR (c.response_id IS NOT NULL AND c.response_id = o.response_id)))",
            query.operations, query.contributions
        ), [], |row| row.get(0)).map_err(query_error)?;
        let total_operations = operation_counts["operations"].as_u64().unwrap_or_default();
        let unknown_intervals = operation_counts["unknown_intervals"]
            .as_u64()
            .unwrap_or_default();
        let observed_tokens = token_counts["observations"].as_u64().unwrap_or_default();
        let complete_projection = query.complete_from_sequence == 1;
        let completeness =
            if !complete_projection || self.pending_entries() != 0 || !gaps.is_empty() {
                "partial"
            } else if total_operations == 0 && observed_tokens == 0 {
                "unobserved"
            } else {
                "recorded"
            };
        let measurement = json!({"measured": token_counts["provider_total"],
            "exact": if observed_tokens > 0 && token_counts["unknown_totals"] == 0 { token_counts["provider_total"].clone() } else { Value::Null },
            "unknown": token_counts["unknown_totals"]});
        let effort = json!({"operations": total_operations, "retryOperations": Value::Null, "reworkOperations": rework_operations,
            "providerTotalTokens": measurement, "activeAgentMs": {"measured":0,"exact":Value::Null,"unknown":total_operations},
            "elapsedExecutionMs": {"measured":operation_counts["interval_sum_ms"],"exact":Value::Null,"unknown":unknown_intervals},
            "recordedWaitMs": {"measured":0,"exact":Value::Null,"unknown":total_operations}});
        Ok(json!({
            "schemaVersion":1,"kind":"performanceReview", "window":{"fromAtMs":(start != i64::MIN).then_some(start),"toAtMs":(end != i64::MAX).then_some(end)},
            "tokens":tokens,"tokensByActivity":by_activity,"operations":operations,"counts":operation_counts,"waits":[],
            "links":{"retryOperations":Value::Null,"reworkOperations":rework_operations,"sample":links.into_iter().take(16).collect::<Vec<_>>(),"omittedOperations":rework_operations.saturating_sub(16),"retryCoverage":"unobserved"},
            "candidates":[],"diagnostics":{"longLivedDeclarations":{"thresholdMs":3600000,"count":Value::Null,"coverage":"unobserved"}},
            "coverage":{"rawOperations":total_operations,"deduplicatedOperations":total_operations,"unknownOperationIntervals":unknown_intervals,
                "unclassifiedOperations":operation_counts["unclassified"],"operationsWithoutRepository":operation_counts["without_repository"],
                "unlinkedToolOperations":Value::Null,"modelRequestsWithoutProviderTotal":model_without_total,
                "rawTokenObservations":observed_tokens,"deduplicatedTokenObservations":observed_tokens,
                "unknownTokenObservations":token_counts["unknown_totals"],"incompleteTokenObservations":token_counts["incomplete"],
                "conflictingTokenObservations":Value::Null,"coverageEvents":gaps,"collectionCompleteness":completeness,
                "projectionCompleteFromSequence":query.complete_from_sequence,"criticalPath":"unobserved","repeatedInputComparison":"unobserved","waits":"unobserved"},
            "workBindings":{"references":[],"boundOperations":0,"unknownOperations":total_operations,"basis":"unobserved"},
            "outcomes":{"schemaVersion":1,"coverage":"unobserved","totals":effort,"attributed":Value::Null,"unattributed":effort,
                "unattributedReasons":{"work_binding_unobserved":total_operations},"rows":[],"totalRows":0,"nextCursor":Value::Null,
                "basis":"No outcome binding was captured; operation completion is not proof of a delivered outcome."},
            "evidence":{"action":"details","details":["operations","tokens","coverage","classifications"],"repository":filter.repository_id,
                "thread_id":filter.thread_id,"from_at_ms":(start != i64::MIN).then_some(start),"to_at_ms":(end != i64::MAX).then_some(end),"limit":20},
            "interpretation":"Operation intervals are clipped to the requested UTC window and summed; overlapping work is not elapsed or active time. Token observations use their observation time. Cached input and reasoning output are subsets. Unknown coverage is never a zero measurement."
        }))
    }
}
