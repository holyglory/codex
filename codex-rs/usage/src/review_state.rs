//! Factual current operation state. Classification changes remain separate facts.
use serde::Deserialize;
use serde::Serialize;

use crate::Result;
use crate::StorageError;
use crate::UsageOutcome;
use crate::UsageRecord;

/// Maximum operations in one independently replaced state partition.
pub const REVIEW_PARTITION_SIZE: u64 = 16_384;

/// Runtime lifecycle families that represent measured work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOperationKind {
    /// A tool invocation.
    Tool,
    /// A request sent to the model provider.
    ModelRequest,
    /// A separately observed model connection warmup.
    ModelWarmup,
}

/// Latest factual state of one operation, independent of observation-hour files.
/// Nullable timing is unknown, and duration is supplied only by a terminal fact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReviewOperation {
    pub operation_id: String,
    pub kind: ReviewOperationKind,
    pub first_sequence: u64,
    pub source_sequence: u64,
    pub observed_at_ms: i64,
    pub start_observed: bool,
    pub terminal_observed: bool,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub duration_ms: Option<u64>,
    pub outcome: Option<UsageOutcome>,
    pub account_id: Option<String>,
    pub thread_id: Option<String>,
    pub repository_id: Option<String>,
    pub turn_id: Option<String>,
    pub model: Option<String>,
    pub tool: Option<String>,
    pub response_id: Option<String>,
    pub phase: Option<String>,
    pub activity: Option<String>,
    pub classification: Option<String>,
    pub coverage: Option<String>,
    pub attempt_number: Option<u64>,
    pub rework_of_operation_id: Option<String>,
}

impl ReviewOperation {
    pub(crate) fn from_fact(sequence: u64, record: &UsageRecord) -> Result<Self> {
        let (kind, _) = lifecycle(record).ok_or_else(|| invalid("not an operation lifecycle"))?;
        let operation_id = record
            .operation_id
            .clone()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| invalid("operation lifecycle has no operation id"))?;
        let mut state = Self {
            operation_id,
            kind,
            first_sequence: sequence,
            source_sequence: sequence,
            observed_at_ms: record.effective_observed_at_ms(),
            start_observed: false,
            terminal_observed: false,
            started_at_ms: None,
            ended_at_ms: None,
            duration_ms: None,
            outcome: None,
            account_id: None,
            thread_id: None,
            repository_id: None,
            turn_id: None,
            model: None,
            tool: None,
            response_id: None,
            phase: None,
            activity: None,
            classification: None,
            coverage: None,
            attempt_number: None,
            rework_of_operation_id: None,
        };
        state.merge(sequence, record)?;
        Ok(state)
    }

    pub(crate) fn merge(&mut self, sequence: u64, record: &UsageRecord) -> Result<()> {
        let (kind, terminal) =
            lifecycle(record).ok_or_else(|| invalid("not an operation lifecycle"))?;
        if record.operation_id.as_deref() != Some(self.operation_id.as_str()) || kind != self.kind {
            return Err(invalid("conflicting explicit operation identity or kind"));
        }
        if sequence < self.first_sequence {
            return Err(invalid("operation update precedes its first accepted fact"));
        }
        for (field, incoming) in [
            (&mut self.account_id, &record.account_id),
            (&mut self.thread_id, &record.thread_id),
            (&mut self.repository_id, &record.repository_id),
            (&mut self.turn_id, &record.turn_id),
            (&mut self.model, &record.model),
            (&mut self.tool, &record.tool),
            (&mut self.response_id, &record.response_id),
            (&mut self.phase, &record.phase),
            (&mut self.activity, &record.activity),
            (&mut self.classification, &record.classification),
            (
                &mut self.rework_of_operation_id,
                &record.rework_of_operation_id,
            ),
        ] {
            fill(field, incoming.clone())?;
        }
        fill(&mut self.attempt_number, record.attempt_number)?;
        fill(&mut self.started_at_ms, record.effective_started_at_ms())?;
        if terminal {
            fill(&mut self.ended_at_ms, record.effective_ended_at_ms())?;
            fill(&mut self.duration_ms, record.duration_ms)?;
            fill(&mut self.outcome, record.outcome)?;
            if self.terminal_observed {
                fill(&mut self.coverage, record.coverage.clone())?;
            } else {
                self.coverage = record.coverage.clone();
            }
            self.terminal_observed = true;
        } else {
            if record.effective_ended_at_ms().is_some()
                || record.duration_ms.is_some()
                || record.outcome.is_some()
            {
                return Err(invalid("start fact carries terminal operation fields"));
            }
            if !self.terminal_observed {
                fill(&mut self.coverage, record.coverage.clone())?;
            }
            self.start_observed = true;
        }
        if self
            .started_at_ms
            .zip(self.ended_at_ms)
            .is_some_and(|(start, end)| end < start)
        {
            return Err(invalid("operation completion precedes its explicit start"));
        }
        self.source_sequence = self.source_sequence.max(sequence);
        self.observed_at_ms = self.observed_at_ms.max(record.effective_observed_at_ms());
        Ok(())
    }
}

