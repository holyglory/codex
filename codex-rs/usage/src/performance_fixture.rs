//! Each round captures three tool invocations and one model request, each with
//! separate actual start/terminal facts, plus seven disjoint token deltas: 15M
//! journal facts, 4M canonical operations, 1M requests, and 7M token observations.
//! Tokens use the captured request UUID while retaining their provider response
//! identity, matching .6 capture without per-operation assignment metadata.
use serde::Deserialize;
use serde::Serialize;

use super::work_fixture::WorkExpectations;
use crate::HOUR_SECONDS;
use crate::ReviewCounts;
use crate::TimeRange;
use crate::UsageOutcome;
use crate::UsageRecord;
use crate::UsageTotals;

pub(super) const ROUNDS: u64 = 1_000_000;
pub(super) const FACTS: u64 = 15 * ROUNDS;
pub(super) const CURRENT_THREAD: u64 = 137;
const START: i64 = 1_700_000_000;
pub(super) const START_MS: i64 = 1_700_000_000_000;
pub(super) const THREADS: u64 = 16_384;
const REPOSITORIES: u64 = 256;

#[derive(Clone, Copy)]
enum Kind {
    ToolStarted { slot: u64 },
    ToolFinished { slot: u64 },
    RequestStarted,
    RequestFinished,
    Tokens { slot: u64 },
}

const ORDER: [Kind; 15] = [
    Kind::ToolStarted { slot: 0 },
    Kind::RequestStarted,
    Kind::Tokens { slot: 0 },
    Kind::ToolStarted { slot: 1 },
    Kind::Tokens { slot: 1 },
    Kind::ToolFinished { slot: 0 },
    Kind::ToolStarted { slot: 2 },
    Kind::Tokens { slot: 2 },
    Kind::ToolFinished { slot: 1 },
    Kind::Tokens { slot: 3 },
    Kind::Tokens { slot: 4 },
    Kind::ToolFinished { slot: 2 },
    Kind::Tokens { slot: 5 },
    Kind::Tokens { slot: 6 },
    Kind::RequestFinished,
];

pub(super) fn thread_id(index: u64) -> String {
    format!("benchmark-thread-{index:05}")
}
pub(super) fn repository_id(index: u64) -> String {
    format!("benchmark-repository-{index:03}")
}
fn thread(round: u64) -> u64 {
    (round / 8) % THREADS
}
fn repository(round: u64) -> u64 {
    (thread(round) * 37) % REPOSITORIES
}

pub(super) fn review_repository() -> String {
    repository_id((CURRENT_THREAD * 37) % REPOSITORIES)
}

pub(super) fn review_range() -> TimeRange {
    TimeRange::new(
        START + 36 * HOUR_SECONDS,
        START + 36 * HOUR_SECONDS + 15 * 60,
    )
    .expect("finite review range")
}

// Match the scattered UUID v4 identities used by runtime capture. The odd
// multiplications and xor-shifts form a bijection on u64; its entire output is
// retained in the first and last eight hex digits, outside the UUID marker
// bits. Distinct ordinals therefore cannot collide. Each baseline round owns
// ordinals 16*round through 16*round+15; recovery starts after that namespace.
fn scattered_id(ordinal: u64) -> String {
    let mut value = ordinal.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^= value >> 31;
    let high = (value >> 32) as u32;
    let low = value as u32;
    let middle = value.rotate_left(29).wrapping_mul(0x9e3779b97f4a7c15);
    let second = middle & 0xffff;
    let third = ((middle >> 16) & 0x0fff) | 0x4000;
    let fourth = ((middle >> 32) & 0x3fff) | 0x8000;
    let fifth = (middle >> 48) & 0xffff;
    format!("{high:08x}-{second:04x}-{third:04x}-{fourth:04x}-{fifth:04x}{low:08x}")
}

