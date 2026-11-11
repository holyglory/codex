//! Finite, explicit operation-ID evidence for external work-context bindings.
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use crate::Result;
use crate::ReviewOperationKind;
use crate::StorageError;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::reader_query::quoted;
use crate::report_query::ReportQuery;
use crate::review_aggregate::detail_statistics;
use crate::review_storage::query_rows;

/// Captured identities for validating declarations without a history scan.
#[derive(Clone, Debug, Default)]
pub struct OperationBindingMetadata {
    pub operation_id: String,
    pub kind: Option<ReviewOperationKind>,
    pub account_id: Option<String>,
    pub repository_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
}

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
    /// Exact indexed operation identities plus this reader's already pinned tail.
    /// Missing identities stay unknown; no path or thread inference is applied.
    pub fn operation_binding_metadata(
        &self,
        operation_ids: &BTreeSet<String>,
    ) -> Result<Vec<OperationBindingMetadata>> {
        if operation_ids.len() > 256 {
            return Err(StorageError::InvalidData(
                "too many explicit operation bindings".into(),
            ));
        }
        if operation_ids.is_empty() {
            return Ok(Vec::new());
        }
        let connection =
            crate::duckdb_connection::open_in_memory().map_err(crate::reader_query::query_error)?;
        let descriptor =
            crate::review_projection::load_review_projection_shared(&self.root, &self.manifest)?;
        let mut operations = descriptor
            .as_ref()
            .map_or_else(
                || Ok(Vec::new()),
                |descriptor| {
                    crate::review_projection::load_review_operations(
                        &self.root,
                        descriptor,
                        operation_ids,
                        &connection,
                    )
                },
            )?
            .into_iter()
            .map(|operation| (operation.operation_id.clone(), operation))
            .collect::<BTreeMap<_, _>>();
        let tail = crate::report_query::canonical_tail(self, &connection)?;
        for record in tail.iter().copied().filter(|record| {
            record
                .operation_id
                .as_ref()
                .is_some_and(|id| operation_ids.contains(id))
        }) {
            if crate::review_state::lifecycle(record).is_some() {
                let id = record.operation_id.as_ref().ok_or_else(|| {
                    StorageError::InvalidData("operation identity missing".into())
                })?;
                let sequence = self.record_sequences[&record.id];
                if let Some(operation) = operations.get_mut(id) {
                    operation.merge(sequence, record)?;
                } else {
                    operations.insert(
                        id.clone(),
                        crate::ReviewOperation::from_fact(sequence, record)?,
                    );
                }
            }
        }
        let mut metadata = operations
            .into_values()
            .map(|operation| {
                (
                    operation.operation_id.clone(),
                    OperationBindingMetadata {
                        operation_id: operation.operation_id,
                        kind: Some(operation.kind),
                        account_id: operation.account_id,
                        repository_id: operation.repository_id,
                        thread_id: operation.thread_id,
                        turn_id: operation.turn_id,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        for record in tail.iter().copied().filter(|record| {
            record
                .operation_id
                .as_ref()
                .is_some_and(|id| operation_ids.contains(id))
                && (crate::review_state::lifecycle(record).is_some()
                    || record.event.as_deref() == Some("token_observation")
                    || record.provider_total_tokens.is_some())
        }) {
            let id = record
                .operation_id
                .as_ref()
                .ok_or_else(|| StorageError::InvalidData("operation identity missing".into()))?;
            let entry = metadata
                .entry(id.clone())
                .or_insert_with(|| OperationBindingMetadata {
                    operation_id: id.clone(),
                    ..Default::default()
                });
            for (captured, incoming) in [
                (&mut entry.account_id, &record.account_id),
                (&mut entry.repository_id, &record.repository_id),
                (&mut entry.thread_id, &record.thread_id),
                (&mut entry.turn_id, &record.turn_id),
            ] {
                if captured
                    .as_ref()
                    .zip(incoming.as_ref())
                    .is_some_and(|(a, b)| a != b)
                {
                    return Err(StorageError::InvalidData(
                        "conflicting operation identity facts".into(),
                    ));
                }
                if captured.is_none() {
                    *captured = incoming.clone();
                }
            }
        }
        Ok(metadata.into_values().collect())
    }

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
