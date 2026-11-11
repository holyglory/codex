//! Measured effort joined to persisted, explicitly declared work dimensions.
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::BoundOperationEffort;
use crate::Result;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::reader_query::quoted;
use crate::report_query::ReportQuery;
use crate::review_aggregate::TOKEN_PREDICATE;
use crate::review_aggregate::detail_statistics;
use crate::review_storage::query_rows;

/// Captured assignment tuple, without inventing a switch time or revision for
/// identity-free retained aggregates. A null stream or outcome remains null.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BoundWorkContext {
    pub native_project_id: String,
    pub workstream_id: Option<String>,
    pub outcome_id: Option<String>,
    pub thread_id: Option<String>,
}

impl BoundWorkContext {
    /// Opaque report-context identity, derived from the complete atomic tuple.
    pub fn context_id(&self) -> String {
        let tuple = json!([
            self.native_project_id,
            self.workstream_id,
            self.outcome_id,
            self.thread_id
        ]);
        format!("work:{:016x}", crate::id_hash(&tuple.to_string()))
    }
}

/// Aggregate measurements; this row does not pretend to be one operation.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct WorkEffort {
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

impl From<&BoundOperationEffort> for WorkEffort {
    fn from(row: &BoundOperationEffort) -> Self {
        Self {
            operations: row.operations,
            retry_operations: row.retry_operations,
            rework_operations: row.rework_operations,
            provider_total_tokens: row.provider_total_tokens,
            token_observations: row.token_observations,
            unknown_token_observations: row.unknown_token_observations,
            execution_interval_sum_ms: row.execution_interval_sum_ms,
            unknown_execution_intervals: row.unknown_execution_intervals,
            recorded_wait_ms: row.recorded_wait_ms,
        }
    }
}

/// Live and retained effort belonging to one captured assignment tuple.
#[derive(Clone, Debug, Deserialize)]
pub struct BoundWorkEffort {
    #[serde(flatten)]
    pub context: BoundWorkContext,
    #[serde(flatten)]
    pub effort: WorkEffort,
}

impl UsageReader {
    /// Read captured live and retained work; exceptional operation declarations
    /// override live identities, while retired identity-free totals stay intact.
    pub fn bound_work_effort(
        &self,
        range: TimeRange,
        filter: &UsageFilter,
        exceptional: &BTreeSet<String>,
    ) -> Result<Vec<BoundWorkEffort>> {
        let query = ReportQuery::new(self, range, filter)?;
        bound_work_effort_from_query(&query, range, exceptional)
    }
}