pub(super) fn round_records(round: u64) -> [UsageRecord; 15] {
    std::array::from_fn(|position| {
        let kind = ORDER[position];
        let base_ms = START_MS + (round * 250) as i64;
        let observed_at_ms = base_ms + (position * 10) as i64;
        let response = scattered_id(round * 16 + 11);
        let (id, operation, event, input, output) = match kind {
            Kind::ToolStarted { slot } => {
                let operation = scattered_id(round * 16 + slot);
                (
                    format!("{operation}:started"),
                    operation,
                    "tool_started",
                    0,
                    0,
                )
            }
            Kind::ToolFinished { slot } => {
                let operation = scattered_id(round * 16 + slot);
                (
                    format!("{operation}:finished"),
                    operation,
                    "tool_finished",
                    0,
                    0,
                )
            }
            Kind::RequestStarted => {
                let operation = scattered_id(round * 16 + 3);
                (
                    format!("{operation}:started"),
                    operation,
                    "model_request_started",
                    0,
                    0,
                )
            }
            Kind::RequestFinished => {
                let operation = scattered_id(round * 16 + 3);
                (
                    format!("{operation}:finished"),
                    operation,
                    "model_request_finished",
                    0,
                    0,
                )
            }
            Kind::Tokens { slot } => {
                let observation = round * 7 + slot;
                (
                    scattered_id(round * 16 + 4 + slot),
                    scattered_id(round * 16 + 3),
                    "token_observation",
                    32 + observation % 17,
                    7 + observation % 11,
                )
            }
        };
        let mut record = UsageRecord::new_millis(id, observed_at_ms, input, output)
            .with_operation(operation)
            .with_account(format!("benchmark-account-{:02}", (round / 1_024) % 16))
            .with_thread(thread_id(thread(round)))
            .with_repository(repository_id(repository(round)));
        let (activity, phase) = [
            ("coding", "implementation"),
            ("repository_analysis", "planning"),
            ("unit_testing", "testing"),
            ("documentation_authoring", "implementation"),
        ][(round % 4) as usize];
        record = record.with_activity(activity, phase);
        record.event = Some(event.into());
        record.coverage = Some(
            if matches!(kind, Kind::ToolStarted { .. } | Kind::RequestStarted) {
                "capture_started"
            } else {
                "recorded"
            }
            .into(),
        );
        record.classification = Some("agent_declared".into());
        match kind {
            Kind::ToolStarted { slot } | Kind::ToolFinished { slot } => {
                record =
                    record.with_tool(["exec_command", "apply_patch", "web.run"][slot as usize]);
                record.started_at_ms = Some(base_ms + (slot * 30) as i64);
                if matches!(kind, Kind::ToolFinished { .. }) {
                    record.ended_at_ms = Some(observed_at_ms);
                    record.duration_ms = Some(50);
                    record.outcome = Some(UsageOutcome::Completed);
                }
            }
            Kind::RequestStarted | Kind::RequestFinished => {
                record = record.with_model(format!("benchmark-model-{}", round % 3));
                record.started_at_ms = Some(base_ms + 10);
                record.attempt_number = Some(1);
                if matches!(kind, Kind::RequestFinished) {
                    record.ended_at_ms = Some(observed_at_ms);
                    record.duration_ms = Some(130);
                    record.outcome = Some(UsageOutcome::Completed);
                    record.response_id = Some(response);
                }
            }
            Kind::Tokens { slot } => {
                let observation = round * 7 + slot;
                record = record.with_model(format!("benchmark-model-{}", round % 3));
                record.cached_input_tokens = 8 + observation % 5;
                record.reasoning_output_tokens = 2 + observation % 3;
                record.response_id = Some(response);
                record.provider_total_tokens = Some(input + output);
                record.cache_write_input_tokens = Some(0);
            }
        }
        record
    })
}

