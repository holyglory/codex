use crate::UtcTimeRange;
use crate::facts::MODEL_REQUEST_CONTEXT_ESTIMATOR;
use crate::store::UsageStore;
use crate::store::UsageStoreError;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

#[path = "task_tree_summary_aggregation.rs"]
mod aggregation;
use aggregation::agent_summaries;
use aggregation::combine_effort;

#[path = "task_tree_summary_internal.rs"]
mod internal;
use internal::*;

#[path = "task_tree_summary_lineage.rs"]
mod lineage;

#[path = "task_tree_summary_types.rs"]
mod types;
pub use types::*;

const MAX_TASK_TREE_THREADS: usize = 64;
const MAX_TASK_TREE_AGENTS: usize = 48;
const MAX_TASK_TREE_OPERATIONS: usize = 100_000;
const MAX_TASK_TREE_FACT_ROWS: usize = 200_000;
const MAX_OPERATION_LINK_VISITS: usize = 4_096;
const NS_PER_MS: u64 = 1_000_000;

impl UsageStore {
    pub async fn task_tree_summary(
        &self,
        query: TaskTreeSummaryQuery,
    ) -> Result<Option<TaskTreeSummary>, UsageStoreError> {
        let thread_ids = self.task_tree_threads(&query).await?;
        if thread_ids.is_empty() {
            return Ok(None);
        }
        let operations = self
            .task_tree_operations(&thread_ids, query.time_range)
            .await?;
        let active_agent_ids = operations
            .iter()
            .filter_map(|operation| operation.agent_id.clone())
            .collect::<HashSet<_>>();
        let agents = self.task_tree_agents(&active_agent_ids).await?;
        let agent_count = agents.len();
        let operation_by_id = operations
            .iter()
            .map(|operation| (operation.id.clone(), operation.clone()))
            .collect::<HashMap<_, _>>();
        let (excluded_wrappers, tool_counts) = self
            .task_tree_tool_roles(&thread_ids, query.time_range)
            .await?;
        let effective_operation_ids = operations
            .iter()
            .filter(|operation| !excluded_wrappers.contains(&operation.id))
            .map(|operation| operation.id.clone())
            .collect::<HashSet<_>>();
        let rework_cache = self.task_tree_rework_flags(&operations).await?;

        let mut agent_effort = agents
            .keys()
            .cloned()
            .map(|id| (Some(id), EffortAccumulator::default()))
            .collect::<BTreeMap<_, _>>();
        let mut first_pass = EffortAccumulator::default();
        let mut rework = EffortAccumulator::default();
        for operation in &operations {
            let interval = clipped_interval(operation, query.time_range)?;
            let agent = agent_effort.entry(operation.agent_id.clone()).or_default();
            add_operation(
                agent,
                operation,
                interval,
                effective_operation_ids.contains(&operation.id),
            )?;
            let stage = if rework_cache.get(&operation.id).copied().unwrap_or(false) {
                &mut rework
            } else {
                &mut first_pass
            };
            add_operation(
                stage,
                operation,
                interval,
                effective_operation_ids.contains(&operation.id),
            )?;
        }
        let mut waits = WaitAccumulators::default();
        for operation in &operations {
            if !matches!(
                operation.activity_state.as_str(),
                "external_wait" | "user_wait" | "blocked_wait"
            ) {
                continue;
            }
            let outcome = wait_outcome(operation);
            let target = waits.get_mut(outcome);
            target.count = target
                .count
                .checked_add(1)
                .ok_or(UsageStoreError::AggregateOverflow)?;
            match clipped_interval(operation, query.time_range)? {
                Some(interval) => target.intervals.push(interval),
                None => increment(&mut target.unknown_intervals)?,
            }
        }

        self.task_tree_tokens(
            &thread_ids,
            query.time_range,
            &operations,
            &operation_by_id,
            &rework_cache,
            &mut agent_effort,
            &mut first_pass,
            &mut rework,
        )
        .await?;
        let context = self
            .task_tree_context(&thread_ids, query.time_range, &operations)
            .await?;
        let totals = combine_effort(&agent_effort)?;
        let agent_summaries = agent_summaries(agents, agent_effort)?;
        let database_schema_version =
            sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
                .fetch_one(&self.pool)
                .await
                .map_err(UsageStoreError::Database)?
                .try_into()
                .map_err(|_| UsageStoreError::DatabaseValueOutOfRange)?;
        Ok(Some(TaskTreeSummary {
            schema_version: TASK_TREE_SUMMARY_SCHEMA_VERSION,
            kind: "taskTreeSummary",
            database_schema_version,
            root_thread_id: query.root_thread_id.as_str().to_string(),
            include_descendants: query.include_descendants,
            time_range: TaskTreeTimeRange {
                start_ms: query.time_range.start_ms(),
                end_ms: query.time_range.end_ms(),
            },
            counts: TaskTreeCounts {
                threads: usize_to_u64(thread_ids.len())?,
                agents: usize_to_u64(agent_count)?,
                raw_operations: usize_to_u64(operations.len())?,
                deduplicated_operations: usize_to_u64(effective_operation_ids.len())?,
                model_requests: usize_to_u64(
                    operations
                        .iter()
                        .filter(|operation| operation.kind == "model_request")
                        .count(),
                )?,
                raw_tool_operations: tool_counts.raw,
                deduplicated_tool_operations: tool_counts
                    .raw
                    .checked_sub(usize_to_u64(excluded_wrappers.len())?)
                    .ok_or(UsageStoreError::AggregateOverflow)?,
                wrapper_tool_operations: tool_counts.wrappers,
                nested_tool_operations: tool_counts.nested,
                unlinked_wrapper_tool_operations: tool_counts.unlinked_wrappers,
                unlinked_nested_tool_operations: tool_counts.unlinked_nested,
            },
            totals,
            agents: agent_summaries,
            waits: waits.finish()?,
            context,
            work: TaskTreeWorkSummary {
                first_pass: finish_effort(first_pass)?,
                post_integration_rework: finish_effort(rework)?,
            },
            formulas: TaskTreeFormulas {
                operation_deduplication: "exclude a wrapper only when its recorded execution group has a nested tool; retain raw and unlinked counts",
                wall_time: "union clipped intervals within each agent or stage; union parallel intervals in task totals",
                tokens: "deduplicate provider total_tokens by factual owner and source event; resolve covered tool tokens to their request",
                context: "content-free estimates of model-visible context assembled at dispatch; older uncaptured requests remain unknown",
                rework: "explicit rework_of operations and their parent/retry descendants are rework; all others are first pass",
                time_window: "clip operation intervals; select token and context facts by observation time in the half-open window",
            },
        }))
    }

