//! Background retention policy. Raw observations and inactive chats have
//! independent horizons; compacted aggregates retain their original totals.
use std::path::Path;

use crate::Result;
use crate::StorageError;

/// Local detail retention, in whole UTC days. Changes apply to future
/// maintenance; increasing a horizon cannot recreate discarded observations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    pub raw_retention_days: u32,
    pub inactive_chat_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            raw_retention_days: 7,
            inactive_chat_days: 7,
        }
    }
}

impl RetentionPolicy {
    pub(crate) fn validate(self) -> Result<()> {
        if [self.raw_retention_days, self.inactive_chat_days]
            .into_iter()
            .any(|days| !(1..=36_500).contains(&days))
        {
            return Err(StorageError::InvalidData(
                "usage retention days must be between 1 and 36500".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn raw_cutoff(self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(i64::from(self.raw_retention_days) * 86_400_000)
    }

    pub(crate) fn inactive_cutoff(self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(i64::from(self.inactive_chat_days) * 86_400_000)
    }
}

/// The existing retention worker keeps bounded rewrites across maintenance wakes.
#[derive(Default)]
pub(crate) struct Maintenance {
    detail: crate::detail_retention::Maintenance,
    corrections: crate::correction_retention::Maintenance,
    operations: crate::operation_id_maintenance::Maintenance,
}

pub(crate) fn maintain(
    root: &Path,
    policy: RetentionPolicy,
    now_ms: i64,
    maintenance: &mut Maintenance,
) -> Result<()> {
    policy.validate()?;
    let started = std::time::Instant::now();
    maintenance.detail.advance_admission(root, policy, now_ms)?;
    for _ in 0..64 {
        let raw_progress = crate::token_rollup::compact_one(root, policy, now_ms)?;
        let mode = if raw_progress {
            crate::rollup_compaction::MergeMode::FullBatch
        } else {
            crate::rollup_compaction::MergeMode::Available
        };
        let merged = crate::rollup_compaction::compact_one(root, policy, now_ms, mode)?;
        if (!raw_progress && !merged) || started.elapsed() >= std::time::Duration::from_secs(10) {
            break;
        }
    }
    // Scan progress can continue without retiring a row. Still service smaller
    // rollup sets and newly inactive chats once per maintenance invocation.
    crate::rollup_compaction::compact_one(
        root,
        policy,
        now_ms,
        crate::rollup_compaction::MergeMode::Available,
    )?;
    let mut scan_corrections = true;
    for _ in 0..64 {
        let id_progress = maintenance
            .detail
            .step_index(root, /*max_entries*/ 16_384)?;
        let operation_progress = maintenance.operations.step(root, /*max_entries*/ 16_384)?;
        let corrected = if scan_corrections {
            maintenance.corrections.step(root)?
        } else {
            false
        };
        // Cursor-only scans and protected sidecars never keep the work loop
        // alive, including while an independent ID rewrite yields more steps.
        scan_corrections = corrected;
        if (!id_progress && !operation_progress && !corrected)
            || started.elapsed() >= std::time::Duration::from_secs(10)
        {
            break;
        }
    }
    crate::reclamation::reclaim(root)?;
    Ok(())
}