/// Extra facts carry zero token values and distinct dimensions. They exercise
/// real concurrent durable capture without changing the planned token sums.
pub(super) fn live_record(index: u64) -> UsageRecord {
    let timestamp_ms = START_MS + ((ROUNDS + index) * 250) as i64 + 10_000;
    let operation = scattered_id(ROUNDS * 16 + index);
    let mut record = UsageRecord::new_millis(
        format!("{operation}:finished"),
        timestamp_ms,
        /*input_tokens*/ 0,
        /*output_tokens*/ 0,
    )
    .with_operation(operation)
    .with_account("benchmark-live-account")
    .with_thread("benchmark-live-thread")
    .with_repository("benchmark-live-repository")
    .with_tool("exec_command");
    record.event = Some("tool_finished".into());
    record.started_at_ms = Some(timestamp_ms - 20);
    record.ended_at_ms = Some(timestamp_ms);
    record.duration_ms = Some(20);
    record.outcome = Some(UsageOutcome::Completed);
    record.coverage = Some("recorded".into());
    record.phase = Some("implementation".into());
    record.activity = Some("coding".into());
    record.classification = Some("agent_declared".into());
    record
}

#[derive(Deserialize, Serialize)]
pub(super) struct Expectations {
    /// Older retained fixtures remain readable but cannot prove sparse work.
    #[serde(default)]
    pub work: Option<WorkExpectations>,
    pub global: UsageTotals,
    pub current_thread: UsageTotals,
    pub finite_repository: UsageTotals,
    pub global_counts: ReviewCounts,
    pub current_thread_counts: ReviewCounts,
    pub finite_repository_counts: ReviewCounts,
    pub finite_token_observations: u64,
    pub first_operation_ids: Vec<String>,
}

impl Expectations {
    pub fn calculate() -> Self {
        let mut expected = Self {
            work: Some(WorkExpectations::calculate()),
            global: token_totals(/*first*/ 0, /*end*/ ROUNDS * 7),
            current_thread: UsageTotals::default(),
            finite_repository: UsageTotals::default(),
            global_counts: ReviewCounts {
                operations: ROUNDS * 4,
                tools: ROUNDS * 3,
                model_requests: ROUNDS,
                tool_completed: ROUNDS * 3,
                ..ReviewCounts::default()
            },
            current_thread_counts: ReviewCounts::default(),
            finite_repository_counts: ReviewCounts::default(),
            finite_token_observations: 0,
            first_operation_ids: Vec::new(),
        };
        expected.global.records = FACTS;
        for round in 0..ROUNDS {
            let mut totals = token_totals(round * 7, (round + 1) * 7);
            totals.records = 15;
            let counts = ReviewCounts {
                operations: 4,
                tools: 3,
                model_requests: 1,
                tool_completed: 3,
                ..ReviewCounts::default()
            };
            if thread(round) == CURRENT_THREAD {
                expected.current_thread.add_assign(&totals);
                expected.current_thread_counts = expected
                    .current_thread_counts
                    .checked_add(&counts)
                    .expect("current thread count oracle");
            }
            if repository(round) == (CURRENT_THREAD * 37) % REPOSITORIES
                && review_range().contains(START + (round / 4) as i64)
            {
                expected.finite_repository.add_assign(&totals);
                expected.finite_repository_counts = expected
                    .finite_repository_counts
                    .checked_add(&counts)
                    .expect("finite repository count oracle");
                expected.finite_token_observations += 7;
                if expected.first_operation_ids.len() < 50 {
                    expected.first_operation_ids.extend([
                        scattered_id(round * 16),
                        scattered_id(round * 16 + 3),
                        scattered_id(round * 16 + 1),
                        scattered_id(round * 16 + 2),
                    ]);
                }
            }
        }
        expected.first_operation_ids.truncate(50);
        expected
    }
}

// Independent arithmetic for the fixture's disjoint deltas, rather than summing
// generated observations back into their own expected result.
fn remainder_sum(count: u64, modulus: u64) -> i64 {
    let remainder = count % modulus;
    ((count / modulus) * modulus * (modulus - 1) / 2 + remainder * remainder.saturating_sub(1) / 2)
        as i64
}

pub(super) fn token_totals(first: u64, end: u64) -> UsageTotals {
    let count = (end - first) as i64;
    let sum = |modulus| remainder_sum(end, modulus) - remainder_sum(first, modulus);
    UsageTotals {
        input_tokens: count * 32 + sum(17),
        cached_input_tokens: count * 8 + sum(5),
        output_tokens: count * 7 + sum(11),
        reasoning_output_tokens: count * 2 + sum(3),
        records: 0,
    }
}
