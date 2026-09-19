use crate::PerformanceReviewQuery;
use crate::UsageStoreError;
use crate::outcome_math::Effort;
use crate::outcome_types::OutcomeReport;
use crate::outcome_types::OutcomeRow;
use crate::performance_review::query;
use crate::work_binding::valid_binding_identifier;
use sqlx::Row;
use sqlx::SqliteConnection;
use std::collections::BTreeMap;
use std::collections::HashMap;

const MAX_FACT_ROWS: usize = 200_000;

pub(super) struct Operation {
    pub(super) agent_id: Option<String>,
    pub(super) interval: Option<(i64, i64)>,
    pub(super) overlaps_window: bool,
    pub(super) state: String,
    pub(super) retry: bool,
    pub(super) rework: bool,
    model: bool,
    key: Option<(String, Option<String>)>,
    reason: Option<&'static str>,
}

pub(crate) async fn read(
    connection: &mut SqliteConnection,
    scope: &PerformanceReviewQuery,
    source: &query::Selection,
) -> Result<OutcomeReport, UsageStoreError> {
    let mut builder = query::selection(scope, source);
    let rows = builder.push(query::TOKEN_FACTS).push("SELECT owner.id, owner.agent_id, owner.operation_kind,
        owner.started_at_ms, terminal.occurred_at_ms ended_at_ms, terminal.duration_ns IS NOT NULL timing_known, effective.effective_state,
        owner.retry_of_operation_id IS NOT NULL retry, owner.rework_of_operation_id IS NOT NULL rework,
        context.operation_id context_id, context.native_project_id, context.workstream_id, context.outcome_id,
        (SELECT COUNT(DISTINCT repository_id) FROM repository_attributions WHERE operation_id = owner.id) repository_count
        FROM scoped owner LEFT JOIN effective ON effective.id = owner.id
        LEFT JOIN operation_events terminal ON terminal.operation_id = owner.id AND terminal.terminal = 1
        LEFT JOIN operation_work_contexts context ON context.operation_id = owner.id
        WHERE effective.id IS NOT NULL OR owner.id IN (SELECT operation_id FROM token_facts)
        ORDER BY owner.id LIMIT 200001").build().fetch_all(&mut *connection).await.map_err(UsageStoreError::Database)?;
    if rows.len() > MAX_FACT_ROWS {
        return Err(UsageStoreError::TaskTreeTooLarge);
    }
    let lower = scope
        .time_range
        .map_or(i64::MIN, crate::UtcTimeRange::start_ms);
    let upper = scope
        .time_range
        .map_or(i64::MAX, crate::UtcTimeRange::end_ms);
    let mut operations = BTreeMap::new();
    for row in rows {
        let id: String = row.try_get("id").map_err(UsageStoreError::Database)?;
        let context: Option<String> = row
            .try_get("context_id")
            .map_err(UsageStoreError::Database)?;
        let project: Option<String> = row
            .try_get("native_project_id")
            .map_err(UsageStoreError::Database)?;
        let outcome: Option<String> = row
            .try_get("outcome_id")
            .map_err(UsageStoreError::Database)?;
        let workstream: Option<String> = row
            .try_get("workstream_id")
            .map_err(UsageStoreError::Database)?;
        if [&project, &outcome, &workstream]
            .into_iter()
            .flatten()
            .any(|id| !valid_binding_identifier(id))
        {
            return Err(UsageStoreError::InvalidFact);
        }
        let reason = if context.is_none() {
            Some("legacy_operation")
        } else if project.is_none() {
            Some("context_unavailable")
        } else if outcome.is_none() {
            Some("outcome_not_declared")
        } else if row
            .try_get::<i64, _>("repository_count")
            .map_err(UsageStoreError::Database)?
            > 1
        {
            Some("multiple_repositories")
        } else {
            None
        };
        let start: i64 = row
            .try_get("started_at_ms")
            .map_err(UsageStoreError::Database)?;
        let end: Option<i64> = row
            .try_get("ended_at_ms")
            .map_err(UsageStoreError::Database)?;
        let overlaps_window =
            start < upper && end.is_none_or(|end| end > lower || end == start && start >= lower);
        let timing_known: bool = row
            .try_get("timing_known")
            .map_err(UsageStoreError::Database)?;
        let interval = end
            .filter(|end| *end >= start && overlaps_window && timing_known)
            .map(|end| (start.max(lower), end.min(upper)));
        operations.insert(
            id,
            Operation {
                agent_id: row.try_get("agent_id").map_err(UsageStoreError::Database)?,
                state: row
                    .try_get::<Option<String>, _>("effective_state")
                    .map_err(UsageStoreError::Database)?
                    .unwrap_or_default(),
                model: row
                    .try_get::<String, _>("operation_kind")
                    .map_err(UsageStoreError::Database)?
                    == "model_request",
                retry: row.try_get("retry").map_err(UsageStoreError::Database)?,
                rework: row.try_get("rework").map_err(UsageStoreError::Database)?,
                interval,
                overlaps_window,
                reason,
                key: if reason.is_none() {
                    outcome.map(|outcome| (outcome, workstream))
                } else {
                    None
                },
            },
        );
    }
    let mut builder = query::selection(scope, source);
    let tokens = builder.push(query::TOKEN_FACTS).push("SELECT operation_id, token_count,
        unknown_count, incomplete, COALESCE(conflict, 0) conflict FROM token_facts
        WHERE category_path = 'total_tokens' AND measurement_provenance = 'provider_reported' LIMIT 200001")
        .build().fetch_all(&mut *connection).await.map_err(UsageStoreError::Database)?;
    if tokens.len() > MAX_FACT_ROWS {
        return Err(UsageStoreError::TaskTreeTooLarge);
    }
    let mut counts = HashMap::<String, (u64, u64)>::new();
    for row in tokens {
        let count = counts
            .entry(
                row.try_get("operation_id")
                    .map_err(UsageStoreError::Database)?,
            )
            .or_default();
        let value: Option<i64> = row
            .try_get("token_count")
            .map_err(UsageStoreError::Database)?;
        let conflict: bool = row.try_get("conflict").map_err(UsageStoreError::Database)?;
        if !conflict && let Some(value) = value {
            count.0 = count
                .0
                .checked_add(
                    u64::try_from(value).map_err(|_| UsageStoreError::DatabaseValueOutOfRange)?,
                )
                .ok_or(UsageStoreError::AggregateOverflow)?;
        }
        count.1 += u64::from(
            conflict
                || value.is_none()
                || row
                    .try_get::<bool, _>("incomplete")
                    .map_err(UsageStoreError::Database)?,
        );
    }
    let mut builder = query::selection(scope, source);
    let rows = builder.push("SELECT span.operation_id, span.started_at_ms, ended.occurred_at_ms ended_at_ms
        FROM activity_spans span JOIN effective ON effective.id = span.operation_id CROSS JOIN bounds
        LEFT JOIN activity_span_events ended ON ended.activity_span_id = span.id AND ended.event_kind = 'ended'
        WHERE span.activity_state IN ('user_wait', 'external_wait', 'blocked_wait')
        AND span.started_at_ms < upper_ms AND (ended.occurred_at_ms IS NULL OR ended.occurred_at_ms > lower_ms)
        LIMIT 200001").build().fetch_all(&mut *connection).await.map_err(UsageStoreError::Database)?;
    if rows.len() > MAX_FACT_ROWS {
        return Err(UsageStoreError::TaskTreeTooLarge);
    }
    let mut waits = HashMap::<String, (Vec<(i64, i64)>, u64)>::new();
    for row in rows {
        let id: String = row
            .try_get("operation_id")
            .map_err(UsageStoreError::Database)?;
        let start: i64 = row
            .try_get("started_at_ms")
            .map_err(UsageStoreError::Database)?;
        let end: Option<i64> = row
            .try_get("ended_at_ms")
            .map_err(UsageStoreError::Database)?;
        let entry = waits.entry(id).or_default();
        if let Some(end) = end.filter(|end| *end >= start) {
            entry.0.push((start.max(lower), end.min(upper)));
        } else {
            entry.1 += 1;
        }
    }
    let mut total = Effort::default();
    let mut attributed = Effort::default();
    let mut unattributed = Effort::default();
    let mut groups = BTreeMap::<(String, Option<String>), Effort>::new();
    let mut reasons = BTreeMap::new();
    for (id, operation) in &operations {
        let (tokens, unknown_tokens) = counts
            .get(id)
            .copied()
            .unwrap_or((0, u64::from(operation.model)));
        let (waits, unknown_waits) = waits.remove(id).unwrap_or_default();
        let mut targets = vec![&mut total];
        if let Some(key) = &operation.key {
            targets.push(&mut attributed);
            targets.push(groups.entry(key.clone()).or_default());
        } else {
            targets.push(&mut unattributed);
            *reasons
                .entry(
                    operation
                        .reason
                        .unwrap_or("context_unavailable")
                        .to_string(),
                )
                .or_insert(0_u64) += 1;
        }
        for target in targets {
            target.add_time(operation, &waits, unknown_waits)?;
            target.tokens = target
                .tokens
                .checked_add(tokens)
                .ok_or(UsageStoreError::AggregateOverflow)?;
            target.unknown_tokens += unknown_tokens;
        }
    }
    let totals = total.finish()?;
    let attributed = attributed.finish()?;
    let unattributed = unattributed.finish()?;
    let coverage = if attributed.operations == 0 {
        "unavailable"
    } else if unattributed.operations > 0
        || totals.provider_total_tokens.unknown > 0
        || totals.active_agent_ms.unknown > 0
        || totals.elapsed_execution_ms.unknown > 0
        || totals.recorded_wait_ms.unknown > 0
    {
        "partial"
    } else {
        "complete"
    };
    let rows = groups
        .into_iter()
        .map(|((outcome_id, workstream_id), effort)| {
            Ok(OutcomeRow {
                outcome_id,
                workstream_id,
                effort: effort.finish()?,
            })
        })
        .collect::<Result<Vec<_>, UsageStoreError>>()?;
    Ok(OutcomeReport {
        schema_version: 1, coverage: coverage.into(), totals, attributed, unattributed,
        unattributed_reasons: reasons, total_rows: rows.len(), rows, next_cursor: None,
        basis: "Prospective operation-start declarations only. Provider totals deduplicate by owner and source event; component categories are not added. Time is clipped to the UTC window. Active time sums per-agent unions after recorded waits; elapsed and waiting time are unions, not sums of outcome rows. Unknown capture and unrecorded waits cannot be reconstructed.".into(),
    })
}
