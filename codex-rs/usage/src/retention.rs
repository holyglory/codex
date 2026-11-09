//! Retention horizons for the existing local usage store.

use std::time::Duration;

/// Detailed tool and lifecycle events remain queryable for seven UTC days.
pub const DETAILED_EVENT_RETENTION_DAYS: u32 = 7;
/// Derived usage aggregates remain queryable for thirty UTC days.
pub const AGGREGATE_RETENTION_DAYS: u32 = 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    pub detailed_event_days: u32,
    pub aggregate_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            detailed_event_days: DETAILED_EVENT_RETENTION_DAYS,
            aggregate_days: AGGREGATE_RETENTION_DAYS,
        }
    }
}

impl RetentionPolicy {
    pub const fn detailed_event_duration(self) -> Duration {
        Duration::from_secs(self.detailed_event_days as u64 * 24 * 60 * 60)
    }

    pub const fn aggregate_duration(self) -> Duration {
        Duration::from_secs(self.aggregate_days as u64 * 24 * 60 * 60)
    }

    pub fn validate(self) -> Result<(), &'static str> {
        if self.detailed_event_days == 0 || self.aggregate_days == 0 {
            return Err("usage retention horizons must be positive");
        }
        if self.detailed_event_days > self.aggregate_days {
            return Err("detailed usage retention cannot exceed aggregate retention");
        }
        Ok(())
    }
}
