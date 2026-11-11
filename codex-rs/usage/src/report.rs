//! Content-free performance evidence. Missing facts stay explicitly unknown.
use serde_json::Value;
use serde_json::json;

use crate::Result;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::reader_query::query_error;
use crate::report_facets::operation_facet;
use crate::report_query::ReportQuery;
use crate::review_storage::query_rows;

impl UsageReader {
    /// Query one exact interval from its indexed operation partitions and token
    /// observation fragments. Timing sums describe overlap, never active time.
    pub fn performance_review(&self, range: TimeRange, filter: &UsageFilter) -> Result<Value> {
        let query = ReportQuery::new(self, range, filter)?;
        let start = range.start_ms();
        let end = range.end_ms();
        let exact_token_window = self.manifest.unapplied_retention_corrections == 0
            && self.manifest.unapplied_retention_records == 0
            && (range == TimeRange::all() || self.manifest.retired_review_contributions == 0);
        let operation_counts = query_rows::<Value>(&query.connection, &format!(
            "SELECT COALESCE(sum(operation_count),0)::UBIGINT operations, \
             COALESCE(sum(operation_count) FILTER (WHERE kind = 'model_request'),0)::UBIGINT model_requests, \
             COALESCE(sum(operation_count) FILTER (WHERE kind = 'tool'),0)::UBIGINT tools, \
             COALESCE(sum(operation_count) FILTER (WHERE kind = 'model_warmup'),0)::UBIGINT warmups, \
             COALESCE(sum(unfinished),0)::UBIGINT unfinished, \
             COALESCE(sum(unknown_intervals),0)::UBIGINT unknown_intervals, \
             COALESCE(sum(operation_count) FILTER (WHERE phase IS NULL OR activity IS NULL),0)::UBIGINT unclassified, \
             COALESCE(sum(operation_count) FILTER (WHERE repository_id IS NULL),0)::UBIGINT without_repository, \
             COALESCE(sum(interval_sum_ms),0)::UBIGINT interval_sum_ms FROM ({}) WHERE kind <> 'wait'", query.operation_statistics
        ), 1)?.remove(0);
        let operations = query_rows::<Value>(
            &query.connection,
            &format!(
                "SELECT kind category, \
             sum(operation_count)::UBIGINT count, COALESCE(sum(interval_sum_ms),0)::UBIGINT \"measuredIntervalSumMs\", \
             sum(unknown_intervals)::UBIGINT \"unknownIntervals\" \
             FROM ({}) WHERE kind <> 'wait' GROUP BY category ORDER BY category",
                query.operation_statistics
            ),
            128,
        )?;
        let by_operation_activity = operation_facet(
            &query.connection,
            &format!(
                "SELECT COALESCE(phase,'unknown') phase, COALESCE(activity,'unknown') activity, \
                 COALESCE(classification,'unknown') provenance, sum(operation_count)::UBIGINT count, \
                 sum(interval_sum_ms)::UBIGINT \"measuredIntervalSumMs\", \
                 sum(unknown_intervals)::UBIGINT \"unknownIntervals\" \
                 FROM ({}) WHERE kind <> 'wait' GROUP BY ALL ORDER BY phase,activity,provenance",
                query.operation_statistics
            ),
            &["phase", "activity", "provenance"],
        )?;
        let tools = operation_facet(
            &query.connection,
            &format!(
                "SELECT COALESCE(tool,'unknown') tool, COALESCE(outcome,'unknown') outcome, \
                 sum(operation_count)::UBIGINT count, sum(interval_sum_ms)::UBIGINT \"measuredIntervalSumMs\", \
                 sum(unknown_intervals)::UBIGINT \"unknownIntervals\" FROM ({}) \
                 WHERE kind='tool' GROUP BY ALL ORDER BY tool,outcome",
                query.operation_statistics
            ),
            &["tool", "outcome"],
        )?;
        let token_predicate = crate::review_aggregate::TOKEN_PREDICATE;
        let token_counts = query_rows::<Value>(
            &query.connection,
            &format!(
                "SELECT COALESCE(sum(COALESCE(observation_count, 1)),0)::UBIGINT observations, \
             COALESCE(sum(CASE WHEN provider_total_unknowns IS NOT NULL THEN provider_total_unknowns \
                              WHEN provider_total_tokens IS NULL THEN COALESCE(observation_count,1) ELSE 0 END),0)::UBIGINT unknown_totals, \
             COALESCE(sum(provider_total_tokens),0)::UBIGINT provider_total, \
             COALESCE(sum(CASE WHEN coverage IS DISTINCT FROM 'recorded' THEN COALESCE(observation_count,1) ELSE 0 END),0)::UBIGINT incomplete \
             FROM ({}) WHERE {token_predicate}",
                query.contributions
            ),
            1,
        )?
        .remove(0);
        let mut tokens = Vec::new();
        for (category, column, unknown_column) in [
            (
                "total_tokens",
                "provider_total_tokens",
                "provider_total_unknowns",
            ),
            ("input_tokens", "input_tokens", "NULL"),
            ("cached_input_tokens", "cached_input_tokens", "NULL"),
            ("output_tokens", "output_tokens", "NULL"),
            ("reasoning_output_tokens", "reasoning_output_tokens", "NULL"),
            (
                "cache_write_input_tokens",
                "cache_write_input_tokens",
                "cache_write_unknowns",
            ),
        ] {
            let (measured, observations, unknown): (i64, u64, u64) = query.connection.query_row(&format!(
                "SELECT COALESCE(sum({column}),0)::BIGINT, \
                 COALESCE(sum(COALESCE(observation_count,1)),0)::UBIGINT, \
                 COALESCE(sum(COALESCE({unknown_column}, CASE WHEN {column} IS NULL THEN COALESCE(observation_count,1) ELSE 0 END)),0)::UBIGINT \
                 FROM ({}) WHERE {token_predicate}", query.contributions
            ), [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).map_err(query_error)?;
            tokens.push(json!({"category": category, "provenance": "provider_reported",
                "measuredTokens": measured, "exactTokens": (exact_token_window && unknown == 0 && observations > 0).then_some(measured),
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
                    COALESCE(sum(CASE WHEN f.provider_total_unknowns IS NOT NULL THEN f.provider_total_unknowns \
                                      WHEN f.provider_total_tokens IS NULL THEN COALESCE(f.observation_count,1) ELSE 0 END),0)::UBIGINT \"unknownObservations\" \
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
                "SELECT COALESCE(coverage,'unknown') state, \
             COALESCE(sum(COALESCE(observation_count,1)),0)::UBIGINT count, \
             COALESCE(sum(COALESCE(missing_observations_sum, missing_observations, 0)),0)::UBIGINT \"missingObservations\" \
             FROM ({}) WHERE event = 'coverage_gap' OR missing_observations IS NOT NULL OR missing_observations_sum IS NOT NULL GROUP BY state ORDER BY state",
                query.contributions
            ),
            32,
        )?;
        let retries = crate::report_retries::RetryEvidence::read(&query)?;
        let links = &retries.links;
        let rework_operations: u64 = query
            .connection
            .query_row(
                &format!(
                    "SELECT COALESCE(sum(rework_operations),0)::UBIGINT FROM ({})",
                    query.operation_statistics
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
        let detail_operations: u64 = query
            .connection
            .query_row(
                &format!(
                    "SELECT count(*)::UBIGINT FROM ({}) WHERE kind <> 'wait'",
                    query.operations
                ),
                [],
                |row| row.get(0),
            )
            .map_err(query_error)?;
        let retained_operations = total_operations.saturating_sub(detail_operations);
        let omitted_link_operations = retries
            .linked_operations
            .saturating_sub(links.len().min(16) as u64);
        let unknown_intervals = operation_counts["unknown_intervals"]
            .as_u64()
            .unwrap_or_default();
        let observed_tokens = token_counts["observations"].as_u64().unwrap_or_default();
        let complete_projection = query.complete_from_sequence == 1;
        let completeness = if !complete_projection
            || self.pending_entries() != 0
            || !gaps.is_empty()
            || self.manifest.retired_observations != 0
            || self.manifest.retired_review_operations != 0
            || self.manifest.retired_review_contributions != 0
            || self.manifest.unapplied_retention_corrections != 0
            || self.manifest.unapplied_retention_records != 0
        {
            "partial"
        } else if total_operations == 0 && observed_tokens == 0 && retries.waits.is_empty() {
            "unobserved"
        } else {
            "recorded"
        };
        let measurement = json!({"measured": token_counts["provider_total"],
            "exact": if exact_token_window && observed_tokens > 0 && token_counts["unknown_totals"] == 0 { token_counts["provider_total"].clone() } else { Value::Null },
            "unknown": token_counts["unknown_totals"]});
        let retry_operations =
            (retries.attempts > 0 || retries.retries > 0).then_some(retries.retries);
        let unknown_attempt_operations = operation_counts["model_requests"]
            .as_u64()
            .unwrap_or_default()
            .saturating_sub(retries.attempts);
        let retry_coverage = if retry_operations.is_some() {
            "partial"
        } else {
            "unobserved"
        };
        let effort = json!({"operations": total_operations, "retryOperations": retry_operations, "reworkOperations": rework_operations,
            "providerTotalTokens": measurement, "activeAgentMs": {"measured":0,"exact":Value::Null,"unknown":total_operations},
            "elapsedExecutionMs": {"measured":operation_counts["interval_sum_ms"],"exact":Value::Null,"unknown":unknown_intervals},
            "recordedWaitMs": {"measured":retries.wait_ms,"exact":Value::Null,"unknown":Value::Null}});
        let mut report = json!({
            "schemaVersion":1,"kind":"performanceReview", "window":{"fromAtMs":(start != i64::MIN).then_some(start),"toAtMs":(end != i64::MAX).then_some(end)},
            "tokens":tokens,"tokensByActivity":by_activity,"operations":operations,
            "operationsByActivity":by_operation_activity,"tools":tools,"counts":operation_counts,"waits":retries.waits,
            "links":{"retryOperations":retry_operations,"observedAttempts":retries.attempts,"unknownAttemptOperations":unknown_attempt_operations,"reworkOperations":rework_operations,"sample":links.iter().take(16).collect::<Vec<_>>(),"omittedOperations":omitted_link_operations,"retryCoverage":retry_coverage,"observedBoundaries":["responses_http_request","responses_websocket_request","http_retry_wait","stream_retry_wait","compaction_retry_wait"],"unobservedBoundaries":["websocket_connection","auth_refresh","non_responses_endpoints"]},
            "candidates":[],"diagnostics":{"longLivedDeclarations":{"thresholdMs":3600000,"count":Value::Null,"coverage":"unobserved"}},
            "coverage":{"rawOperations":detail_operations,"deduplicatedOperations":total_operations,"unknownOperationIntervals":unknown_intervals,
                "retainedAggregateOperations":retained_operations,
                "operationStatistics":if self.manifest.unapplied_retention_records != 0 || range != TimeRange::all() && self.manifest.retired_review_operations != 0 {"partial_retention"} else {"recorded"},
                "reviewDetail":if self.manifest.review_retention_cutoff_at_ms.is_some() {"partial_retention"} else {"recorded"},
                "tokenTotals":if exact_token_window {"recorded"} else {"partial_retention"},
                "unclassifiedOperations":operation_counts["unclassified"],"operationsWithoutRepository":operation_counts["without_repository"],
                "unlinkedToolOperations":Value::Null,"modelRequestsWithoutProviderTotal":if retained_operations == 0 {Some(model_without_total)} else {None},
                "rawTokenObservations":observed_tokens,"deduplicatedTokenObservations":observed_tokens,
                "unknownTokenObservations":token_counts["unknown_totals"],"incompleteTokenObservations":token_counts["incomplete"],
                "conflictingTokenObservations":Value::Null,"coverageEvents":gaps,"collectionCompleteness":completeness,
                "projectionCompleteFromSequence":query.complete_from_sequence,
                "unappliedCorrectionsAfterRetention":self.manifest.unapplied_retention_corrections,
                "unappliedObservationsAfterRetention":self.manifest.unapplied_retention_records,
                "criticalPath":"unobserved","repeatedInputComparison":"unobserved","waits":"partial"},
            "workBindings":{"references":[],"boundOperations":0,"unknownOperations":total_operations,"basis":"unobserved"},
            "outcomes":{"schemaVersion":1,"coverage":"unobserved","totals":effort,"attributed":Value::Null,"unattributed":effort,
                "unattributedReasons":{"work_binding_unobserved":total_operations},"rows":[],"totalRows":0,"nextCursor":Value::Null,
                "basis":"No outcome binding was captured; operation completion is not proof of a delivered outcome."},
            "evidence":{"action":"details","details":["operations","tokens","coverage","classifications"],"repository":filter.repository_id,
                "thread_id":filter.thread_id,"from_at_ms":(start != i64::MIN).then_some(start),"to_at_ms":(end != i64::MAX).then_some(end),"limit":20},
            "interpretation":"Retained statistics preserve all-time counts and interval sums; finite windows can be partial after timing detail is retired. Retained hourly operations use their completion hour; inactive chats retain lifetime totals. Available execution intervals are clipped to the requested UTC window and summed; overlapping work is not elapsed or active time. Retry waits are measured separately and excluded from execution counts and sums. Token observations use their observation time. Cached input and reasoning output are subsets. Unknown coverage is never a zero measurement."
        });
        if !self.work_binding_transitions().is_empty() {
            let sparse = self.sparse_bound_effort(
                range,
                filter,
                self.work_binding_transitions(),
                &Default::default(),
            )?;
            crate::work_binding_report::apply_sparse_report(&mut report, &sparse)?;
        }
        Ok(report)
    }
}
