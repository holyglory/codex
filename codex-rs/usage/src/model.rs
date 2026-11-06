use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Result;
use crate::error::StorageError;

/// Number of seconds in an hourly segment.
pub const HOUR_SECONDS: i64 = 60 * 60;

/// Monotonic position assigned to an entry in the append-only journal.
pub type JournalSequence = u64;

/// Generation assigned to a materialized snapshot.
pub type SnapshotGeneration = u64;

/// The freshness state of the latest snapshot or upstream usage read.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// No successful freshness observation exists yet.
    #[default]
    Unknown,
    /// The snapshot is current according to its producer.
    Fresh,
    /// The snapshot is usable but may lag its producer.
    Stale,
}

/// Observed terminal state of an operation; absence means no terminal fact yet.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageOutcome {
    /// The operation completed successfully.
    Completed,
    /// The operation reported failure.
    Failed,
    /// Admission or policy prevented execution.
    Blocked,
    /// Cancellation stopped the operation.
    Cancelled,
    /// Execution ended without an observed complete result.
    Incomplete,
}

impl UsageOutcome {
    /// Stable content-free representation used by columnar facts and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Incomplete => "incomplete",
        }
    }
}

impl std::str::FromStr for UsageOutcome {
    type Err = StorageError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "blocked" => Ok(Self::Blocked),
            "cancelled" => Ok(Self::Cancelled),
            "incomplete" => Ok(Self::Incomplete),
            _ => Err(StorageError::InvalidData(
                "unsupported usage outcome".into(),
            )),
        }
    }
}

/// One usage observation stored in the journal and hourly segments.
///
/// Token values are unsigned because observations describe work that happened.
/// Corrections are represented separately by [`UsageCorrection`] and use signed
/// deltas so a correction never rewrites an existing observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageRecord {
    /// Stable id used for replay deduplication and correction targeting.
    pub id: String,
    /// Unix timestamp in seconds at which the observation was made.
    pub observed_at: i64,
    /// Exact Unix millisecond observation time, when supplied by the producer.
    #[serde(default)]
    pub observed_at_ms: Option<i64>,
    /// Optional account or profile identity supplied by the producer.
    #[serde(default)]
    pub account_id: Option<String>,
    /// Optional operation identity for grouping tool and model work.
    #[serde(default)]
    pub operation_id: Option<String>,
    /// Optional thread identity used by current-chat and task-tree queries.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Optional repository identity used by repository-scoped queries.
    #[serde(default)]
    pub repository_id: Option<String>,
    /// Optional model/provider label supplied by the producer.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional tool name for tool-operation records.
    #[serde(default)]
    pub tool: Option<String>,
    /// Optional activity taxonomy value.
    #[serde(default)]
    pub activity: Option<String>,
    /// Optional phase taxonomy value.
    #[serde(default)]
    pub phase: Option<String>,
    /// Optional classification/provenance value.
    #[serde(default)]
    pub classification: Option<String>,
    /// Optional event kind for lifecycle/event records.
    #[serde(default)]
    pub event: Option<String>,
    /// Optional coverage state for event and token observations.
    #[serde(default)]
    pub coverage: Option<String>,
    /// Optional operation start timestamp in Unix seconds.
    #[serde(default)]
    pub started_at: Option<i64>,
    /// Exact Unix millisecond start time, when supplied by the producer.
    #[serde(default)]
    pub started_at_ms: Option<i64>,
    /// Optional operation completion timestamp in Unix seconds.
    #[serde(default)]
    pub ended_at: Option<i64>,
    /// Exact Unix millisecond completion time, when supplied by the producer.
    #[serde(default)]
    pub ended_at_ms: Option<i64>,
    /// Number of input tokens in the observation.
    pub input_tokens: u64,
    /// Number of cached input tokens in the observation.
    pub cached_input_tokens: u64,
    /// Number of output tokens in the observation.
    pub output_tokens: u64,
    /// Number of reasoning output tokens in the observation.
    pub reasoning_output_tokens: u64,
    /// Trusted parent thread recorded by the runtime graph.
    #[serde(default)]
    pub parent_thread_id: Option<String>,
    /// Turn owning the observation.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Provider response identity linking request and token observations.
    #[serde(default)]
    pub response_id: Option<String>,
    /// Observed terminal outcome, without provider or tool result content.
    #[serde(default)]
    pub outcome: Option<UsageOutcome>,
    /// Measured elapsed execution time, excluding inferred durations.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Actual transport attempt number when supplied by the runtime.
    #[serde(default)]
    pub attempt_number: Option<u64>,
    /// Provider-reported total, without summing potentially overlapping components.
    #[serde(default)]
    pub provider_total_tokens: Option<u64>,
    /// Provider-reported cache-write input tokens when available.
    #[serde(default)]
    pub cache_write_input_tokens: Option<u64>,
    /// Count of observations lost before durable capture, for explicit coverage gaps.
    #[serde(default)]
    pub missing_observations: Option<u64>,
    /// Prior model operation named by an explicit redo declaration.
    #[serde(default)]
    pub rework_of_operation_id: Option<String>,
}

