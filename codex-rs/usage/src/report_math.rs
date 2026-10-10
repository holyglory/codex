use crate::store::UsageStoreError;
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

    pub fn is_finite(self) -> bool {
        self.start_ms != i64::MIN && self.end_ms != i64::MAX
    }

    pub fn start_ms(self) -> i64 {
        self.start_ms
    }

    pub fn end_ms(self) -> i64 {
        self.end_ms
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

pub(crate) fn interval_union_ns(intervals: &[(i64, i64)]) -> Result<u64, UsageStoreError> {
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

pub(crate) fn subtract_intervals(
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

#[cfg(test)]
#[path = "report_math_tests.rs"]
mod tests;