pub(crate) fn bound_work_effort_from_query(
    query: &ReportQuery,
    range: TimeRange,
    exceptional: &BTreeSet<String>,
) -> Result<Vec<BoundWorkEffort>> {
    let predicate = |column: &str| {
        if exceptional.is_empty() {
            "true".into()
        } else {
            format!(
                "({column} IS NULL OR {column} NOT IN ({}))",
                exceptional
                    .iter()
                    .map(|id| quoted(id))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    };
    let operation_filter = predicate("operation_id");
    let token_filter = predicate("work_operation_id");
    let detail = detail_statistics(&query.operations, range);
    let columns = "work_native_project_id,workstream_id,work_outcome_id,thread_id,kind,operation_count,interval_sum_ms,unknown_intervals,retry_operations,rework_operations";
    let statistics = if exceptional.is_empty() {
        query.operation_statistics.clone()
    } else {
        format!(
            "SELECT {columns} FROM ({detail}) WHERE {operation_filter} UNION ALL SELECT {columns} FROM ({})",
            query.retained_operation_statistics
        )
    };
    let work = query.work_contributions();
    let sql = format!(
        "WITH effort AS (SELECT work_native_project_id,workstream_id,work_outcome_id,thread_id, \
         CASE WHEN kind <> 'wait' THEN operation_count ELSE 0 END::UBIGINT operations,retry_operations,rework_operations, \
         0::UBIGINT provider_total_tokens,0::UBIGINT token_observations,0::UBIGINT unknown_token_observations, \
         CASE WHEN kind <> 'wait' THEN interval_sum_ms ELSE 0 END::UBIGINT execution_interval_sum_ms, \
         CASE WHEN kind <> 'wait' THEN unknown_intervals ELSE 0 END::UBIGINT unknown_execution_intervals, \
         CASE WHEN kind = 'wait' THEN interval_sum_ms ELSE 0 END::UBIGINT recorded_wait_ms \
         FROM ({statistics}) WHERE work_native_project_id IS NOT NULL UNION ALL \
         SELECT work_native_project_id,workstream_id,work_outcome_id,thread_id,0,0,0, \
         COALESCE(provider_total_tokens,0)::UBIGINT,COALESCE(observation_count,1)::UBIGINT, \
         COALESCE(provider_total_unknowns,CASE WHEN provider_total_tokens IS NULL THEN COALESCE(observation_count,1) ELSE 0 END)::UBIGINT,0,0,0 \
         FROM ({work}) WHERE work_native_project_id IS NOT NULL AND ({TOKEN_PREDICATE}) AND {token_filter}) \
         SELECT work_native_project_id native_project_id,workstream_id,work_outcome_id outcome_id, \
         CASE WHEN count(DISTINCT thread_id)=1 AND count(*) FILTER (WHERE thread_id IS NULL)=0 THEN min(thread_id) ELSE NULL END thread_id, \
         sum(operations)::UBIGINT operations,sum(retry_operations)::UBIGINT retry_operations,sum(rework_operations)::UBIGINT rework_operations, \
         sum(provider_total_tokens)::UBIGINT provider_total_tokens,sum(token_observations)::UBIGINT token_observations, \
         sum(unknown_token_observations)::UBIGINT unknown_token_observations,sum(execution_interval_sum_ms)::UBIGINT execution_interval_sum_ms, \
         sum(unknown_execution_intervals)::UBIGINT unknown_execution_intervals,sum(recorded_wait_ms)::UBIGINT recorded_wait_ms \
         FROM effort GROUP BY work_native_project_id,workstream_id,work_outcome_id \
         ORDER BY work_native_project_id,workstream_id,work_outcome_id"
    );
    // Existing model/IPC response bounds still apply. Larger views must be
    // narrowed, never silently truncated or mistaken for complete accounting.
    query_rows(&query.connection, &sql, /*max_rows*/ 16_384)
}

pub(crate) fn apply_sparse_report(report: &mut Value, sparse: &[BoundWorkEffort]) -> Result<()> {
    if sparse.is_empty() {
        report["workBindings"]["basis"] = json!("explicit_sparse_transitions");
        report["workBindings"]["coverage"] = json!("unattributed");
        report["outcomes"]["coverage"] = json!("unattributed");
        report["outcomes"]["basis"] = json!(
            "Explicit switches were captured, but selected work is unassigned or lacks operation start/lineage evidence."
        );
        return Ok(());
    }
    let zero = || {
        let measurement = json!({"measured":0,"exact":Value::Null,"unknown":0});
        json!({"operations":0,"retryOperations":0,"reworkOperations":0,"providerTotalTokens":measurement,
            "activeAgentMs":measurement,"elapsedExecutionMs":measurement,"recordedWaitMs":measurement})
    };
    let mut attributed = zero();
    let mut groups = std::collections::BTreeMap::<(String, Option<String>, String), Value>::new();
    let mut bound = 0u64;
    for row in sparse {
        bound = bound
            .checked_add(row.effort.operations)
            .ok_or_else(arithmetic_error)?;
        let Some(outcome) = &row.context.outcome_id else {
            continue;
        };
        let project = row.context.native_project_id.clone();
        let mut effort = zero();
        effort["operations"] = json!(row.effort.operations);
        effort["retryOperations"] = json!(row.effort.retry_operations);
        effort["reworkOperations"] = json!(row.effort.rework_operations);
        effort["providerTotalTokens"]["measured"] = json!(row.effort.provider_total_tokens);
        effort["providerTotalTokens"]["unknown"] = json!(row.effort.unknown_token_observations);
        effort["activeAgentMs"]["unknown"] = json!(row.effort.operations);
        effort["elapsedExecutionMs"]["measured"] = json!(row.effort.execution_interval_sum_ms);
        effort["elapsedExecutionMs"]["unknown"] = json!(row.effort.unknown_execution_intervals);
        effort["recordedWaitMs"]["measured"] = json!(row.effort.recorded_wait_ms);
        combine(&mut attributed, &effort, /*subtract*/ false)?;
        let group = groups
            .entry((outcome.clone(), row.context.workstream_id.clone(), project))
            .or_insert_with(zero);
        combine(group, &effort, /*subtract*/ false)?;
    }
    let total = report["outcomes"]["totals"].clone();
    let mut unattributed = total.clone();
    combine(&mut unattributed, &attributed, /*subtract*/ true)?;
    let unknown = total["operations"]
        .as_u64()
        .ok_or_else(arithmetic_error)?
        .checked_sub(bound)
        .ok_or_else(arithmetic_error)?;
    let rows = groups
        .into_iter()
        .map(|((outcome, workstream, project), effort)| {
            json!({"outcomeId":outcome,
        "workstreamId":workstream,"nativeProjectId":project,"title":Value::Null,"effort":effort})
        })
        .collect::<Vec<_>>();
    report["workBindings"] = json!({"basis":"explicit_sparse_transitions","boundOperations":bound,
        "unknownOperations":unknown,"coverage":if unknown == 0 {"recorded"} else {"partial"},"contexts":sparse.len()});
    report["outcomes"] = json!({"schemaVersion":1,"coverage":if rows.is_empty() {"unattributed"} else {"partial"},"totals":total,"attributed":attributed,
        "unattributed":unattributed,"unattributedReasons":{"work_binding_or_outcome_unobserved":unattributed["operations"]},
        "totalRows":rows.len(),"rows":rows,"nextCursor":Value::Null,
        "basis":"Captured agent-declared assignment tuples from canonical operations and retained aggregates. Missing links stay unattributed; operation completion does not prove delivery."});
    Ok(())
}

fn combine(target: &mut Value, delta: &Value, subtract: bool) -> Result<()> {
    fn amount(target: &mut Value, delta: &Value, subtract: bool) -> Result<()> {
        if let (Some(left), Some(right)) = (target.as_u64(), delta.as_u64()) {
            let value = if subtract {
                left.checked_sub(right)
            } else {
                left.checked_add(right)
            }
            .ok_or_else(arithmetic_error)?;
            *target = json!(value);
        }
        Ok(())
    }
    for key in ["operations", "retryOperations", "reworkOperations"] {
        amount(&mut target[key], &delta[key], subtract)?;
    }
    for key in [
        "providerTotalTokens",
        "activeAgentMs",
        "elapsedExecutionMs",
        "recordedWaitMs",
    ] {
        for component in ["measured", "unknown"] {
            amount(
                &mut target[key][component],
                &delta[key][component],
                subtract,
            )?;
        }
        target[key]["exact"] = Value::Null;
    }
    Ok(())
}

fn arithmetic_error() -> crate::StorageError {
    crate::StorageError::InvalidData("inconsistent sparse work binding measurements".into())
}