impl UsageRecord {
    /// Construct an observation with the common input/output token counts.
    pub fn new(
        id: impl Into<String>,
        observed_at: i64,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Self {
        Self {
            id: id.into(),
            observed_at,
            observed_at_ms: None,
            account_id: None,
            operation_id: None,
            thread_id: None,
            repository_id: None,
            model: None,
            tool: None,
            activity: None,
            phase: None,
            classification: None,
            event: None,
            coverage: None,
            started_at: None,
            started_at_ms: None,
            ended_at: None,
            ended_at_ms: None,
            input_tokens,
            cached_input_tokens: 0,
            output_tokens,
            reasoning_output_tokens: 0,
            parent_thread_id: None,
            turn_id: None,
            response_id: None,
            outcome: None,
            duration_ms: None,
            attempt_number: None,
            provider_total_tokens: None,
            cache_write_input_tokens: None,
            missing_observations: None,
            rework_of_operation_id: None,
        }
    }

    /// Construct an observation from one exact UTC millisecond timestamp.
    pub fn new_millis(
        id: impl Into<String>,
        observed_at_ms: i64,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Self {
        let mut record = Self::new(
            id,
            observed_at_ms.div_euclid(1_000),
            input_tokens,
            output_tokens,
        );
        record.observed_at_ms = Some(observed_at_ms);
        record
    }

    /// Read exact observation time, with a saturating fallback for legacy seconds.
    pub fn effective_observed_at_ms(&self) -> i64 {
        self.observed_at_ms
            .unwrap_or_else(|| self.observed_at.saturating_mul(1_000))
    }

    /// Read exact start time, with a saturating fallback for legacy seconds.
    pub fn effective_started_at_ms(&self) -> Option<i64> {
        self.started_at_ms
            .or_else(|| self.started_at.map(|value| value.saturating_mul(1_000)))
    }

    /// Read exact completion time, with a saturating fallback for legacy seconds.
    pub fn effective_ended_at_ms(&self) -> Option<i64> {
        self.ended_at_ms
            .or_else(|| self.ended_at.map(|value| value.saturating_mul(1_000)))
    }

    /// Attach an account/profile identity to the observation.
    pub fn with_account(mut self, account_id: impl Into<String>) -> Self {
        self.account_id = Some(account_id.into());
        self
    }

    /// Attach an operation identity to this observation.
    pub fn with_operation(mut self, operation_id: impl Into<String>) -> Self {
        self.operation_id = Some(operation_id.into());
        self
    }

    /// Attach a thread identity to this observation.
    pub fn with_thread(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = Some(thread_id.into());
        self
    }

    /// Attach a repository identity to this observation.
    pub fn with_repository(mut self, repository_id: impl Into<String>) -> Self {
        self.repository_id = Some(repository_id.into());
        self
    }

    /// Attach a tool name to this observation.
    pub fn with_tool(mut self, tool: impl Into<String>) -> Self {
        self.tool = Some(tool.into());
        self
    }

    /// Attach activity and phase taxonomy values to this observation.
    pub fn with_activity(mut self, activity: impl Into<String>, phase: impl Into<String>) -> Self {
        self.activity = Some(activity.into());
        self.phase = Some(phase.into());
        self
    }

    /// Attach a model/provider label to the observation.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Return this observation as signed aggregate values.
    pub fn totals(&self) -> UsageTotals {
        UsageTotals {
            input_tokens: self.input_tokens as i64,
            cached_input_tokens: self.cached_input_tokens as i64,
            output_tokens: self.output_tokens as i64,
            reasoning_output_tokens: self.reasoning_output_tokens as i64,
            records: 1,
        }
    }
}

/// A correction appended after an observation was materialized.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageCorrection {
    /// Stable id of this correction entry.
    pub id: String,
    /// Id of the observation to which the delta applies.
    pub record_id: String,
    /// Unix timestamp at which the correction was recorded.
    pub observed_at: i64,
    /// Signed input-token delta.
    pub input_tokens_delta: i64,
    /// Signed cached-input-token delta.
    pub cached_input_tokens_delta: i64,
    /// Signed output-token delta.
    pub output_tokens_delta: i64,
    /// Signed reasoning-output-token delta.
    pub reasoning_output_tokens_delta: i64,
    /// Human-readable producer reason for the correction.
    pub reason: String,
}

impl UsageCorrection {
    /// Construct a correction with the common input/output token deltas.
    pub fn new(
        id: impl Into<String>,
        record_id: impl Into<String>,
        observed_at: i64,
        input_tokens_delta: i64,
        output_tokens_delta: i64,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            record_id: record_id.into(),
            observed_at,
            input_tokens_delta,
            cached_input_tokens_delta: 0,
            output_tokens_delta,
            reasoning_output_tokens_delta: 0,
            reason: reason.into(),
        }
    }

