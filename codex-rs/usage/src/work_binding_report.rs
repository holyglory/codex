//! Join sparse assignment intervals to canonical operation capture sequences.
use std::collections::BTreeSet;

use serde::Deserialize;

use crate::BoundOperationEffort;
use crate::Result;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::WorkBindingTransition;
use crate::reader_query::quoted;
use crate::reader_query::values_source;
use crate::report_query::ReportQuery;
use crate::review_aggregate::TOKEN_PREDICATE;
use crate::review_aggregate::detail_statistics;
use crate::review_storage::query_rows;
use serde_json::Value;
use serde_json::json;

/// Measured effort for one explicit assignment. Missing operation/token links
/// stay outside this result and are reported as unattributed by the caller.
#[derive(Clone, Debug)]
pub struct BoundWorkEffort {
    pub binding: WorkBindingTransition,
    pub effort: BoundOperationEffort,
}

#[derive(Deserialize)]
struct Row {
    binding_index: usize,
    #[serde(flatten)]
    effort: BoundOperationEffort,
}

impl UsageReader {
    /// Resolve assignments from the first captured operation fact, preserving
    /// that assignment across later terminals, account changes and retries.
    /// Explicit exceptional operation bindings take precedence and are excluded
    /// here so the two evidence sources cannot double-count the same work.
    pub fn sparse_bound_effort(
        &self,
        range: TimeRange,
        filter: &UsageFilter,
        transitions: &[WorkBindingTransition],
        exceptional_operations: &BTreeSet<String>,
    ) -> Result<Vec<BoundWorkEffort>> {
        let selected = transitions
            .iter()
            .enumerate()
            .filter(|(_, binding)| {
                binding.effective_capture_sequence <= self.journal_watermark()
                    && filter
                        .thread_id
                        .as_ref()
                        .is_none_or(|id| id == &binding.update.thread_id)
                    && filter
                        .thread_ids
                        .as_ref()
                        .is_none_or(|ids| ids.contains(&binding.update.thread_id))
            })
            .map(|(index, binding)| {
                format!(
                    "({index},{},{},{},{},{},{},{})",
                    quoted(&binding.update.thread_id),
                    binding.effective_capture_sequence,
                    binding.update.transition_sequence,
                    binding.update.native_project_id.is_some(),
                    binding
                        .update
                        .native_project_id
                        .as_deref()
                        .map_or_else(|| "NULL".into(), quoted),
                    binding
                        .update
                        .workstream_id
                        .as_deref()
                        .map_or_else(|| "NULL".into(), quoted),
                    binding
                        .update
                        .outcome_id
                        .as_deref()
                        .map_or_else(|| "NULL".into(), quoted)
                )
            })
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Ok(Vec::new());
        }
        let query = ReportQuery::new(self, range, filter)?;
        let dependencies = retry_dependencies(self, &query)?;
        let exceptions = exceptional_operations
            .iter()
            .map(|id| quoted(id))
            .collect::<Vec<_>>();
        let predicate = if exceptions.is_empty() {
            "true".into()
        } else {
            format!("operation_id NOT IN ({})", exceptions.join(","))
        };
        let statistics = detail_statistics(&query.operations, range);
        let sql = format!(
            "WITH RECURSIVE bindings(binding_index,thread_id,effective_sequence,transition_sequence,assigned,binding_project_id,binding_workstream_id,binding_outcome_id) AS (VALUES {}), \
             points AS (SELECT * FROM bindings QUALIFY row_number() OVER \
               (PARTITION BY thread_id,effective_sequence ORDER BY transition_sequence DESC)=1), \
             ops AS (SELECT * FROM ({statistics}) WHERE {predicate} AND work_binding_start_sequence=first_sequence), \
             lineage_ops AS (SELECT * FROM (SELECT operation_id,first_sequence,thread_id,retry_of_operation_id FROM ops UNION ALL {dependencies}) \
               QUALIFY row_number() OVER (PARTITION BY operation_id ORDER BY first_sequence)=1), \
             lineage AS (SELECT operation_id,operation_id ancestor,first_sequence,thread_id,retry_of_operation_id,0 depth FROM ops \
               UNION ALL SELECT l.operation_id,p.operation_id,p.first_sequence,p.thread_id,p.retry_of_operation_id,l.depth+1 \
               FROM lineage l JOIN lineage_ops p ON l.retry_of_operation_id=p.operation_id \
               AND p.thread_id=l.thread_id AND p.first_sequence<l.first_sequence WHERE l.depth<64), \
             anchors AS (SELECT operation_id,first_sequence,thread_id FROM lineage WHERE retry_of_operation_id IS NULL \
               QUALIFY row_number() OVER (PARTITION BY operation_id ORDER BY depth DESC)=1), \
             assigned AS (SELECT o.*,b.binding_index,b.assigned,b.binding_project_id,b.binding_workstream_id,b.binding_outcome_id \
               FROM ops o JOIN anchors l USING(operation_id) ASOF JOIN points b \
               ON l.thread_id=b.thread_id AND l.first_sequence >= b.effective_sequence), \
             tokens AS (SELECT operation_id,COALESCE(sum(provider_total_tokens),0)::UBIGINT provider_total_tokens, \
               sum(COALESCE(observation_count,1))::UBIGINT token_observations, \
               sum(COALESCE(provider_total_unknowns,CASE WHEN provider_total_tokens IS NULL THEN COALESCE(observation_count,1) ELSE 0 END))::UBIGINT unknown_token_observations \
               FROM ({}) WHERE ({TOKEN_PREDICATE}) GROUP BY operation_id) \
             SELECT min(a.binding_index)::UBIGINT binding_index,'' operation_id, \
               sum((kind <> 'wait')::UBIGINT)::UBIGINT operations, \
               sum(retry_operations)::UBIGINT retry_operations,sum(rework_operations)::UBIGINT rework_operations, \
               sum(COALESCE(t.provider_total_tokens,0))::UBIGINT provider_total_tokens, \
               sum(COALESCE(t.token_observations,0))::UBIGINT token_observations, \
               sum(COALESCE(t.unknown_token_observations,0))::UBIGINT unknown_token_observations, \
               sum(CASE WHEN kind <> 'wait' THEN interval_sum_ms ELSE 0 END)::UBIGINT execution_interval_sum_ms, \
               sum(CASE WHEN kind <> 'wait' THEN unknown_intervals ELSE 0 END)::UBIGINT unknown_execution_intervals, \
               sum(CASE WHEN kind = 'wait' THEN interval_sum_ms ELSE 0 END)::UBIGINT recorded_wait_ms \
             FROM assigned a LEFT JOIN tokens t USING(operation_id) WHERE a.assigned \
             GROUP BY a.thread_id,a.binding_project_id,a.binding_workstream_id,a.binding_outcome_id ORDER BY binding_index",
            selected.join(","),
            query.contributions
        );
        query_rows::<Row>(&query.connection, &sql, transitions.len())?
            .into_iter()
            .map(|row| {
                let binding = transitions[row.binding_index].clone();
                Ok(BoundWorkEffort {
                    binding,
                    effort: row.effort,
                })
            })
            .collect()
    }
}

