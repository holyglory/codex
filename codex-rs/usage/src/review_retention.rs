//! Bounded retirement of immutable review detail.
//!
//! All-time operation counts are already maintained in the manifest's
//! mergeable counters. Once a partition is entirely terminal and older than
//! the configured raw horizon, its row-level operation/contribution and
//! classification files can be dropped. Reports expose the resulting finite
//! window as partial retention coverage.
use std::collections::BTreeSet;
use std::path::Path;

use duckdb::Connection;

use crate::Result;
use crate::StorageError;
use crate::UsageManifest;
use crate::review_projection::load_review_projection;
use crate::review_storage::parquet_source;
use crate::review_storage::query_error;
use crate::review_storage::write_json;
use crate::review_storage::write_query;

/// Retire detail that is wholly before `cutoff_at_ms`. The caller must hold
/// the snapshot publication lock; all output is immutable and a new descriptor
/// is installed through the caller's manifest replacement.
pub(crate) fn retire(
    root: &Path,
    manifest: &mut UsageManifest,
    cutoff_at_ms: i64,
    inactive_cutoff_at_ms: i64,
    stamp: &str,
) -> Result<bool> {
    let Some(mut descriptor) = load_review_projection(root, manifest)? else {
        manifest.review_retention_cutoff_at_ms = Some(
            manifest
                .review_retention_cutoff_at_ms
                .map_or(cutoff_at_ms, |previous| previous.max(cutoff_at_ms)),
        );
        return Ok(false);
    };
    let connection = Connection::open_in_memory().map_err(query_error)?;
    let inactive_threads = manifest
        .thread_last_activity_at_ms
        .iter()
        .filter(|(_, at)| **at < inactive_cutoff_at_ms)
        .map(|(thread, _)| format!("'{}'", thread.replace('\'', "''")))
        .collect::<Vec<_>>();
    let inactive = if inactive_threads.is_empty() {
        "false".to_string()
    } else {
        format!(
            "COALESCE(thread_id IN ({}), false)",
            inactive_threads.join(", ")
        )
    };
    let retire_predicate = format!(
        "(COALESCE(ended_at_ms, started_at_ms, observed_at_ms) < {cutoff_at_ms} OR {inactive})"
    );
    let contribution_retire_predicate = format!("(observed_at_ms < {cutoff_at_ms} OR {inactive})");
    let mut changed = descriptor
        .review_retention_cutoff_at_ms
        .is_none_or(|previous| cutoff_at_ms > previous);
    let mut retired_operations = 0_u64;
    let mut retired_operation_ids = BTreeSet::new();
    let mut kept_partitions = Vec::with_capacity(descriptor.partitions.len());
    for partition in descriptor.partitions.iter() {
        let source = parquet_source(root, std::slice::from_ref(&partition.file))?;
        let (last_at, open, live): (Option<i64>, u64, u64) = connection
            .query_row(
                &format!(
                    "SELECT max(COALESCE(ended_at_ms, started_at_ms, observed_at_ms)), \
                     count(*) FILTER (WHERE NOT terminal_observed), \
                     count(*) FILTER (WHERE NOT ({retire_predicate})) FROM {source}"
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(query_error)?;
        if open == 0 && live == 0 && last_at.is_some() {
            let mut statement = connection
                .prepare(&format!("SELECT operation_id FROM {source}"))
                .map_err(query_error)?;
            let mut rows = statement.query([]).map_err(query_error)?;
            while let Some(row) = rows.next().map_err(query_error)? {
                retired_operation_ids.insert(row.get::<_, String>(0).map_err(query_error)?);
            }
            retired_operations = retired_operations
                .checked_add(u64::from(partition.operation_count))
                .ok_or_else(|| {
                    StorageError::InvalidData("retired operation count overflow".into())
                })?;
            changed = true;
        } else {
            kept_partitions.push(partition.clone());
        }
    }
    let mut kept_contributions = Vec::with_capacity(descriptor.contribution_fragments.len());
    let mut retired_contributions = 0_u64;
    let mut rollup_index = 0_usize;
    for fragment in &descriptor.contribution_fragments {
        let source = parquet_source(root, std::slice::from_ref(&fragment.file))?;
        let (count, live): (u64, u64) = connection
            .query_row(
                &format!(
                    "SELECT count(*), count(*) FILTER (WHERE NOT ({contribution_retire_predicate})) FROM {source}"
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(query_error)?;
        if count != 0 && live == 0 {
            let file = format!("review-contribution-rollup-{stamp}-{rollup_index}.parquet");
            write_query(
                &connection,
                root,
                &file,
                &contribution_rollup_query(&source, stamp, rollup_index),
            )?;
            descriptor.contribution_rollup_files.push(file);
            rollup_index = rollup_index.saturating_add(1);
            retired_contributions = retired_contributions.checked_add(count).ok_or_else(|| {
                StorageError::InvalidData("retired contribution count overflow".into())
            })?;
            changed = true;
        } else {
            kept_contributions.push(fragment.clone());
        }
    }
    let mut kept_classifications = Vec::with_capacity(descriptor.classification_files.len());
    let mut retired_classifications = 0_u64;
    for file in &descriptor.classification_files {
        let source = parquet_source(root, std::slice::from_ref(file))?;
        let retired = if retired_operation_ids.is_empty() {
            false
        } else {
            let ids = retired_operation_ids
                .iter()
                .map(|id| format!("'{}'", id.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            let (total, retired): (u64, u64) = connection
                .query_row(
                    &format!(
                        "SELECT count(*), count(*) FILTER (WHERE operation_id IN ({ids})) FROM {source}"
                    ),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(query_error)?;
            total != 0 && total == retired
        };
        if retired {
            retired_classifications = retired_classifications.saturating_add(1);
            changed = true;
        } else {
            kept_classifications.push(file.clone());
        }
    }
    if !changed {
        return Ok(false);
    }
    descriptor.partitions = kept_partitions;
    descriptor.contribution_fragments = kept_contributions;
    descriptor.classification_files = kept_classifications;
    if retired_operations != 0 {
        let kept = descriptor
            .partitions
            .iter()
            .map(|partition| partition.partition)
            .collect::<std::collections::BTreeSet<_>>();
        for partitions in descriptor.dimension_partitions.values_mut() {
            partitions.retain(|partition| kept.contains(partition));
        }
        // The cumulative index still contains rows for removed partitions.
        // Falling back to the surviving per-partition membership files keeps
        // selection conservative until a later publisher rebuilds the index.
        descriptor.membership_index_file = None;
    }
    descriptor.review_retention_cutoff_at_ms = Some(
        descriptor
            .review_retention_cutoff_at_ms
            .map_or(cutoff_at_ms, |previous| previous.max(cutoff_at_ms)),
    );
    descriptor.retired_operation_rows = descriptor
        .retired_operation_rows
        .checked_add(retired_operations)
        .ok_or_else(|| StorageError::InvalidData("retired review rows overflow".into()))?;
    descriptor.retired_contribution_rows = descriptor
        .retired_contribution_rows
        .checked_add(retired_contributions)
        .ok_or_else(|| StorageError::InvalidData("retired contribution rows overflow".into()))?;
    let file = format!("review-projection-retained-{stamp}.json");
    write_json(root, &file, &descriptor)?;
    #[cfg(unix)]
    std::fs::File::open(root.join("segments"))?.sync_all()?;
    manifest.review_projection_file = Some(file);
    manifest.review_retention_cutoff_at_ms = descriptor.review_retention_cutoff_at_ms;
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
    Ok(true)
}

/// Group old token observations by their report dimensions and UTC hour. The
/// row keeps weighted counters rather than pretending one aggregate row is one
/// observation. Operation and response ids are intentionally omitted after
/// detail retirement; operation pages remain detail-only and report coverage
/// exposes the resulting attribution gap.
fn contribution_rollup_query(source: &str, stamp: &str, rollup_index: usize) -> String {
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
        "SELECT observed_at_ms::HUGEINT - ((observed_at_ms::HUGEINT % 3600000 + 3600000) % 3600000) AS bucket_at_ms, {dimensions}, \
         min(source_sequence) AS source_sequence, min(observed_at_ms) AS observed_at_ms, \
         sum(input_tokens::HUGEINT)::UBIGINT AS input_tokens, \
         sum(cached_input_tokens::HUGEINT)::UBIGINT AS cached_input_tokens, \
         sum(output_tokens::HUGEINT)::UBIGINT AS output_tokens, \
         sum(reasoning_output_tokens::HUGEINT)::UBIGINT AS reasoning_output_tokens, \
         CASE WHEN count(*) FILTER (WHERE provider_total_tokens IS NOT NULL) = 0 THEN NULL ELSE \
              sum(provider_total_tokens::HUGEINT)::UBIGINT END AS provider_total_tokens, \
         CASE WHEN count(*) FILTER (WHERE cache_write_input_tokens IS NOT NULL) = 0 THEN NULL ELSE \
              sum(cache_write_input_tokens::HUGEINT)::UBIGINT END AS cache_write_input_tokens, \
         sum(CASE WHEN observation_count IS NULL THEN 1 ELSE observation_count END)::UBIGINT AS observation_count, \
         sum(CASE WHEN provider_total_unknowns IS NOT NULL THEN provider_total_unknowns \
                  WHEN provider_total_tokens IS NULL THEN 1 ELSE 0 END)::UBIGINT AS provider_total_unknowns, \
         sum(CASE WHEN cache_write_unknowns IS NOT NULL THEN cache_write_unknowns \
                  WHEN cache_write_input_tokens IS NULL THEN 1 ELSE 0 END)::UBIGINT AS cache_write_unknowns, \
         CASE WHEN sum(CASE WHEN missing_observations_sum IS NOT NULL THEN missing_observations_sum \
                            ELSE COALESCE(missing_observations, 0) END) = 0 THEN NULL ELSE \
              sum(CASE WHEN missing_observations_sum IS NOT NULL THEN missing_observations_sum \
                       ELSE COALESCE(missing_observations, 0) END)::UBIGINT END AS missing_observations_sum \
         FROM {source} GROUP BY ALL"
    );
    format!(
        "SELECT concat('review-rollup:{stamp}:{rollup_index}:', row_number() OVER ()) AS record_id, source_sequence, \
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
