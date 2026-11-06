//! Bounded immutable rollup merging with pinned inputs and atomic installation.

use std::fs::TryLockError;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use duckdb::Connection;

use crate::ManifestStore;
use crate::Result;
use crate::RetentionPolicy;
use crate::StorageError;
use crate::reclamation::SnapshotPin;
use crate::review_storage::parquet_source;
use crate::review_storage::query_error;
use crate::review_storage::quoted;
use crate::storage_file;
use crate::token_rollup::DIMENSIONS;
use crate::token_rollup::write_query;

const MAX_INPUTS: usize = 8;

#[derive(Clone, Copy, Debug)]
pub(crate) enum MergeMode {
    /// Amortize work while the bounded raw-retention scan reports progress.
    FullBatch,
    /// Drain a smaller remainder and convert newly inactive hourly buckets.
    Available,
}

/// The pin protects both selected inputs and the unpublished output from GC.
#[derive(Debug)]
struct RollupCompactionJob {
    _pin: SnapshotPin,
    root: PathBuf,
    source_generation: u64,
    inputs: Vec<String>,
    output: String,
}

impl RollupCompactionJob {
    fn prepare(
        root: &Path,
        policy: RetentionPolicy,
        now_ms: i64,
        mode: MergeMode,
    ) -> Result<Option<Self>> {
        policy.validate()?;
        let pin = SnapshotPin::acquire(root)?;
        let manifest = ManifestStore::new(root.join("manifest.json")).load()?;
        let minimum = match mode {
            MergeMode::FullBatch => MAX_INPUTS,
            MergeMode::Available => 1,
        };
        if manifest.snapshot_generation == 0 || manifest.token_rollup_files.len() < minimum {
            return Ok(None);
        }
        let inputs = manifest
            .token_rollup_files
            .iter()
            .take(MAX_INPUTS)
            .cloned()
            .collect::<Vec<_>>();
        let inactive = manifest
            .thread_last_activity_at_ms
            .iter()
            .filter(|(_, at)| **at < policy.inactive_cutoff(now_ms))
            .map(|(thread, _)| quoted(thread))
            .collect::<Vec<_>>();
        let inactive = if inactive.is_empty() {
            "false".to_string()
        } else {
            format!("COALESCE(thread_id IN ({}), false)", inactive.join(", "))
        };
        let connection = Connection::open_in_memory().map_err(query_error)?;
        let source = parquet_source(root, &inputs)?;
        if inputs.len() == 1 {
            let needs_conversion: bool = connection
                .query_row(
                    &format!(
                        "SELECT EXISTS (SELECT 1 FROM {source} WHERE hour_at_ms IS NOT NULL AND {inactive})"
                    ),
                    [],
                    |row| row.get(0),
                )
                .map_err(query_error)?;
            if !needs_conversion {
                return Ok(None);
            }
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let output = format!("rollup-merged-{}-{stamp}.parquet", std::process::id());
        let dimensions = DIMENSIONS.join(", ");
        // Preserve lifetime NULL buckets when a chat resumes; discarded hours
        // cannot be reconstructed. Sum observations, not aggregate row counts.
        let query = format!(
            "SELECT CASE WHEN {inactive} THEN NULL ELSE hour_at_ms END AS hour_at_ms, \
             {dimensions}, min(first_at_ms) AS first_at_ms, max(last_at_ms) AS last_at_ms, \
             sum(records::HUGEINT)::UBIGINT AS records, \
             sum(input_tokens::HUGEINT)::BIGINT AS input_tokens, \
             sum(cached_input_tokens::HUGEINT)::BIGINT AS cached_input_tokens, \
             sum(output_tokens::HUGEINT)::BIGINT AS output_tokens, \
             sum(reasoning_output_tokens::HUGEINT)::BIGINT AS reasoning_output_tokens, \
             sum(token_observations::HUGEINT)::UBIGINT AS token_observations, \
             sum(provider_total_tokens::HUGEINT)::BIGINT AS provider_total_tokens, \
             sum(provider_total_unknowns::HUGEINT)::UBIGINT AS provider_total_unknowns, \
             sum(cache_write_input_tokens::HUGEINT)::BIGINT AS cache_write_input_tokens, \
             sum(cache_write_unknowns::HUGEINT)::UBIGINT AS cache_write_unknowns, \
             sum(missing_observations::HUGEINT)::UBIGINT AS missing_observations \
             FROM {source} GROUP BY ALL"
        );
        write_query(&connection, root, &output, &query)?;
        Ok(Some(Self {
            _pin: pin,
            root: root.to_path_buf(),
            source_generation: manifest.snapshot_generation,
            inputs,
            output,
        }))
    }

    fn install(self) -> Result<bool> {
        let lock = storage_file::open_read_write(&self.root.join(".snapshot.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let store = ManifestStore::new(self.root.join("manifest.json"));
        let mut current = store.load()?;
        if current.snapshot_generation != self.source_generation
            || current.token_rollup_files.get(..self.inputs.len()) != Some(self.inputs.as_slice())
        {
            // A publisher or another maintenance pass won. Its data and counters
            // remain authoritative; this output becomes a reclaimable orphan.
            return Ok(false);
        }
        current
            .snapshot_generation
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("snapshot generation exhausted".into()))?;
        current
            .token_rollup_files
            .drain(..self.inputs.len())
            .for_each(drop);
        current.token_rollup_files.push(self.output.clone());
        current.mark_snapshot(current.watermark, current.freshness);
        #[cfg(unix)]
        std::fs::File::open(self.root.join("segments"))?.sync_all()?;
        store.save(&current)?;
        Ok(true)
    }
}

pub(crate) fn compact_one(
    root: &Path,
    policy: RetentionPolicy,
    now_ms: i64,
    mode: MergeMode,
) -> Result<bool> {
    match RollupCompactionJob::prepare(root, policy, now_ms, mode)? {
        Some(job) => job.install(),
        None => Ok(false),
    }
}

#[cfg(test)]
#[path = "rollup_compaction_tests.rs"]
mod tests;