/// One canonical observation's token and coverage contribution. A response id
/// links provider observations to a lifecycle whose operation id is different.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReviewContribution {
    pub record_id: String,
    pub source_sequence: u64,
    pub observed_at_ms: i64,
    pub operation_id: Option<String>,
    pub response_id: Option<String>,
    pub account_id: Option<String>,
    pub thread_id: Option<String>,
    pub repository_id: Option<String>,
    pub model: Option<String>,
    pub tool: Option<String>,
    pub event: Option<String>,
    pub coverage: Option<String>,
    pub phase: Option<String>,
    pub activity: Option<String>,
    pub classification: Option<String>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub provider_total_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub missing_observations: Option<u64>,
    /// Number of original observations represented by this row. Current
    /// publication fragments use one; retained review rollups carry a larger
    /// weighted value so report queries never mistake an aggregate row for a
    /// single observation.
    #[serde(default)]
    pub observation_count: Option<u64>,
    /// Number of represented observations without a provider total. This is
    /// populated only by retained rollups; current facts derive it from the
    /// nullable provider value.
    #[serde(default)]
    pub provider_total_unknowns: Option<u64>,
    /// Number of represented observations without a cache-write total.
    #[serde(default)]
    pub cache_write_unknowns: Option<u64>,
    /// Sum of represented missing-observation markers.
    #[serde(default)]
    pub missing_observations_sum: Option<u64>,
    pub attempt_number: Option<u64>,
    pub rework_of_operation_id: Option<String>,
}

impl ReviewContribution {
    pub(crate) fn from_fact(sequence: u64, fact: &UsageRecord) -> Self {
        Self {
            record_id: fact.id.clone(),
            source_sequence: sequence,
            observed_at_ms: fact.effective_observed_at_ms(),
            operation_id: fact.operation_id.clone(),
            response_id: fact.response_id.clone(),
            account_id: fact.account_id.clone(),
            thread_id: fact.thread_id.clone(),
            repository_id: fact.repository_id.clone(),
            model: fact.model.clone(),
            tool: fact.tool.clone(),
            event: fact.event.clone(),
            coverage: fact.coverage.clone(),
            phase: fact.phase.clone(),
            activity: fact.activity.clone(),
            classification: fact.classification.clone(),
            input_tokens: fact.input_tokens,
            cached_input_tokens: fact.cached_input_tokens,
            output_tokens: fact.output_tokens,
            reasoning_output_tokens: fact.reasoning_output_tokens,
            provider_total_tokens: fact.provider_total_tokens,
            cache_write_input_tokens: fact.cache_write_input_tokens,
            missing_observations: fact.missing_observations,
            observation_count: Some(1),
            provider_total_unknowns: None,
            cache_write_unknowns: None,
            missing_observations_sum: None,
            attempt_number: fact.attempt_number,
            rework_of_operation_id: fact.rework_of_operation_id.clone(),
        }
    }
}

/// A late append-only taxonomy change, selected by operation identity rather
/// than its observation time. Unknown target ids stay available for later facts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReviewClassification {
    pub record_id: String,
    pub operation_id: String,
    pub source_sequence: u64,
    pub observed_at_ms: i64,
    pub phase: Option<String>,
    pub activity: Option<String>,
    pub classification: Option<String>,
}

pub(crate) fn lifecycle(record: &UsageRecord) -> Option<(ReviewOperationKind, bool)> {
    match record.event.as_deref()? {
        "tool_started" => Some((ReviewOperationKind::Tool, false)),
        "tool_finished" => Some((ReviewOperationKind::Tool, true)),
        "model_request_started" => Some((ReviewOperationKind::ModelRequest, false)),
        "model_request_finished" => Some((ReviewOperationKind::ModelRequest, true)),
        "model_warmup_started" => Some((ReviewOperationKind::ModelWarmup, false)),
        "model_warmup_finished" => Some((ReviewOperationKind::ModelWarmup, true)),
        _ => None,
    }
}

fn fill<T: Eq>(field: &mut Option<T>, incoming: Option<T>) -> Result<()> {
    if let Some(incoming) = incoming {
        if field.as_ref().is_some_and(|previous| *previous != incoming) {
            return Err(invalid("conflicting explicit operation field"));
        }
        *field = Some(incoming);
    }
    Ok(())
}

pub(crate) fn invalid(message: &str) -> StorageError {
    StorageError::InvalidData(message.into())
}
