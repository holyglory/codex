use crate::UsageStoreError;
use crate::outcome_types::OutcomeEffort;
use crate::outcome_types::OutcomeMeasurement;
use crate::report_math::interval_union_ns;
use crate::report_math::subtract_intervals;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Effort {
    pub(super) operations: u64,
    pub(super) retries: u64,
    pub(super) rework: u64,
    pub(super) tokens: u64,
    pub(super) unknown_tokens: u64,
    active: BTreeMap<String, Vec<(i64, i64)>>,
    elapsed: Vec<(i64, i64)>,
    waits: Vec<(i64, i64)>,
    unknown_active: u64,
    unknown_elapsed: u64,
    unknown_waits: u64,
}

impl Effort {
    pub(super) fn add_time(
        &mut self,
        operation: &super::outcomes::Operation,
        waits: &[(i64, i64)],
        unknown_waits: u64,
    ) -> Result<(), UsageStoreError> {
        self.operations += 1;
        self.retries += u64::from(operation.retry);
        self.rework += u64::from(operation.rework);
        let is_wait = matches!(
            operation.state.as_str(),
            "user_wait" | "external_wait" | "blocked_wait"
        );
        if !operation.overlaps_window {
            return Ok(());
        }
        self.waits.extend_from_slice(waits);
        self.unknown_waits += unknown_waits;
        let Some(interval) = operation.interval else {
            self.unknown_elapsed += 1;
            if is_wait {
                self.unknown_waits += 1;
            } else {
                self.unknown_active += 1;
            }
            return Ok(());
        };
        self.elapsed.push(interval);
        if is_wait {
            self.waits.push(interval);
        } else if let Some(agent) = &operation.agent_id {
            if unknown_waits > 0 {
                self.unknown_active += 1;
            } else {
                self.active
                    .entry(agent.clone())
                    .or_default()
                    .extend(subtract_intervals(interval, waits)?);
            }
        } else {
            self.unknown_active += 1;
        }
        Ok(())
    }

    pub(super) fn finish(self) -> Result<OutcomeEffort, UsageStoreError> {
        let active = self.active.values().try_fold(0_u64, |sum, intervals| {
            sum.checked_add(interval_union_ns(intervals)? / 1_000_000)
                .ok_or(UsageStoreError::AggregateOverflow)
        })?;
        Ok(OutcomeEffort {
            operations: self.operations,
            retry_operations: self.retries,
            rework_operations: self.rework,
            provider_total_tokens: measurement(self.tokens, self.unknown_tokens),
            active_agent_ms: measurement(active, self.unknown_active),
            elapsed_execution_ms: measurement(
                interval_union_ns(&self.elapsed)? / 1_000_000,
                self.unknown_elapsed,
            ),
            recorded_wait_ms: measurement(
                interval_union_ns(&self.waits)? / 1_000_000,
                self.unknown_waits,
            ),
        })
    }
}

fn measurement(measured: u64, unknown: u64) -> OutcomeMeasurement {
    OutcomeMeasurement {
        measured,
        exact: (unknown == 0).then_some(measured),
        unknown,
    }
}
