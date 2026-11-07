//! Incremental retirement of immutable review detail, prepared outside the
//! publication lock. Weighted summaries keep observation totals after detail
//! expires; operation counts already live in independent manifest summaries.
use std::collections::BTreeSet;
use std::fs::TryLockError;
use std::path::Path;
use std::path::PathBuf;

use duckdb::Connection;

use crate::ManifestStore;
use crate::Result;
use crate::StorageError;
use crate::UsageJournal;
use crate::UsageManifest;
use crate::reclamation::SnapshotPin;
use crate::review_index::IndexedInterval;
use crate::review_index::write_membership;
use crate::review_operation_storage::write_operation_rows;
use crate::review_projection::load_review_projection;
use crate::review_projection::operation_bloom;
use crate::review_state::REVIEW_PARTITION_SIZE;
use crate::review_state::ReviewOperation;
use crate::review_storage::parquet_source;
use crate::review_storage::query_error;
use crate::review_storage::query_rows;
use crate::review_storage::quoted;
use crate::review_storage::write_json;
use crate::review_storage::write_query;
use crate::review_storage::write_rows;
use crate::storage_file;

const MAX_ROLLUP_INPUTS: usize = 8;

struct ReviewRetentionJob {
    _pin: SnapshotPin,
    root: PathBuf,
    source: UsageManifest,
    prepared: UsageManifest,
    progress: bool,
}