    /// Return this correction as signed aggregate values.
    pub fn totals(&self) -> UsageTotals {
        UsageTotals {
            input_tokens: self.input_tokens_delta,
            cached_input_tokens: self.cached_input_tokens_delta,
            output_tokens: self.output_tokens_delta,
            reasoning_output_tokens: self.reasoning_output_tokens_delta,
            records: 0,
        }
    }
}

/// Optional dimensions used to narrow an in-memory query.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageFilter {
    /// Restrict results to one account/profile identity.
    pub account_id: Option<String>,
    /// Restrict results to one thread identity.
    pub thread_id: Option<String>,
    /// Restrict results to an explicit thread tree; an empty set matches nothing.
    pub thread_ids: Option<BTreeSet<String>>,
    /// Restrict results to one repository identity.
    pub repository_id: Option<String>,
    /// Restrict results to one model/provider label.
    pub model: Option<String>,
}

impl UsageFilter {
    /// Return whether an observation satisfies this filter.
    pub fn matches(&self, record: &UsageRecord) -> bool {
        self.account_id
            .as_ref()
            .is_none_or(|account| record.account_id.as_ref() == Some(account))
            && self
                .thread_id
                .as_ref()
                .is_none_or(|thread| record.thread_id.as_ref() == Some(thread))
            && self.thread_ids.as_ref().is_none_or(|threads| {
                record
                    .thread_id
                    .as_ref()
                    .is_some_and(|thread| threads.contains(thread))
            })
            && self
                .repository_id
                .as_ref()
                .is_none_or(|repository| record.repository_id.as_ref() == Some(repository))
            && self
                .model
                .as_ref()
                .is_none_or(|model| record.model.as_ref() == Some(model))
    }

    /// Set an account restriction.
    pub fn for_account(mut self, account_id: impl Into<String>) -> Self {
        self.account_id = Some(account_id.into());
        self
    }

    /// Set a thread restriction.
    pub fn for_thread(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = Some(thread_id.into());
        self.thread_ids = None;
        self
    }

    /// Restrict results to the exact known thread set, replacing a single-thread filter.
    pub fn for_threads(mut self, thread_ids: impl IntoIterator<Item = String>) -> Self {
        self.thread_id = None;
        self.thread_ids = Some(thread_ids.into_iter().collect());
        self
    }

    /// Set a repository restriction.
    pub fn for_repository(mut self, repository_id: impl Into<String>) -> Self {
        self.repository_id = Some(repository_id.into());
        self
    }