    async fn task_tree_threads(
        &self,
        query: &TaskTreeSummaryQuery,
    ) -> Result<Vec<String>, UsageStoreError> {
        let rows = sqlx::query(
            r#"
            WITH RECURSIVE tree(id, depth, path) AS (
                SELECT id, 0, ',' || id || ',' FROM threads WHERE id = ?
                UNION ALL
                SELECT child.id, tree.depth + 1, tree.path || child.id || ','
                FROM threads AS child JOIN tree ON child.parent_thread_id = tree.id
                WHERE ? AND tree.depth < ?
                  AND instr(tree.path, ',' || child.id || ',') = 0
            )
            SELECT id FROM tree ORDER BY depth, id LIMIT ?
            "#,
        )
        .bind(query.root_thread_id.as_str())
        .bind(query.include_descendants)
        .bind(i64::try_from(MAX_TASK_TREE_THREADS).unwrap_or(i64::MAX))
        .bind(i64::try_from(MAX_TASK_TREE_THREADS + 1).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if rows.len() > MAX_TASK_TREE_THREADS {
            return Err(UsageStoreError::TaskTreeTooLarge);
        }
        Ok(rows.into_iter().map(|row| row.get("id")).collect())
    }

    async fn task_tree_agents(
        &self,
        agent_ids: &HashSet<String>,
    ) -> Result<BTreeMap<String, AgentMetadata>, UsageStoreError> {
        if agent_ids.len() > MAX_TASK_TREE_AGENTS {
            return Err(UsageStoreError::TaskTreeTooLarge);
        }
        if agent_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let agent_ids = agent_ids.iter().cloned().collect::<Vec<_>>();
        let mut query =
            QueryBuilder::<Sqlite>::new("SELECT id, role_kind FROM agents WHERE id IN (");
        push_string_binds(&mut query, &agent_ids);
        query.push(" ) ORDER BY created_at_ms, id LIMIT ");
        query.push_bind(i64::try_from(MAX_TASK_TREE_AGENTS + 1).unwrap_or(i64::MAX));
        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
        if rows.len() > MAX_TASK_TREE_AGENTS {
            return Err(UsageStoreError::TaskTreeTooLarge);
        }
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.get("id"),
                    AgentMetadata {
                        role: row.get("role_kind"),
                    },
                )
            })
            .collect())
    }

    async fn task_tree_operations(
        &self,
        thread_ids: &[String],
        range: UtcTimeRange,
    ) -> Result<Vec<OperationRow>, UsageStoreError> {
        let mut query = QueryBuilder::<Sqlite>::new(
            r#"SELECT operation.id, operation.parent_operation_id,
                      operation.retry_of_operation_id, operation.rework_of_operation_id,
                      operation.operation_kind, operation.agent_id,
                      operation.started_at_ms, operation.activity_state,
                      terminal.occurred_at_ms AS ended_at_ms,
                      terminal.event_kind AS terminal_status, terminal.error_category,
                      request.id AS model_request_id
               FROM operations AS operation
               LEFT JOIN operation_events AS terminal
                 ON terminal.operation_id = operation.id AND terminal.terminal = 1
               LEFT JOIN model_requests AS request ON request.operation_id = operation.id
               WHERE operation.thread_id IN ("#,
        );
        push_string_binds(&mut query, thread_ids);
        query
            .push(") AND operation.started_at_ms < ")
            .push_bind(range.end_ms())
            .push(" AND (terminal.occurred_at_ms IS NULL OR terminal.occurred_at_ms > ")
            .push_bind(range.start_ms())
            .push(") ORDER BY operation.started_at_ms, operation.id LIMIT ")
            .push_bind(i64::try_from(MAX_TASK_TREE_OPERATIONS + 1).unwrap_or(i64::MAX));
        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
        if rows.len() > MAX_TASK_TREE_OPERATIONS {
            return Err(UsageStoreError::TaskTreeTooLarge);
        }
        Ok(rows
            .into_iter()
            .map(|row| OperationRow {
                id: row.get("id"),
                parent_operation_id: row.get("parent_operation_id"),
                retry_of_operation_id: row.get("retry_of_operation_id"),
                rework_of_operation_id: row.get("rework_of_operation_id"),
                kind: row.get("operation_kind"),
                agent_id: row.get("agent_id"),
                started_at_ms: row.get("started_at_ms"),
                ended_at_ms: row.get("ended_at_ms"),
                terminal_status: row.get("terminal_status"),
                error_category: row.get("error_category"),
                activity_state: row.get("activity_state"),
                model_request_id: row.get("model_request_id"),
            })
            .collect())
    }
}