impl ReviewRetentionJob {
    fn prepare(root: &Path, cutoff: i64, inactive_cutoff: i64) -> Result<Option<Self>> {
        let pin = SnapshotPin::acquire(root)?;
        let source = ManifestStore::new(root.join("manifest.json")).load()?;
        if UsageJournal::new(root.join("events.jsonl"))
            .checkpoint()?
            .sequence
            != source.watermark
        {
            return Ok(None);
        }
        let mut prepared = source.clone();
        let stamp = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let progress = retire(root, &mut prepared, cutoff, inactive_cutoff, &stamp)?;
        Ok((prepared != source).then_some(Self {
            _pin: pin,
            root: root.to_path_buf(),
            source,
            prepared,
            progress,
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
        // Compare the complete source, including metadata-only maintenance
        // changes that do not advance snapshot_generation.
        if store.load()? != self.source {
            return Ok(false);
        }
        let journal_lock = storage_file::open_read_write(&self.root.join("events.lock"))?;
        match journal_lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        if UsageJournal::new(self.root.join("events.jsonl"))
            .checkpoint()?
            .sequence
            != self.source.watermark
        {
            return Ok(false);
        }
        store.save(&self.prepared)?;
        Ok(self.progress)
    }
}

pub(crate) fn maintain(root: &Path, cutoff: i64, inactive_cutoff: i64) -> Result<bool> {
    match ReviewRetentionJob::prepare(root, cutoff, inactive_cutoff)? {
        Some(job) => job.install(),
        None => Ok(false),
    }
}

/// Prepare one operation partition, contribution fragment and classification
/// sidecar, plus at most eight summary inputs. A snapshot pin must protect
/// these immutable inputs and unpublished outputs until manifest installation.
fn retire(
    root: &Path,
    manifest: &mut UsageManifest,
    cutoff_at_ms: i64,
    inactive_cutoff_at_ms: i64,
    stamp: &str,
) -> Result<bool> {
    let cutoff = manifest
        .review_retention_cutoff_at_ms
        .map_or(cutoff_at_ms, |previous| previous.max(cutoff_at_ms));
    let Some(mut descriptor) = load_review_projection(root, manifest)? else {
        manifest.review_retention_cutoff_at_ms = Some(cutoff);
        return Ok(false);
    };
    let original = descriptor.clone();
    if descriptor.retention_scan_remaining == 0 {
        descriptor.retention_scan_offset = 0;
        descriptor.retention_scan_remaining = descriptor
            .partitions
            .len()
            .max(descriptor.contribution_fragments.len())
            .max(descriptor.classification_files.len());
    }
    let index = descriptor.retention_scan_offset;
    if descriptor.retention_scan_remaining != 0 {
        descriptor.retention_scan_offset = index.saturating_add(1);
        descriptor.retention_scan_remaining -= 1;
    }
    let connection = Connection::open_in_memory().map_err(query_error)?;
    let inactive_threads = manifest
        .thread_last_activity_at_ms
        .iter()
        .filter(|(_, at)| **at < inactive_cutoff_at_ms)
        .map(|(thread, _)| thread.as_str())
        .collect::<BTreeSet<_>>();
    let inactive = if inactive_threads.is_empty() {
        "false".to_string()
    } else {
        format!(
            "COALESCE(thread_id IN ({}), false)",
            inactive_threads
                .iter()
                .map(|id| quoted(id))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let mut retired_operations = 0_u64;
    let mut retired_operation_ids = BTreeSet::new();
    if let Some(partition) = descriptor.partitions.get(index).cloned() {
        let source = parquet_source(root, std::slice::from_ref(&partition.file))?;
        let mut rows = query_rows::<ReviewOperation>(
            &connection,
            &format!("SELECT * FROM {source}"),
            REVIEW_PARTITION_SIZE as usize,
        )?;
        let expired = |row: &ReviewOperation| {
            row.terminal_observed
                && (row
                    .ended_at_ms
                    .or(row.started_at_ms)
                    .unwrap_or(row.observed_at_ms)
                    < cutoff
                    || row
                        .thread_id
                        .as_deref()
                        .is_some_and(|id| inactive_threads.contains(id)))
        };
        let candidates = rows
            .iter()
            .filter(|row| expired(row))
            .cloned()
            .collect::<Vec<_>>();
        let referenced = crate::review_aggregate::referenced_operations(
            root,
            &connection,
            &descriptor,
            &candidates,
        )?;
        let mut retired = Vec::new();
        rows.retain(|row| {
            let retire = expired(row) && !referenced.contains(&row.operation_id);
            if retire {
                retired_operation_ids.insert(row.operation_id.clone());
                retired.push(row.clone());
            }
            !retire
        });
        retired_operations = retired_operation_ids.len() as u64;
        if retired_operations != 0 {
            crate::review_aggregate::retain_operations(
                root,
                &connection,
                &mut descriptor,
                &retired,
                &inactive,
                stamp,
            )?;
            if rows.is_empty() {
                descriptor.partitions.remove(index);
                for partitions in descriptor.dimension_partitions.values_mut() {
                    partitions.retain(|value| *value != partition.partition);
                }
            } else {
                let mut retained = partition;
                retained.file = format!("review-operation-retained-{stamp}.parquet");
                retained.operation_index_file =
                    format!("review-operation-index-retained-{stamp}.parquet");
                retained.membership_file = format!("review-membership-retained-{stamp}.parquet");
                write_operation_rows(root, &retained.file, &rows)?;
                let source = parquet_source(root, std::slice::from_ref(&retained.file))?;
                write_query(
                    &connection,
                    root,
                    &retained.operation_index_file,
                    &format!(
                        "SELECT operation_id, first_sequence FROM {source} ORDER BY operation_id"
                    ),
                )?;
                retained.bounds = write_membership(
                    root,
                    &retained.membership_file,
                    &retained.file,
                    rows.iter().map(|row| IndexedInterval {
                        account_id: row.account_id.as_deref(),
                        thread_id: row.thread_id.as_deref(),
                        repository_id: row.repository_id.as_deref(),
                        model: row.model.as_deref(),
                        start: row.started_at_ms,
                        end: row.ended_at_ms,
                    }),
                )?;
                retained.operation_count = rows.len() as u32;
                retained.open_operations =
                    rows.iter().filter(|row| !row.terminal_observed).count() as u32;
                retained.operation_id_bloom = operation_bloom(&rows);
                descriptor.partitions[index] = retained;
            }
            // Per-partition memberships now name the rewritten file. The old
            // cumulative index cannot safely route queries to it.
            descriptor.membership_index_file = None;
        }
    }
    let mut retired_contributions = 0_u64;
    if let Some(fragment) = descriptor.contribution_fragments.get(index).cloned() {
        let previous_file = fragment.file.clone();
        let source = parquet_source(root, std::slice::from_ref(&fragment.file))?;
        // Raw detail must disappear first: until then a newly published token
        // correction can still target its identity. The corresponding review
        // row must remain available to consume that same signed correction.
        let mapping_complete = manifest
            .segment_files
            .iter()
            .all(|file| manifest.review_segment_contributions.contains_key(file));
        let raw_files = manifest
            .segment_files
            .iter()
            .filter(|file| {
                !mapping_complete
                    || manifest.review_segment_contributions[*file].contains(&fragment.file)
            })
            .cloned()
            .collect::<Vec<_>>();
        let raw_retired = if raw_files.is_empty() {
            "true".into()
        } else {
            let raw = parquet_source(root, &raw_files)?;
            format!("NOT EXISTS (SELECT 1 FROM {raw} raw WHERE raw.id = record_id)")
        };
        let predicate = format!("(observed_at_ms < {cutoff} OR {inactive}) AND ({raw_retired})");
        retired_contributions = connection
            .query_row(
                &format!("SELECT count(*) FROM {source} WHERE {predicate}"),
                [],
                |row| row.get(0),
            )
            .map_err(query_error)?;
        if retired_contributions != 0 {
            let file = format!("review-contribution-rollup-{stamp}.parquet");
            let bucket = format!("CASE WHEN {inactive} THEN NULL ELSE {} END", hour_bucket());
            let retiring = crate::review_aggregate::retained_contribution_source(
                root,
                manifest,
                &descriptor,
                &format!("(SELECT * FROM {source} WHERE {predicate})"),
            )?;
            write_query(
                &connection,
                root,
                &file,
                &contribution_rollup_query(&format!("({retiring})"), stamp, "detail", &bucket),
            )?;
            descriptor.contribution_rollup_files.push(file);
            let (first, last): (Option<i64>, Option<i64>) = connection.query_row(
                &format!("SELECT min(observed_at_ms), max(observed_at_ms) FROM {source} WHERE NOT ({predicate})"),
                [], |row| Ok((row.get(0)?, row.get(1)?)),
            ).map_err(query_error)?;
            let mut replacement_file = None;
            if let (Some(first), Some(last)) = (first, last) {
                let mut retained = fragment;
                retained.file = format!("review-contribution-retained-{stamp}.parquet");
                retained.membership_file =
                    format!("review-contribution-membership-retained-{stamp}.parquet");
                write_query(
                    &connection,
                    root,
                    &retained.file,
                    &format!("SELECT * FROM {source} WHERE NOT ({predicate})"),
                )?;
                let retained_source = parquet_source(root, std::slice::from_ref(&retained.file))?;
                let dimensions = [
                    ("all", "''"),
                    ("account", "account_id"),
                    ("thread", "thread_id"),
                    ("repository", "repository_id"),
                    ("model", "model"),
                ]
                .into_iter()
                .map(|(kind, id)| {
                    format!(
                        "SELECT '{kind}' AS dimension_kind, {id} AS dimension_id, observed_at_ms \
                            FROM {retained_source} WHERE {id} IS NOT NULL"
                    )
                })
                .collect::<Vec<_>>()
                .join(" UNION ALL ");
                write_query(
                    &connection,
                    root,
                    &retained.membership_file,
                    &format!(
                        "SELECT dimension_kind, dimension_id, {} AS segment_file, \
                     min(observed_at_ms) AS min_started_at_ms, max(observed_at_ms) AS max_ended_at_ms, \
                     false AS unknown_start, false AS unknown_end FROM ({dimensions}) GROUP BY ALL",
                        quoted(&retained.file),
                    ),
                )?;
                retained.min_observed_at_ms = first;
                retained.max_observed_at_ms = last;
                replacement_file = Some(retained.file.clone());
                descriptor.contribution_fragments[index] = retained;
            } else {
                descriptor.contribution_fragments.remove(index);
            }
            for files in manifest.review_segment_contributions.values_mut() {
                files.retain_mut(|file| {
                    if *file != previous_file {
                        return true;
                    }
                    match &replacement_file {
                        Some(replacement) => {
                            file.clone_from(replacement);
                            true
                        }
                        None => false,
                    }
                });
            }
        }
    }
    let mut retired_classifications = 0_u64;
    if let Some(file) = descriptor.classification_files.get(index).cloned() {
        let source = parquet_source(root, std::slice::from_ref(&file))?;
        let indexes = descriptor
            .partitions
            .iter()
            .map(|partition| partition.operation_index_file.clone())
            .collect::<Vec<_>>();
        let live = if indexes.is_empty() {
            "false".to_string()
        } else {
            let indexes = parquet_source(root, &indexes)?;
            format!("EXISTS (SELECT 1 FROM {indexes} i WHERE i.operation_id = c.operation_id)")
        };
        let retired_ids = if retired_operation_ids.is_empty() {
            "false".to_string()
        } else {
            format!(
                "c.operation_id IN ({})",
                retired_operation_ids
                    .iter()
                    .map(|id| quoted(id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let predicate = format!("NOT ({live}) AND (c.observed_at_ms < {cutoff} OR {retired_ids})");
        let (total, retired): (u64, u64) = connection
            .query_row(
                &format!("SELECT count(*), count(*) FILTER (WHERE {predicate}) FROM {source} c"),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(query_error)?;
        retired_classifications = retired;
        if retired_classifications != 0 {
            if retired == total {
                descriptor.classification_files.remove(index);
            } else {
                let file = format!("review-classification-retained-{stamp}.parquet");
                write_query(
                    &connection,
                    root,
                    &file,
                    &format!("SELECT c.* FROM {source} c WHERE NOT ({predicate})"),
                )?;
                descriptor.classification_files[index] = file;
            }
        }
    }
    let inputs = descriptor
        .contribution_rollup_files
        .iter()
        .take(MAX_ROLLUP_INPUTS)
        .cloned()
        .collect::<Vec<_>>();
    let mut merged = false;
    if !inputs.is_empty() {
        let source = parquet_source(root, &inputs)?;
        let needs_conversion = inputs.len() > 1 || connection.query_row(&format!(
            "SELECT EXISTS (SELECT 1 FROM {source} WHERE {inactive} AND NOT starts_with(record_id, 'review-lifetime:'))"
        ), [], |row| row.get::<_, bool>(0)).map_err(query_error)?;
        if needs_conversion {
            let bucket = format!(
                "CASE WHEN {inactive} OR starts_with(record_id, 'review-lifetime:') THEN NULL ELSE {} END",
                hour_bucket()
            );
            let file = format!("review-contribution-merged-{stamp}.parquet");
            write_query(
                &connection,
                root,
                &file,
                &contribution_rollup_query(&source, stamp, "merged", &bucket),
            )?;
            descriptor.contribution_rollup_files.drain(..inputs.len());
            descriptor.contribution_rollup_files.push(file);
            merged = true;
        }
    }
    merged |= crate::review_aggregate::merge_operation_rollups(
        root,
        &connection,
        &mut descriptor,
        &inactive,
        stamp,
    )?;
    descriptor.review_retention_cutoff_at_ms = Some(cutoff);
    descriptor.retired_operation_rows = descriptor
        .retired_operation_rows
        .checked_add(retired_operations)
        .ok_or_else(|| StorageError::InvalidData("retired review rows overflow".into()))?;
    descriptor.retired_contribution_rows = descriptor
        .retired_contribution_rows
        .checked_add(retired_contributions)
        .ok_or_else(|| StorageError::InvalidData("retired contribution rows overflow".into()))?;
    if descriptor != original {
        let file = format!("review-projection-retained-{stamp}.json");
        write_json(root, &file, &descriptor)?;
        #[cfg(unix)]
        std::fs::File::open(root.join("segments"))?.sync_all()?;
        manifest.review_projection_file = Some(file);
    }
    manifest.review_retention_cutoff_at_ms = Some(cutoff);
    manifest.retired_review_operations = manifest
        .retired_review_operations
        .checked_add(retired_operations)
        .ok_or_else(|| StorageError::InvalidData("retired operation count overflow".into()))?;
    manifest.retired_review_contributions = manifest
        .retired_review_contributions
        .checked_add(retired_contributions)
        .ok_or_else(|| StorageError::InvalidData("retired contribution count overflow".into()))?;
    manifest.retired_review_classifications = manifest
        .retired_review_classifications
        .checked_add(retired_classifications)
        .ok_or_else(|| StorageError::InvalidData("retired classification count overflow".into()))?;
    let progress = retired_operations != 0
        || retired_contributions != 0
        || retired_classifications != 0
        || merged;
    if progress {
        manifest
            .snapshot_generation
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("snapshot generation exhausted".into()))?;
        manifest.mark_snapshot(manifest.watermark, manifest.freshness);
    }
    Ok(progress)
}

fn hour_bucket() -> &'static str {
    "observed_at_ms::HUGEINT - ((observed_at_ms::HUGEINT % 3600000 + 3600000) % 3600000)"
}

/// A generated lifetime id marks the absence of an hourly bucket, so resuming
/// a chat never recreates discarded detail. The observation time remains the
/// first represented observation; finite reports declare partial retention.
fn contribution_rollup_query(source: &str, stamp: &str, suffix: &str, bucket: &str) -> String {
    let dimensions = [
        "account_id",
        "thread_id",
        "repository_id",
        "model",
        "tool",
        "event",
        "coverage",
        "phase",
        "activity",
        "classification",
    ]
    .join(", ");
    let grouped = format!(
        "SELECT {bucket} AS bucket_at_ms, {dimensions}, \
         min(source_sequence) AS source_sequence, min(observed_at_ms) AS observed_at_ms, \
         sum(input_tokens::HUGEINT)::BIGINT AS input_tokens, \
         sum(cached_input_tokens::HUGEINT)::BIGINT AS cached_input_tokens, \
         sum(output_tokens::HUGEINT)::BIGINT AS output_tokens, \
         sum(reasoning_output_tokens::HUGEINT)::BIGINT AS reasoning_output_tokens, \
         CASE WHEN count(*) FILTER (WHERE provider_total_tokens IS NOT NULL) = 0 THEN NULL ELSE \
              sum(provider_total_tokens::HUGEINT)::UBIGINT END AS provider_total_tokens, \
         CASE WHEN count(*) FILTER (WHERE cache_write_input_tokens IS NOT NULL) = 0 THEN NULL ELSE \
              sum(cache_write_input_tokens::HUGEINT)::UBIGINT END AS cache_write_input_tokens, \
         sum(COALESCE(observation_count, 1))::UBIGINT AS observation_count, \
         sum(CASE WHEN provider_total_unknowns IS NOT NULL THEN provider_total_unknowns \
                  WHEN provider_total_tokens IS NULL THEN 1 ELSE 0 END)::UBIGINT AS provider_total_unknowns, \
         sum(CASE WHEN cache_write_unknowns IS NOT NULL THEN cache_write_unknowns \
                  WHEN cache_write_input_tokens IS NULL THEN 1 ELSE 0 END)::UBIGINT AS cache_write_unknowns, \
         NULLIF(sum(COALESCE(missing_observations_sum, missing_observations, 0)), 0)::UBIGINT AS missing_observations_sum \
         FROM {source} GROUP BY ALL"
    );
    format!(
        "SELECT concat(CASE WHEN bucket_at_ms IS NULL THEN 'review-lifetime:' ELSE 'review-rollup:' END, \
         '{stamp}:{suffix}:', row_number() OVER ()) AS record_id, source_sequence, \
         observed_at_ms, NULL::VARCHAR AS operation_id, NULL::VARCHAR AS response_id, \
         account_id, thread_id, repository_id, model, tool, event, coverage, phase, activity, \
         classification, input_tokens, cached_input_tokens, output_tokens, reasoning_output_tokens, \
         provider_total_tokens, cache_write_input_tokens, NULL::UBIGINT AS missing_observations, \
         observation_count, provider_total_unknowns, cache_write_unknowns, missing_observations_sum, \
         NULL::UBIGINT AS attempt_number, NULL::VARCHAR AS rework_of_operation_id \
         FROM ({grouped})"
    )
}

#[cfg(test)]
#[path = "review_retention_tests.rs"]
mod tests;