    /// Set a model restriction.
    pub fn for_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

/// Half-open exact Unix millisecond range used by query primitives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeRange {
    start_ms: i64,
    end_ms: i64,
}

impl TimeRange {
    /// Build a range from legacy seconds, rejecting inverted bounds.
    pub fn new(start: i64, end: i64) -> Result<Self> {
        if end < start {
            return Err(StorageError::InvalidData(format!(
                "time range end {end} precedes start {start}"
            )));
        }
        Self::from_millis(start.saturating_mul(1_000), end.saturating_mul(1_000))
    }

    /// Build an exact millisecond range with inclusive start and exclusive end.
    pub fn from_millis(start_ms: i64, end_ms: i64) -> Result<Self> {
        if end_ms < start_ms {
            return Err(StorageError::InvalidData(
                "time range end precedes start".into(),
            ));
        }
        Ok(Self { start_ms, end_ms })
    }

    /// Inclusive exact UTC millisecond bound.
    pub const fn start_ms(self) -> i64 {
        self.start_ms
    }

    /// Exclusive exact UTC millisecond bound.
    pub const fn end_ms(self) -> i64 {
        self.end_ms
    }

    /// Conservative seconds bounds for pruning hourly files, never row filtering.
    pub fn coarse_seconds(self) -> (i64, i64) {
        let start = if self.start_ms == i64::MIN {
            i64::MIN
        } else {
            self.start_ms.div_euclid(1_000)
        };
        let end = if self.end_ms == i64::MAX {
            i64::MAX
        } else {
            self.end_ms.div_euclid(1_000) + i64::from(self.end_ms.rem_euclid(1_000) != 0)
        };
        (start, end)
    }

    /// Build an unbounded range for callers that need the full local snapshot.
    pub const fn all() -> Self {
        Self {
            start_ms: i64::MIN,
            end_ms: i64::MAX,
        }
    }

    /// Return whether a legacy seconds timestamp falls inside this range.
    pub fn contains(self, timestamp: i64) -> bool {
        self.contains_millis(timestamp.saturating_mul(1_000))
    }

    /// Return whether an exact millisecond timestamp falls inside this range.
    pub fn contains_millis(self, timestamp_ms: i64) -> bool {
        timestamp_ms >= self.start_ms && timestamp_ms < self.end_ms
    }
}

/// Signed aggregate token values returned by query primitives.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageTotals {
    /// Sum of input tokens after corrections.
    pub input_tokens: i64,
    /// Sum of cached input tokens after corrections.
    pub cached_input_tokens: i64,
    /// Sum of output tokens after corrections.
    pub output_tokens: i64,
    /// Sum of reasoning output tokens after corrections.
    pub reasoning_output_tokens: i64,
    /// Number of distinct observations included in the result.
    pub records: u64,
}

impl UsageTotals {
    /// Add another aggregate to this one.
    pub fn add_assign(&mut self, other: &Self) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_output_tokens += other.reasoning_output_tokens;
        self.records += other.records;
    }

    /// Subtract another aggregate while preserving the non-negative record count.
    pub fn subtract_assign(&mut self, other: &Self) {
        self.input_tokens -= other.input_tokens;
        self.cached_input_tokens -= other.cached_input_tokens;
        self.output_tokens -= other.output_tokens;
        self.reasoning_output_tokens -= other.reasoning_output_tokens;
        self.records = self.records.saturating_sub(other.records);
    }

    /// Apply a correction delta without changing the observation count.
    pub fn apply_correction(&mut self, correction: &UsageCorrection) {
        let delta = correction.totals();
        self.input_tokens += delta.input_tokens;
        self.cached_input_tokens += delta.cached_input_tokens;
        self.output_tokens += delta.output_tokens;
        self.reasoning_output_tokens += delta.reasoning_output_tokens;
    }
}

/// Optional producer-defined dimensions retained for future columnar adapters.
///
/// The core record schema stays compact while a caller can carry stable string
/// dimensions in a bounded map before an Arrow/Parquet/DuckDB adapter is added.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageDimensions(pub BTreeMap<String, String>);