#[derive(Default)]
struct ToolCounts {
    raw: u64,
    wrappers: u64,
    nested: u64,
    unlinked_wrappers: u64,
    unlinked_nested: u64,
}

#[derive(Default)]
struct WaitAccumulators {
    completed: OutcomeAccumulator,
    intentional_expiry: OutcomeAccumulator,
    failed: OutcomeAccumulator,
    cancelled: OutcomeAccumulator,
    unknown: OutcomeAccumulator,
}

#[derive(Clone, Copy)]
enum WaitOutcome {
    Completed,
    IntentionalExpiry,
    Failed,
    Cancelled,
    Unknown,
}

impl WaitAccumulators {
    fn get_mut(&mut self, outcome: WaitOutcome) -> &mut OutcomeAccumulator {
        match outcome {
            WaitOutcome::Completed => &mut self.completed,
            WaitOutcome::IntentionalExpiry => &mut self.intentional_expiry,
            WaitOutcome::Failed => &mut self.failed,
            WaitOutcome::Cancelled => &mut self.cancelled,
            WaitOutcome::Unknown => &mut self.unknown,
        }
    }

    fn finish(self) -> Result<TaskTreeWaitSummary, UsageStoreError> {
        Ok(TaskTreeWaitSummary {
            completed: finish_outcome(self.completed)?,
            intentional_expiry: finish_outcome(self.intentional_expiry)?,
            failed: finish_outcome(self.failed)?,
            cancelled: finish_outcome(self.cancelled)?,
            unknown: finish_outcome(self.unknown)?,
        })
    }
}

