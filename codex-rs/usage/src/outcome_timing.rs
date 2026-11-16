//! Execution unions for explicit work sets. Delivery duration is not inferred.
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::Result;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::reader_query::quoted;
use crate::reader_query::values_source;
use crate::report_metrics::IntervalMetrics;
use crate::report_metrics::grouped_interval_metrics;
use crate::report_query::ReportQuery;

/// An exceptional operation declaration replaces its captured sparse tuple.
#[derive(Clone, Debug, Serialize)]
pub struct OutcomeTimingContext {
    pub native_project_id: Option<String>,
    pub workstream_id: Option<String>,
    pub outcome_id: Option<String>,
}

/// Independently measured operation sets; their unions cannot be added or subtracted.
pub struct OutcomeTimings {
    groups: BTreeMap<String, IntervalMetrics>,
}

impl OutcomeTimings {
    pub(crate) fn total(&self) -> IntervalMetrics {
        self.groups.get("totals").cloned().unwrap_or_default()
    }

    /// Fill existing outcome effort fields without changing counts, tokens, or delivery evidence.
    pub fn apply(&self, outcomes: &mut Value) {
        for bucket in ["totals", "attributed", "unattributed"] {
            self.apply_group(&mut outcomes[bucket], bucket);
        }
        if let Some(rows) = outcomes["rows"].as_array_mut() {
            for row in rows {
                let key = format!(
                    "group:{}",
                    json!([
                        row["nativeProjectId"],
                        row["workstreamId"],
                        row["outcomeId"]
                    ])
                );
                self.apply_group(&mut row["effort"], &key);
            }
        }
    }

    fn apply_group(&self, effort: &mut Value, group: &str) {
        let timing = self.groups.get(group).cloned().unwrap_or_default().value();
        for (output, source) in [
            ("activeAgentMs", "activeMs"),
            ("elapsedExecutionMs", "elapsedMs"),
            ("recordedWaitMs", "waitMs"),
        ] {
            effort[output] = timing[source].clone();
        }
    }
}

pub(crate) fn from_query(
    reader: &UsageReader,
    query: &ReportQuery,
    range: TimeRange,
    filter: &UsageFilter,
    overrides: &BTreeMap<String, OutcomeTimingContext>,
    workstream: Option<&str>,
) -> Result<OutcomeTimings> {
    if overrides.len() > 256 {
        return Err(crate::review_state::invalid(
            "too many explicit operation timing bindings",
        ));
    }
    let rows = overrides
        .iter()
        .map(|(operation, context)| {
            json!({
                "operation_id":operation,"native_project_id":context.native_project_id,
                "workstream_id":context.workstream_id,"outcome_id":context.outcome_id,
            })
        })
        .collect::<Vec<_>>();
    let declarations = values_source(
        &rows,
        &[
            ("operation_id", "VARCHAR"),
            ("native_project_id", "VARCHAR"),
            ("workstream_id", "VARCHAR"),
            ("outcome_id", "VARCHAR"),
        ],
    )?;
    let scope = workstream.map_or_else(
        || "true".into(),
        |stream| {
            format!(
                "(workstream_id IS NULL OR workstream_id={})",
                quoted(stream)
            )
        },
    );
    let attributed = if workstream.is_some() {
        "outcome_id IS NOT NULL AND workstream_id IS NOT NULL"
    } else {
        "outcome_id IS NOT NULL"
    };
    let descriptor =
        crate::review_projection::load_review_projection_shared(&reader.root, &reader.manifest)?;
    // Retired detail has no interval endpoints. Completion-hour buckets cannot
    // establish whether execution spanned this window, including lifetime rows.
    let retired = crate::review_aggregate::retained_statistics_source(
        &reader.root,
        descriptor.as_deref(),
        TimeRange::all(),
        filter,
    )?;
    let source = format!(
        "WITH declarations AS ({declarations}), operation_sets AS (
            SELECT o.kind,o.started_at_ms,o.ended_at_ms,o.agent_id,o.thread_id,
                1::UBIGINT observation_weight,false retired_interval,
                CASE WHEN d.operation_id IS NOT NULL THEN d.native_project_id ELSE o.work_native_project_id END native_project_id,
                CASE WHEN d.operation_id IS NOT NULL THEN d.workstream_id ELSE o.workstream_id END workstream_id,
                CASE WHEN d.operation_id IS NOT NULL THEN d.outcome_id ELSE o.work_outcome_id END outcome_id
            FROM ({operations}) o LEFT JOIN declarations d USING(operation_id)
            UNION ALL
            SELECT kind,NULL::BIGINT,NULL::BIGINT,NULL::VARCHAR,thread_id,
                operation_count::UBIGINT,true,work_native_project_id,workstream_id,work_outcome_id
            FROM ({retired}) WHERE operation_count>0
        ), scoped AS (SELECT * FROM operation_sets WHERE {scope})
        SELECT 'totals' timing_group,* FROM scoped
        UNION ALL SELECT 'attributed',* FROM scoped WHERE {attributed}
        UNION ALL SELECT 'unattributed',* FROM scoped WHERE NOT ({attributed})
        UNION ALL SELECT 'group:' || to_json([native_project_id,workstream_id,outcome_id])::VARCHAR,*
            FROM scoped WHERE {attributed}", operations=query.operations,
    );
    Ok(OutcomeTimings {
        groups: grouped_interval_metrics(
            &query.connection,
            &source,
            range.start_ms(),
            range.end_ms(),
        )?,
    })
}
