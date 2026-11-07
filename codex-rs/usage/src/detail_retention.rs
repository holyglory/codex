//! Advance admission separately from bounded, resumable exact-ID retirement.
use std::fs::TryLockError;
use std::path::Path;

use crate::JournalCheckpoint;
use crate::ManifestStore;
use crate::Result;
use crate::RetentionPolicy;
use crate::UsageJournal;
use crate::id_index::IdRunDescriptor;
use crate::id_index::load_id_index;
use crate::id_index::write_descriptor;
use crate::id_index_compaction::IdCompactionJob;
use crate::id_index_compaction::IdCompactionPlan;
use crate::id_index_compaction::IdCompactionProgress;
use crate::storage_file;

/// Owned by the existing retention worker; pins survive bounded step yields.
#[derive(Default)]
pub(crate) struct Maintenance {
    id_job: Option<IdRetentionJob>,
}

struct IdRetentionJob {
    input: IdRunDescriptor,
    cutoff_at_ms: i64,
    worker: IdCompactionJob,
    output: Option<IdRunDescriptor>,
}

impl Maintenance {
    /// A short writer barrier fixes the rules for future capture. Durable
    /// unpublished entries retain the admission rules under which they arrived.
    pub(crate) fn advance_admission(
        &mut self,
        root: &Path,
        policy: RetentionPolicy,
        now_ms: i64,
    ) -> Result<bool> {
        storage_file::ensure_usage_directory(root)?;
        let lock = storage_file::open_read_write(&root.join(".snapshot.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let writer = storage_file::open_read_write(&root.join("events.lock"))?;
        match writer.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let store = ManifestStore::new(root.join("manifest.json"));
        let mut manifest = store.load()?;
        if UsageJournal::new(root.join("events.jsonl"))
            .checkpoint()?
            .sequence
            != manifest.watermark
        {
            return Ok(false);
        }
        let cutoff = manifest
            .raw_admission_cutoff_at_ms
            .map_or(policy.raw_cutoff(now_ms), |old| {
                old.max(policy.raw_cutoff(now_ms))
            });
        let before = manifest.review_segment_contributions.len();
        manifest
            .review_segment_contributions
            .retain(|_, files| !files.is_empty());
        if manifest.raw_admission_cutoff_at_ms == Some(cutoff)
            && before == manifest.review_segment_contributions.len()
        {
            return Ok(false);
        }
        manifest.raw_admission_cutoff_at_ms = Some(cutoff);
        manifest.mark_snapshot(manifest.watermark, manifest.freshness);
        store.save(&manifest)?;
        Ok(true)
    }

    /// Consume at most `max_entries` IDs. New publication does not discard this
    /// work: installation replaces only its still-identical selected input.
    pub(crate) fn step_index(&mut self, root: &Path, max_entries: usize) -> Result<bool> {
        let _pin = crate::reclamation::SnapshotPin::acquire(root)?;
        let store = ManifestStore::new(root.join("manifest.json"));
        let source = store.load()?;
        let Some(cutoff) = source.raw_admission_cutoff_at_ms else {
            return Ok(false);
        };
        let descriptor = load_id_index(
            root,
            source.id_index_file.as_deref(),
            JournalCheckpoint {
                sequence: source.watermark,
                byte_offset: source.journal_byte_offset,
            },
        )?;
        if self
            .id_job
            .as_ref()
            .is_some_and(|job| !descriptor.runs.contains(&job.input))
        {
            self.id_job = None;
        }
        if self.id_job.is_none() {
            let whole_hour = cutoff.div_euclid(3_600_000).saturating_mul(3_600_000);
            let Some(input) = descriptor
                .runs
                .iter()
                .find(|run| run.retained_from_at_ms.is_none_or(|old| old < whole_hour))
            else {
                return Ok(false);
            };
            self.id_job = Some(IdRetentionJob {
                input: input.clone(),
                cutoff_at_ms: cutoff,
                worker: IdCompactionJob::start(
                    root,
                    IdCompactionPlan {
                        source_watermark: descriptor.watermark,
                        inputs: vec![input.clone()],
                        output_level: input.level,
                    },
                    &stamp(),
                )?,
                output: None,
            });
        }
        let Some(job) = self.id_job.as_mut() else {
            return Ok(false);
        };
        if job.output.is_none() {
            let cutoff = job.cutoff_at_ms;
            match job.worker.step_retaining(max_entries, |entry| {
                // Younger inactive-chat IDs stay exact through the raw horizon.
                segment_hour(&entry.segment_file, "segment-").is_none_or(|hour| {
                    hour.saturating_mul(1_000).saturating_add(3_600_000) > cutoff
                })
            })? {
                IdCompactionProgress::More { .. } => return Ok(true),
                IdCompactionProgress::Finished(mut output) => {
                    output.retained_from_at_ms =
                        Some(cutoff.div_euclid(3_600_000).saturating_mul(3_600_000));
                    job.output = Some(output);
                }
            }
        }
        let lock = storage_file::open_read_write(&root.join(".snapshot.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let mut current = store.load()?;
        if current
            .raw_admission_cutoff_at_ms
            .is_none_or(|floor| floor < job.cutoff_at_ms)
        {
            self.id_job = None;
            return Ok(false);
        }
        let mut descriptor = load_id_index(
            root,
            current.id_index_file.as_deref(),
            JournalCheckpoint {
                sequence: current.watermark,
                byte_offset: current.journal_byte_offset,
            },
        )?;
        let Some(index) = descriptor.runs.iter().position(|run| run == &job.input) else {
            self.id_job = None;
            return Ok(true);
        };
        let Some(output) = job.output.as_ref() else {
            return Err(crate::StorageError::InvalidData(
                "finished ID retention has no output".into(),
            ));
        };
        if output.entry_count == 0 {
            descriptor.runs.remove(index);
        } else {
            descriptor.runs[index] = output.clone();
        }
        descriptor.retained_from_at_ms = Some(
            descriptor
                .retained_from_at_ms
                .map_or(job.cutoff_at_ms, |old| old.max(job.cutoff_at_ms)),
        );
        let file = format!("record-id-index-{}.json", stamp());
        write_descriptor(root, &file, &descriptor)?;
        current.id_index_file = Some(file);
        current.mark_snapshot(current.watermark, current.freshness);
        store.save(&current)?;
        self.id_job = None;
        Ok(true)
    }
}

pub(crate) fn segment_hour(file: &str, prefix: &str) -> Option<i64> {
    file.strip_prefix(prefix)?
        .strip_suffix(".parquet")?
        .split_once('-')?
        .1
        .parse()
        .ok()
}

pub(crate) fn stamp() -> String {
    format!(
        "retained_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}