fn wait_outcome(operation: &OperationRow) -> WaitOutcome {
    match operation.terminal_status.as_deref() {
        Some("completed") => WaitOutcome::Completed,
        Some("timed_out") if operation.error_category.is_none() => WaitOutcome::IntentionalExpiry,
        Some("cancelled" | "interrupted") => WaitOutcome::Cancelled,
        Some("failed" | "incomplete" | "denied" | "timed_out") => WaitOutcome::Failed,
        Some(_) | None => WaitOutcome::Unknown,
    }
}

fn add_operation(
    effort: &mut EffortAccumulator,
    operation: &OperationRow,
    interval: Option<(i64, i64)>,
    effective: bool,
) -> Result<(), UsageStoreError> {
    if effective {
        increment(&mut effort.operations)?;
        if operation.kind == "model_request" {
            increment(&mut effort.model_requests)?;
        }
    }
    match interval {
        Some(interval) => effort.intervals.push(interval),
        None => increment(&mut effort.unknown_intervals)?,
    }
    Ok(())
}

fn clipped_interval(
    operation: &OperationRow,
    range: UtcTimeRange,
) -> Result<Option<(i64, i64)>, UsageStoreError> {
    let Some(end) = operation.ended_at_ms else {
        return Ok(None);
    };
    if end < operation.started_at_ms {
        return Err(UsageStoreError::InvalidFact);
    }
    let start = operation.started_at_ms.max(range.start_ms());
    let end = end.min(range.end_ms());
    Ok((start <= end).then_some((start, end)))
}

fn finish_effort(value: EffortAccumulator) -> Result<TaskTreeEffort, UsageStoreError> {
    Ok(TaskTreeEffort {
        operations: value.operations,
        model_requests: value.model_requests,
        provider_total_tokens: finish_tokens(value.tokens),
        wall_time: finish_duration(value.intervals, value.unknown_intervals)?,
    })
}

fn finish_tokens(value: TokenAccumulator) -> TaskTreeTokenAggregate {
    TaskTreeTokenAggregate {
        measured_tokens: value.measured_tokens,
        exact_tokens: (!value.has_gap && value.unknown_observations == 0)
            .then_some(value.measured_tokens),
        unknown_observations: value.unknown_observations,
    }
}

fn finish_outcome(value: OutcomeAccumulator) -> Result<TaskTreeOutcomeAggregate, UsageStoreError> {
    Ok(TaskTreeOutcomeAggregate {
        count: value.count,
        wall_time: finish_duration(value.intervals, value.unknown_intervals)?,
    })
}

fn finish_duration(
    mut intervals: Vec<(i64, i64)>,
    unknown_intervals: u64,
) -> Result<TaskTreeDuration, UsageStoreError> {
    intervals.sort_unstable();
    let mut measured_ns = 0_u64;
    if let Some(mut current) = intervals.first().copied() {
        for interval in intervals.into_iter().skip(1) {
            if interval.0 <= current.1 {
                current.1 = current.1.max(interval.1);
            } else {
                measured_ns = measured_ns
                    .checked_add(interval_ns(current)?)
                    .ok_or(UsageStoreError::AggregateOverflow)?;
                current = interval;
            }
        }
        measured_ns = measured_ns
            .checked_add(interval_ns(current)?)
            .ok_or(UsageStoreError::AggregateOverflow)?;
    }
    Ok(TaskTreeDuration {
        measured_ns,
        exact_ns: (unknown_intervals == 0).then_some(measured_ns),
        unknown_intervals,
    })
}

fn interval_ns((start, end): (i64, i64)) -> Result<u64, UsageStoreError> {
    u64::try_from(i128::from(end) - i128::from(start))
        .map_err(|_| UsageStoreError::AggregateOverflow)?
        .checked_mul(NS_PER_MS)
        .ok_or(UsageStoreError::AggregateOverflow)
}

fn increment(value: &mut u64) -> Result<(), UsageStoreError> {
    *value = value
        .checked_add(1)
        .ok_or(UsageStoreError::AggregateOverflow)?;
    Ok(())
}

fn push_string_binds(query: &mut QueryBuilder<Sqlite>, values: &[String]) {
    let mut separated = query.separated(", ");
    for value in values {
        separated.push_bind(value);
    }
}

fn usize_to_u64(value: usize) -> Result<u64, UsageStoreError> {
    value
        .try_into()
        .map_err(|_| UsageStoreError::AggregateOverflow)
}

#[cfg(test)]
#[path = "task_tree_summary_tests.rs"]
mod tests;
