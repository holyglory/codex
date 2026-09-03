use crate::report::UsageSummaryScope;
use crate::store::UsageStore;
use crate::store::UsageStoreError;
use crate::types::AccountProfileRef;
use sqlx::Row;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use thiserror::Error;

const NS_PER_MS: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UtcTimeRange {
    start_ms: i64,
    end_ms: i64,
}

impl UtcTimeRange {
    pub fn new(start_ms: i64, end_ms: i64) -> Result<Self, UtcTimeRangeError> {
        (start_ms < end_ms)
            .then_some(Self { start_ms, end_ms })
            .ok_or(UtcTimeRangeError)
    }

    pub fn start_ms(self) -> i64 {
        self.start_ms
    }

    pub fn end_ms(self) -> i64 {
        self.end_ms
    }

    pub(crate) fn contains(self, timestamp_ms: i64) -> bool {
        timestamp_ms >= self.start_ms && timestamp_ms < self.end_ms
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("UTC usage time range must be a nonempty half-open interval")]
pub struct UtcTimeRangeError;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DurationAggregate {
    pub measured_ns: u64,
    pub exact_ns: Option<u64>,
    pub unknown_intervals: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedDuration {
    pub name: String,
    pub duration: DurationAggregate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolOutcomeCount {
    pub outcome: String,
    pub count: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolMetrics {
    pub count: u64,
    pub duration: DurationAggregate,
    pub outcomes: Vec<ToolOutcomeCount>,
    pub duration_basis: &'static str,
}

#[derive(Default)]
struct IntervalGroup {
    intervals: Vec<(i64, i64)>,
    unknown_intervals: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenActivityAggregate {
    pub phase: String,
    pub activity: String,
    pub attribution_provenance: String,
    pub measured_tokens: i64,
    pub exact_tokens: Option<i64>,
    pub unknown_observations: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParticipationCounts {
    pub operation_count: u64,
    pub tool_count: u64,
    pub additive: bool,
    pub label: &'static str,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReportTimeMetrics {
    pub request_to_delivery_wall: DurationAggregate,
    pub execution_wall_union: DurationAggregate,
    pub phase_interval_unions: Vec<NamedDuration>,
    pub activity_state_interval_unions: Vec<NamedDuration>,
    pub summed_per_agent_active: DurationAggregate,
}

#[derive(Clone)]
pub(crate) struct OperationLifecycle {
    pub id: String,
    pub kind: String,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub terminal_status: Option<String>,
    pub agent_id: Option<String>,
    pub phase: String,
    pub activity: String,
    pub activity_state: String,
    pub attribution_provenance: String,
}

pub(crate) struct ReportSelection {
    pub scope: UsageSummaryScope,
    pub time_range: Option<UtcTimeRange>,
    pub operations: Vec<OperationLifecycle>,
    operation_indexes: HashMap<String, usize>,
    uses_report_cache: bool,
}

impl ReportSelection {
    fn new(
        scope: UsageSummaryScope,
        time_range: Option<UtcTimeRange>,
        operations: Vec<OperationLifecycle>,
        uses_report_cache: bool,
    ) -> Self {
        let operation_indexes = operations
            .iter()
            .enumerate()
            .map(|(index, operation)| (operation.id.clone(), index))
            .collect();
        Self {
            scope,
            time_range,
            operations,
            operation_indexes,
            uses_report_cache,
        }
    }

    pub fn contains_operation(&self, operation_id: &str) -> bool {
        self.operation_indexes.contains_key(operation_id)
    }

    pub fn contains_timestamp(&self, timestamp_ms: i64) -> bool {
        self.time_range
            .is_none_or(|range| range.contains(timestamp_ms))
    }

    pub fn classification(&self, operation_id: &str) -> Option<(&str, &str, &str)> {
        self.operation_indexes
            .get(operation_id)
            .and_then(|index| self.operations.get(*index))
            .map(|operation| {
                (
                    operation.phase.as_str(),
                    operation.activity.as_str(),
                    operation.attribution_provenance.as_str(),
                )
            })
    }

    pub fn uses_report_cache(&self) -> bool {
        self.uses_report_cache
    }
}

impl UsageStore {
    pub(crate) async fn build_report_selection(
        &self,
        scope: UsageSummaryScope,
        time_range: Option<UtcTimeRange>,
        repository_family: Option<&HashSet<String>>,
        account_profile_ref: Option<&AccountProfileRef>,
    ) -> Result<ReportSelection, UsageStoreError> {
        let attributed_operations = self
            .attributed_operation_ids_for_math(repository_family)
            .await?;
        let effective = self.effective_classifications().await?;
        let rows = sqlx::query(
            r#"
            SELECT operation.id, operation.operation_kind, operation.thread_id,
                   operation.agent_id, operation.started_at_ms, operation.phase,
                   operation.activity, operation.activity_state,
                   operation.attribution_provenance,
                   terminal.occurred_at_ms, terminal.event_kind,
                   COALESCE(request.account_profile_ref,
                            parent_request.account_profile_ref,
                            turn.account_profile_ref)
                       AS account_profile_ref
            FROM operations AS operation
            LEFT JOIN operation_events AS terminal
              ON terminal.operation_id = operation.id AND terminal.terminal = 1
            LEFT JOIN model_requests AS request ON request.operation_id = operation.id
            LEFT JOIN model_requests AS parent_request
              ON parent_request.operation_id = operation.parent_operation_id
            LEFT JOIN turns AS turn ON turn.id = operation.turn_id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let mut operations = Vec::new();
        for row in rows {
            let id: String = row.get("id");
            let thread_id: Option<String> = row.get("thread_id");
            let started_at_ms: i64 = row.get("started_at_ms");
            let ended_at_ms: Option<i64> = row.get("occurred_at_ms");
            let scope_matches = match &scope {
                UsageSummaryScope::All => true,
                UsageSummaryScope::Thread(expected) => {
                    thread_id.as_deref() == Some(expected.as_str())
                }
                UsageSummaryScope::Repository(_) => attributed_operations.contains(&id),
            };
            let repository_matches =
                repository_family.is_none() || attributed_operations.contains(&id);
            let account_matches = account_profile_ref.is_none_or(|expected| {
                row.get::<Option<String>, _>("account_profile_ref")
                    .as_deref()
                    == Some(expected.as_str())
            });
            if !scope_matches
                || !repository_matches
                || !account_matches
                || !interval_may_overlap(started_at_ms, ended_at_ms, time_range)
            {
                continue;
            }
            let (phase, activity, state, provenance) =
                effective.get(&id).cloned().unwrap_or_else(|| {
                    (
                        row.get("phase"),
                        row.get("activity"),
                        row.get("activity_state"),
                        row.get("attribution_provenance"),
                    )
                });
            operations.push(OperationLifecycle {
                id,
                kind: row.get("operation_kind"),
                started_at_ms,
                ended_at_ms,
                terminal_status: row.get("event_kind"),
                agent_id: row.get("agent_id"),
                phase,
                activity,
                activity_state: state,
                attribution_provenance: provenance,
            });
        }
        Ok(ReportSelection::new(
            scope, time_range, operations, /*uses_report_cache*/ false,
        ))
    }

    pub(crate) async fn build_cached_all_report_selection(
        &self,
    ) -> Result<ReportSelection, UsageStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT operation_id, operation_kind, agent_id, started_at_ms,
                   ended_at_ms, terminal_status, phase, activity,
                   activity_state, attribution_provenance
            FROM _usage_report_operations
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let operations = rows
            .into_iter()
            .map(|row| OperationLifecycle {
                id: row.get("operation_id"),
                kind: row.get("operation_kind"),
                started_at_ms: row.get("started_at_ms"),
                ended_at_ms: row.get("ended_at_ms"),
                terminal_status: row.get("terminal_status"),
                agent_id: row.get("agent_id"),
                phase: row.get("phase"),
                activity: row.get("activity"),
                activity_state: row.get("activity_state"),
                attribution_provenance: row.get("attribution_provenance"),
            })
            .collect();
        Ok(ReportSelection::new(
            UsageSummaryScope::All,
            /*time_range*/ None,
            operations,
            /*uses_report_cache*/ true,
        ))
    }

    pub(crate) async fn report_time_metrics(
        &self,
        selection: &ReportSelection,
    ) -> Result<(ReportTimeMetrics, ToolMetrics), UsageStoreError> {
        let mut execution_intervals = Vec::new();
        let mut request_intervals = Vec::new();
        let mut phase_intervals = BTreeMap::<String, IntervalGroup>::new();
        let mut state_intervals = BTreeMap::<String, IntervalGroup>::new();
        let mut agent_intervals = BTreeMap::<String, Vec<(i64, i64)>>::new();
        let mut execution_unknown = 0_u64;
        let mut request_unknown = 0_u64;
        let mut agent_unknown = 0_u64;
        let mut wait_intervals = HashMap::<String, Vec<(String, (i64, i64))>>::new();
        let mut operations_with_unknown_wait = HashSet::new();
        let span_query = if selection.uses_report_cache() {
            r#"
            SELECT operation_id, activity_state, started_at_ms, ended_at_ms
            FROM _usage_report_spans
            "#
        } else {
            r#"
            SELECT span.operation_id, span.activity_state, span.started_at_ms,
                   ended.occurred_at_ms AS ended_at_ms
            FROM activity_spans AS span
            LEFT JOIN activity_span_events AS ended
              ON ended.activity_span_id = span.id AND ended.event_kind = 'ended'
            "#
        };
        for row in sqlx::query(span_query)
            .fetch_all(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?
        {
            let operation_id: String = row.get("operation_id");
            if !selection.contains_operation(&operation_id) {
                continue;
            }
            let state: String = row.get("activity_state");
            let lifecycle = OperationLifecycle {
                id: String::new(),
                kind: String::new(),
                agent_id: None,
                started_at_ms: row.get("started_at_ms"),
                ended_at_ms: row.get("ended_at_ms"),
                terminal_status: None,
                phase: String::new(),
                activity: String::new(),
                activity_state: state.clone(),
                attribution_provenance: String::new(),
            };
            if let Some(interval) = completed_interval(&lifecycle, selection.time_range)? {
                state_intervals
                    .entry(state.clone())
                    .or_default()
                    .intervals
                    .push(interval);
                wait_intervals
                    .entry(operation_id)
                    .or_default()
                    .push((state, interval));
            } else {
                increment(&mut state_intervals.entry(state).or_default().unknown_intervals)?;
                operations_with_unknown_wait.insert(operation_id);
            }
        }
        for operation in &selection.operations {
            let counts_as_agent_active = !matches!(
                operation.activity_state.as_str(),
                "external_wait" | "user_wait" | "blocked_wait"
            );
            let Some(interval) = completed_interval(operation, selection.time_range)? else {
                increment(&mut execution_unknown)?;
                increment(
                    &mut phase_intervals
                        .entry(operation.phase.clone())
                        .or_default()
                        .unknown_intervals,
                )?;
                increment(
                    &mut state_intervals
                        .entry(operation.activity_state.clone())
                        .or_default()
                        .unknown_intervals,
                )?;
                if operation.kind == "model_request" {
                    increment(&mut request_unknown)?;
                }
                if counts_as_agent_active {
                    increment(&mut agent_unknown)?;
                }
                continue;
            };
            execution_intervals.push(interval);
            phase_intervals
                .entry(operation.phase.clone())
                .or_default()
                .intervals
                .push(interval);
            let nested_waits = wait_intervals
                .get(&operation.id)
                .map(|spans| {
                    spans
                        .iter()
                        .map(|(_, interval)| *interval)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let active_segments = if counts_as_agent_active {
                subtract_intervals(interval, &nested_waits)?
            } else {
                Vec::new()
            };
            let parent_state_intervals = if counts_as_agent_active {
                active_segments.as_slice()
            } else {
                std::slice::from_ref(&interval)
            };
            state_intervals
                .entry(operation.activity_state.clone())
                .or_default()
                .intervals
                .extend_from_slice(parent_state_intervals);
            if counts_as_agent_active {
                if operations_with_unknown_wait.contains(&operation.id) {
                    increment(&mut agent_unknown)?;
                } else if let Some(agent_id) = &operation.agent_id {
                    agent_intervals
                        .entry(agent_id.clone())
                        .or_default()
                        .extend(active_segments);
                } else {
                    increment(&mut agent_unknown)?;
                }
            }
            if operation.kind == "model_request" {
                request_intervals.push(interval);
            }
        }
        if request_intervals.is_empty() && request_unknown == 0 {
            request_unknown = 1;
        }
        let request_measured = enclosing_span_ns(&request_intervals)?;
        let execution_measured = interval_union_ns(&execution_intervals)?;
        let mut agent_measured = 0_u64;
        for intervals in agent_intervals.values() {
            agent_measured = agent_measured
                .checked_add(interval_union_ns(intervals)?)
                .ok_or(UsageStoreError::AggregateOverflow)?;
        }
        let metrics = ReportTimeMetrics {
            request_to_delivery_wall: duration(request_measured, request_unknown),
            execution_wall_union: duration(execution_measured, execution_unknown),
            phase_interval_unions: named_unions(phase_intervals)?,
            activity_state_interval_unions: named_unions(state_intervals)?,
            summed_per_agent_active: duration(agent_measured, agent_unknown),
        };
        let tools = tool_metrics(selection)?;
        Ok((metrics, tools))
    }

    async fn effective_classifications(
        &self,
    ) -> Result<HashMap<String, (String, String, String, String)>, UsageStoreError> {
        let rows = sqlx::query_as::<_, (String, String, String, String, String)>(
            r#"
            SELECT operation_id, phase, activity, activity_state, provenance
            FROM effective_classification_events
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(rows
            .into_iter()
            .map(|(id, phase, activity, state, provenance)| {
                (id, (phase, activity, state, provenance))
            })
            .collect())
    }

    async fn attributed_operation_ids_for_math(
        &self,
        repository_family: Option<&HashSet<String>>,
    ) -> Result<HashSet<String>, UsageStoreError> {
        let Some(repository_family) = repository_family else {
            return Ok(HashSet::new());
        };
        let rows = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT operation_id, repository_id FROM repository_attributions",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(rows
            .into_iter()
            .filter_map(|(operation, repository)| {
                repository
                    .is_some_and(|repository| repository_family.contains(&repository))
                    .then_some(operation)
            })
            .collect())
    }
}

fn tool_metrics(selection: &ReportSelection) -> Result<ToolMetrics, UsageStoreError> {
    let mut count = 0_u64;
    let mut measured_ns = 0_u64;
    let mut unknown_intervals = 0_u64;
    let mut outcomes = BTreeMap::<String, u64>::new();
    for operation in selection.operations.iter().filter(|operation| {
        matches!(
            operation.kind.as_str(),
            "local_tool" | "hosted_tool" | "activity_control"
        )
    }) {
        increment(&mut count)?;
        let outcome = operation
            .terminal_status
            .as_deref()
            .unwrap_or("unknown")
            .to_string();
        increment(outcomes.entry(outcome).or_default())?;
        match completed_interval(operation, selection.time_range)? {
            Some(interval) => {
                measured_ns = measured_ns
                    .checked_add(interval_duration_ns(interval)?)
                    .ok_or(UsageStoreError::AggregateOverflow)?;
            }
            None => increment(&mut unknown_intervals)?,
        }
    }
    Ok(ToolMetrics {
        count,
        duration: duration(measured_ns, unknown_intervals),
        outcomes: outcomes
            .into_iter()
            .map(|(outcome, count)| ToolOutcomeCount { outcome, count })
            .collect(),
        duration_basis: "sum of clipped UTC wall intervals; lifecycle facts retain full monotonic duration",
    })
}

fn interval_may_overlap(start: i64, end: Option<i64>, range: Option<UtcTimeRange>) -> bool {
    range.is_none_or(|range| start < range.end_ms && end.is_none_or(|end| end > range.start_ms))
}

fn completed_interval(
    operation: &OperationLifecycle,
    range: Option<UtcTimeRange>,
) -> Result<Option<(i64, i64)>, UsageStoreError> {
    let Some(end) = operation.ended_at_ms else {
        return Ok(None);
    };
    if end < operation.started_at_ms {
        return Ok(None);
    }
    let mut interval = (operation.started_at_ms, end);
    if let Some(range) = range {
        interval.0 = interval.0.max(range.start_ms);
        interval.1 = interval.1.min(range.end_ms);
    }
    Ok((interval.0 <= interval.1).then_some(interval))
}

fn named_unions(
    groups: BTreeMap<String, IntervalGroup>,
) -> Result<Vec<NamedDuration>, UsageStoreError> {
    groups
        .into_iter()
        .map(|(name, group)| {
            let measured = interval_union_ns(&group.intervals)?;
            Ok(NamedDuration {
                name,
                duration: duration(measured, group.unknown_intervals),
            })
        })
        .collect()
}

fn enclosing_span_ns(intervals: &[(i64, i64)]) -> Result<u64, UsageStoreError> {
    let Some(start) = intervals.iter().map(|interval| interval.0).min() else {
        return Ok(0);
    };
    let end = intervals
        .iter()
        .map(|interval| interval.1)
        .max()
        .unwrap_or(start);
    interval_duration_ns((start, end))
}

fn interval_union_ns(intervals: &[(i64, i64)]) -> Result<u64, UsageStoreError> {
    let mut intervals = intervals.to_vec();
    intervals.sort_unstable();
    let mut total = 0_u64;
    let Some(mut current) = intervals.first().copied() else {
        return Ok(0);
    };
    for interval in intervals.into_iter().skip(1) {
        if interval.0 <= current.1 {
            current.1 = current.1.max(interval.1);
        } else {
            total = total
                .checked_add(interval_duration_ns(current)?)
                .ok_or(UsageStoreError::AggregateOverflow)?;
            current = interval;
        }
    }
    total
        .checked_add(interval_duration_ns(current)?)
        .ok_or(UsageStoreError::AggregateOverflow)
}

fn interval_duration_ns(interval: (i64, i64)) -> Result<u64, UsageStoreError> {
    let milliseconds = i128::from(interval.1) - i128::from(interval.0);
    let milliseconds =
        u64::try_from(milliseconds).map_err(|_| UsageStoreError::AggregateOverflow)?;
    milliseconds
        .checked_mul(NS_PER_MS)
        .ok_or(UsageStoreError::AggregateOverflow)
}

fn subtract_intervals(
    base: (i64, i64),
    exclusions: &[(i64, i64)],
) -> Result<Vec<(i64, i64)>, UsageStoreError> {
    let mut exclusions = exclusions
        .iter()
        .map(|(start, end)| ((*start).max(base.0), (*end).min(base.1)))
        .filter(|(start, end)| start <= end)
        .collect::<Vec<_>>();
    exclusions.sort_unstable();
    let mut result = Vec::new();
    let mut cursor = base.0;
    for (start, end) in exclusions {
        if start > cursor {
            result.push((cursor, start));
        }
        cursor = cursor.max(end);
    }
    if cursor < base.1 {
        result.push((cursor, base.1));
    }
    if result.iter().any(|interval| interval.0 > interval.1) {
        return Err(UsageStoreError::AggregateOverflow);
    }
    Ok(result)
}

fn duration(measured_ns: u64, unknown_intervals: u64) -> DurationAggregate {
    DurationAggregate {
        measured_ns,
        exact_ns: (unknown_intervals == 0).then_some(measured_ns),
        unknown_intervals,
    }
}

fn increment(value: &mut u64) -> Result<(), UsageStoreError> {
    *value = value
        .checked_add(1)
        .ok_or(UsageStoreError::AggregateOverflow)?;
    Ok(())
}

#[cfg(test)]
#[path = "report_math_tests.rs"]
mod tests;