fn retry_dependencies(reader: &UsageReader, query: &ReportQuery) -> Result<String> {
    let mut pending = query_rows::<Value>(&query.connection, &format!(
        "SELECT DISTINCT retry_of_operation_id FROM ({}) WHERE retry_of_operation_id IS NOT NULL", query.operations
    ), /*max_rows*/ 4096)?.into_iter().filter_map(|row| row["retry_of_operation_id"].as_str().map(str::to_owned)).collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    let mut dependencies = Vec::new();
    for _ in 0..64 {
        pending.retain(|id| !seen.contains(id));
        if pending.is_empty() {
            break;
        }
        if pending.len().saturating_add(seen.len()) > 4096 {
            return Err(crate::StorageError::InvalidData(
                "sparse retry dependency selection exceeds bounds".into(),
            ));
        }
        seen.extend(pending.iter().cloned());
        let ids = pending.into_iter().collect::<Vec<_>>();
        pending = BTreeSet::new();
        for chunk in ids.chunks(256) {
            for metadata in reader.operation_binding_metadata(&chunk.iter().cloned().collect())? {
                if metadata.first_sequence == 0
                    || metadata.work_binding_start_sequence != Some(metadata.first_sequence)
                {
                    continue;
                }
                if let Some(parent) = &metadata.retry_of_operation_id {
                    pending.insert(parent.clone());
                }
                dependencies.push(json!({"operation_id":metadata.operation_id,"first_sequence":metadata.first_sequence,
                    "thread_id":metadata.thread_id,"retry_of_operation_id":metadata.retry_of_operation_id}));
            }
        }
    }
    values_source(
        &dependencies,
        &[
            ("operation_id", "VARCHAR"),
            ("first_sequence", "UBIGINT"),
            ("thread_id", "VARCHAR"),
            ("retry_of_operation_id", "VARCHAR"),
        ],
    )
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
        let Some(outcome) = &row.binding.update.outcome_id else {
            continue;
        };
        let project = row
            .binding
            .update
            .native_project_id
            .clone()
            .ok_or_else(arithmetic_error)?;
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
            .entry((
                outcome.clone(),
                row.binding.update.workstream_id.clone(),
                project,
            ))
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
        "unknownOperations":unknown,"transitions":sparse.len()});
    report["outcomes"] = json!({"schemaVersion":1,"coverage":if rows.is_empty() {"unattributed"} else {"partial"},"totals":total,"attributed":attributed,
        "unattributed":unattributed,"unattributedReasons":{"work_binding_or_outcome_unobserved":unattributed["operations"]},
        "totalRows":rows.len(),"rows":rows,"nextCursor":Value::Null,
        "basis":"Explicit sparse assignment intervals joined to first captured operation sequence. Missing or retired links stay unattributed; operation completion does not prove delivery."});
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
