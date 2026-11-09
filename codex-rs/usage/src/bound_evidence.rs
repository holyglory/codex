//! Finite, explicit operation-ID evidence for external work-context bindings.
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use crate::Result;
use crate::StorageError;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::reader_query::quoted;
use crate::report_query::ReportQuery;
use crate::review_aggregate::detail_statistics;
use crate::review_storage::query_rows;

/// Measurements joined only through a captured `operation_id`. Retired detail,
/// response-only token links, and absent observations are never reconstructed.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BoundOperationEffort {
    pub operation_id: String,
    pub operations: u64,
    pub retry_operations: u64,
    pub rework_operations: u64,
    pub provider_total_tokens: u64,
    pub token_observations: u64,
    pub unknown_token_observations: u64,
    pub execution_interval_sum_ms: u64,
    pub unknown_execution_intervals: u64,
    pub recorded_wait_ms: u64,
}

impl UsageReader {
    /// Read at most 256 exact identities from the same pinned review view.
    /// A binding alone does not create an operation, token, or timing fact.
    pub fn bound_operation_effort(
        &self,
        range: TimeRange,
        filter: &UsageFilter,
        operation_ids: &BTreeSet<String>,
    ) -> Result<Vec<BoundOperationEffort>> {
        if operation_ids.len() > 256 {
            return Err(StorageError::InvalidData(
                "too many explicit operation bindings".into(),
            ));
        }
        if operation_ids.is_empty() {
            return Ok(Vec::new());
        }
        let query = ReportQuery::new(self, range, filter)?;
        let selected = operation_ids
            .iter()
            .map(|id| quoted(id))
            .collect::<Vec<_>>()
            .join(", ");
        let statistics = detail_statistics(&query.operations, range);
        let token_predicate = crate::review_aggregate::TOKEN_PREDICATE;
        query_rows(
            &query.connection,
            &format!(
                "WITH ops AS (SELECT operation_id, \
             (kind <> 'wait')::UBIGINT operations, retry_operations, rework_operations, \
             CASE WHEN kind <> 'wait' THEN interval_sum_ms ELSE 0 END::UBIGINT execution_interval_sum_ms, \
             CASE WHEN kind <> 'wait' THEN unknown_intervals ELSE 0 END::UBIGINT unknown_execution_intervals, \
             CASE WHEN kind = 'wait' THEN interval_sum_ms ELSE 0 END::UBIGINT recorded_wait_ms \
             FROM ({statistics}) WHERE operation_id IN ({selected})), \
             tokens AS (SELECT operation_id, COALESCE(sum(provider_total_tokens),0)::UBIGINT provider_total_tokens, \
             COALESCE(sum(COALESCE(observation_count,1)),0)::UBIGINT token_observations, \
             COALESCE(sum(COALESCE(provider_total_unknowns, CASE WHEN provider_total_tokens IS NULL \
               THEN COALESCE(observation_count,1) ELSE 0 END)),0)::UBIGINT unknown_token_observations \
             FROM ({contributions}) WHERE operation_id IN ({selected}) AND ({token_predicate}) GROUP BY operation_id) \
             SELECT COALESCE(o.operation_id,t.operation_id) operation_id, \
             COALESCE(o.operations,0)::UBIGINT operations, COALESCE(o.retry_operations,0)::UBIGINT retry_operations, \
             COALESCE(o.rework_operations,0)::UBIGINT rework_operations, \
             COALESCE(t.provider_total_tokens,0)::UBIGINT provider_total_tokens, \
             COALESCE(t.token_observations,0)::UBIGINT token_observations, \
             COALESCE(t.unknown_token_observations,0)::UBIGINT unknown_token_observations, \
             COALESCE(o.execution_interval_sum_ms,0)::UBIGINT execution_interval_sum_ms, \
             COALESCE(o.unknown_execution_intervals,0)::UBIGINT unknown_execution_intervals, \
             COALESCE(o.recorded_wait_ms,0)::UBIGINT recorded_wait_ms \
             FROM ops o FULL OUTER JOIN tokens t USING(operation_id) ORDER BY operation_id",
                contributions = query.contributions
            ),
            operation_ids.len(),
        )
    }
}
