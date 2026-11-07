//! Retry links and measured waits from explicit request and timer observations.
use serde_json::Value;

use crate::Result;
use crate::reader_query::query_error;
use crate::report_query::ReportQuery;
use crate::review_storage::query_rows;

pub(crate) struct RetryEvidence {
    pub(crate) retries: u64,
    pub(crate) attempts: u64,
    pub(crate) waits: Vec<Value>,
    pub(crate) wait_ms: u64,
    pub(crate) links: Vec<Value>,
    pub(crate) linked_operations: u64,
}

impl RetryEvidence {
    pub(crate) fn read(query: &ReportQuery) -> Result<Self> {
        let (retries, attempts, wait_ms, linked_operations) = query
            .connection
            .query_row(
                &format!(
                    "SELECT COALESCE(sum(retry_operations),0)::UBIGINT, \
                 COALESCE(sum(observed_attempts),0)::UBIGINT, \
                 COALESCE(sum(interval_sum_ms) FILTER (WHERE kind = 'wait'),0)::UBIGINT, \
                 COALESCE(sum(COALESCE(linked_operations,rework_operations)),0)::UBIGINT \
                 FROM ({})",
                    query.operation_statistics,
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(query_error)?;
        let waits = query_rows::<Value>(
            &query.connection,
            &format!(
                "SELECT COALESCE(tool,'unknown') reason, COALESCE(outcome,'unknown') outcome, \
             sum(operation_count)::UBIGINT count, sum(interval_sum_ms)::UBIGINT \"measuredWaitMs\", \
             sum(unknown_intervals)::UBIGINT \"unknownIntervals\" \
             FROM ({}) WHERE kind = 'wait' GROUP BY ALL ORDER BY reason,outcome",
                query.operation_statistics,
            ),
            32,
        )?;
        let predicate = "kind <> 'wait' AND (retry_of_operation_id IS NOT NULL OR rework_of_operation_id IS NOT NULL)";
        let links = query_rows::<Value>(
            &query.connection,
            &format!(
                "SELECT operation_id \"operationId\", started_at_ms \"startedAtMs\", \
             attempt_number \"attemptNumber\", retry_of_operation_id \"retryOfOperationId\", \
             rework_of_operation_id \"reworkOfOperationId\" \
             FROM ({}) WHERE {predicate} ORDER BY started_at_ms,operation_id LIMIT 17",
                query.operations,
            ),
            17,
        )?;
        Ok(Self {
            retries,
            attempts,
            waits,
            wait_ms,
            links,
            linked_operations,
        })
    }
}
