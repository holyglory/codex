//! Immutable token aggregates. A background pass retires one bounded capture
//! segment, retaining hourly totals or lifetime totals for inactive chats.
use std::collections::BTreeSet;
use std::fs::TryLockError;
use std::path::Path;

use duckdb::Connection;

use crate::ManifestStore;
use crate::Result;
use crate::RetentionPolicy;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::reader_query::CORRECTION_COLUMNS;
use crate::reader_query::RECORD_COLUMNS;
use crate::reader_query::milliseconds_expression;
use crate::reader_query::parquet_source;
use crate::reader_query::query_error;
use crate::reader_query::quoted;
use crate::review_storage::segment_path;
use crate::storage_file;

pub(crate) const DIMENSIONS: &[&str] = &[
    "account_id",
    "thread_id",
    "repository_id",
    "model",
    "tool",
    "phase",
    "activity",
    "classification",
    "event",
    "coverage",
    "outcome",
];
const TOKENS: &[&str] = &[
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
];

pub(crate) fn compact_one(root: &Path, policy: RetentionPolicy, now_ms: i64) -> Result<bool> {
    let review_changed = crate::review_retention::maintain(
        root,
        policy.raw_cutoff(now_ms),
        policy.inactive_cutoff(now_ms),
    )?;
    let lock = storage_file::open_read_write(&root.join(".snapshot.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(review_changed),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    let store = ManifestStore::new(root.join("manifest.json"));
    let mut manifest = store.load()?;
    if crate::UsageJournal::new(root.join("events.jsonl"))
        .checkpoint()?
        .sequence
        != manifest.watermark
    {
        return Ok(review_changed);
    }
    let cutoff = policy.raw_cutoff(now_ms);
    let cutoff_changed = manifest
        .correction_retention_cutoff_at_ms
        .is_none_or(|previous| cutoff > previous);
    if cutoff_changed {
        manifest.correction_retention_cutoff_at_ms = Some(cutoff);
    }
    if manifest.snapshot_generation == 0 || manifest.segment_files.is_empty() {
        if cutoff_changed || review_changed {
            store.save(&manifest)?;
        }
        return Ok(review_changed);
    }
    let connection = Connection::open_in_memory().map_err(query_error)?;
    let inactive = manifest
        .thread_last_activity_at_ms
        .iter()
        .filter(|(_, at)| **at < policy.inactive_cutoff(now_ms))
        .map(|(thread, _)| quoted(thread))
        .collect::<Vec<_>>();
    let inactive = if inactive.is_empty() {
        "false".into()
    } else {
        format!("COALESCE(thread_id IN ({}), false)", inactive.join(", "))
    };
    let observed = milliseconds_expression("observed_at");
    let mut retired = format!(
        "({observed} < {} OR ({inactive}))",
        policy.raw_cutoff(now_ms)
    );
    let directory = root.join("segments");
    let corrections = parquet_source(&directory, &manifest.correction_files, CORRECTION_COLUMNS)?;
    let corrections = format!(
        "SELECT * FROM ({corrections}) WHERE {}",
        crate::reader_query::eligible_corrections(&manifest)
    );
    if !manifest.pending_corrections.is_empty() {
        let pending = manifest
            .pending_corrections
            .iter()
            .map(|id| quoted(id))
            .collect::<Vec<_>>()
            .join(", ");
        retired = format!(
            "({retired} AND id NOT IN (SELECT record_id FROM ({corrections}) WHERE id IN ({pending})))"
        );
    }
    let mut selected = None;
    // Only metadata and one segment are selected per pass. Parquet row-group
    // statistics eliminate recent hours before any expired row is decoded.
    let count = manifest.segment_files.len();
    let scan_count = count.min(32);
    let scan_start = manifest.retention_scan_offset % count;
    for offset in 0..scan_count {
        let index = (scan_start + offset) % count;
        let file = &manifest.segment_files[index];
        let source = parquet_source(&directory, std::slice::from_ref(file), RECORD_COLUMNS)?;
        let exists: bool = connection
            .query_row(
                &format!("SELECT EXISTS (SELECT 1 FROM ({source}) WHERE {retired} LIMIT 1)"),
                [],
                |row| row.get(0),
            )
            .map_err(query_error)?;
        if exists {
            selected = Some((file.clone(), source));
            manifest.retention_scan_offset = index;
            break;
        }
    }
    let Some((file, source)) = selected else {
        let next_offset = (scan_start + scan_count) % count;
        if cutoff_changed || review_changed || manifest.retention_scan_offset != next_offset {
            manifest.retention_scan_offset = next_offset;
            store.save(&manifest)?;
        }
        return Ok(review_changed || scan_count < count);
    };
    let stamp = format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    connection.execute_batch(&format!(
        "CREATE TEMP TABLE retention_rows AS SELECT *, {observed} AS effective_at_ms FROM ({source}) WHERE {retired}"
    )).map_err(query_error)?;
    let retired_count: u64 = connection
        .query_row("SELECT count(*) FROM retention_rows", [], |row| row.get(0))
        .map_err(query_error)?;
    let token_sums = TOKENS
        .iter()
        .map(|column| {
            format!("sum(r.{column}::HUGEINT + COALESCE(c.{column}_delta, 0))::BIGINT AS {column}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let deltas = TOKENS
        .iter()
        .map(|column| format!("sum({column}_delta)::BIGINT AS {column}_delta"))
        .collect::<Vec<_>>()
        .join(", ");
    let rollup = format!("rollup-{stamp}.parquet");
    let dimensions = DIMENSIONS
        .iter()
        .map(|column| format!("r.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    let token_predicate = format!(
        "r.event = 'token_observation' OR r.provider_total_tokens IS NOT NULL OR {}",
        TOKENS
            .iter()
            .map(|column| format!("r.{column}::HUGEINT + COALESCE(c.{column}_delta,0) <> 0"))
            .collect::<Vec<_>>()
            .join(" OR ")
    );
    let query = format!(
        "SELECT CASE WHEN {inactive} THEN NULL ELSE \
         effective_at_ms::HUGEINT - ((effective_at_ms::HUGEINT % 3600000 + 3600000) % 3600000) END AS hour_at_ms, \
         {dimensions}, min(effective_at_ms) AS first_at_ms, max(effective_at_ms) AS last_at_ms, \
         count(*)::UBIGINT AS records, {token_sums}, \
         count(*) FILTER (WHERE {token_predicate})::UBIGINT AS token_observations, \
         sum(r.provider_total_tokens::HUGEINT)::BIGINT AS provider_total_tokens, \
         count(*) FILTER (WHERE ({token_predicate}) AND r.provider_total_tokens IS NULL)::UBIGINT AS provider_total_unknowns, \
         sum(r.cache_write_input_tokens::HUGEINT)::BIGINT AS cache_write_input_tokens, \
         count(*) FILTER (WHERE ({token_predicate}) AND r.cache_write_input_tokens IS NULL)::UBIGINT AS cache_write_unknowns, \
         sum(r.missing_observations::HUGEINT)::UBIGINT AS missing_observations \
         FROM retention_rows r LEFT JOIN \
         (SELECT record_id, {deltas} FROM ({corrections}) GROUP BY record_id) c \
         ON r.id = c.record_id GROUP BY ALL"
    );
    write_query(&connection, root, &rollup, &query)?;
    let retained_count: u64 = connection
        .query_row(
            &format!("SELECT count(*) FROM ({source}) WHERE NOT {retired}"),
            [],
            |row| row.get(0),
        )
        .map_err(query_error)?;
    let replacement = if retained_count == 0 {
        None
    } else {
        // Keep the original hour suffix for cheap reader time pruning.
        let hour = file
            .trim_end_matches(".parquet")
            .rsplit_once('-')
            .map_or("unknown", |(_, hour)| hour);
        let replacement = format!("segment-retained_{stamp}-{hour}.parquet");
        write_query(
            &connection,
            root,
            &replacement,
            &format!("SELECT * FROM ({source}) WHERE NOT {retired}"),
        )?;
        Some(replacement)
    };
    let memberships = manifest
        .membership_files
        .iter()
        .chain(manifest.membership_file.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut rewritten = Vec::new();
    for (index, membership) in memberships.into_iter().enumerate() {
        let path = quoted(&segment_path(root, &membership)?.to_string_lossy());
        let selected = quoted(&file);
        let affected: bool = connection.query_row(&format!(
            "SELECT EXISTS (SELECT 1 FROM read_parquet({path}) WHERE segment_file = {selected})"
        ), [], |row| row.get(0)).map_err(query_error)?;
        if !affected {
            rewritten.push(membership);
            continue;
        }
        let output = format!("membership-retained-{stamp}-{index}.parquet");
        let query = match &replacement {
            Some(replacement) => format!(
                "SELECT * REPLACE (CASE WHEN segment_file = {selected} THEN {} ELSE segment_file END AS segment_file) FROM read_parquet({path})",
                quoted(replacement)
            ),
            None => format!("SELECT * FROM read_parquet({path}) WHERE segment_file <> {selected}"),
        };
        write_query(&connection, root, &output, &query)?;
        rewritten.push(output);
    }
    manifest.membership_file = None;
    manifest.membership_files = rewritten;
    let contributions = manifest.review_segment_contributions.remove(&file);
    if let Some(replacement) = &replacement
        && let Some(contributions) = contributions
    {
        manifest
            .review_segment_contributions
            .insert(replacement.clone(), contributions);
    }
    manifest.segment_files.retain(|existing| existing != &file);
    manifest.segment_files.extend(replacement);
    manifest.token_rollup_files.push(rollup);
    manifest.retired_observations = manifest
        .retired_observations
        .checked_add(retired_count)
        .ok_or_else(|| {
            crate::StorageError::InvalidData("retired observation count overflow".into())
        })?;
    manifest.mark_snapshot(manifest.watermark, manifest.freshness);
    // Capture remains independent during Parquet work. Only installation needs
    // the writer barrier, so an earlier durable correction cannot lose its target.
    let writer = storage_file::open_read_write(&root.join("events.lock"))?;
    match writer.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(review_changed),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    if crate::UsageJournal::new(root.join("events.jsonl"))
        .checkpoint()?
        .sequence
        != manifest.watermark
    {
        return Ok(review_changed);
    }
    #[cfg(unix)]
    std::fs::File::open(&directory)?.sync_all()?;
    store.save(&manifest)?;
    Ok(true)
}

pub(crate) fn source(
    reader: &UsageReader,
    range: TimeRange,
    filter: &UsageFilter,
) -> Result<String> {
    if reader.manifest.token_rollup_files.is_empty() {
        return Ok(crate::reader_query::empty_source(&[
            ("repository_id", "VARCHAR"),
            ("first_at_ms", "BIGINT"),
            ("last_at_ms", "BIGINT"),
            ("input_tokens", "BIGINT"),
            ("cached_input_tokens", "BIGINT"),
            ("output_tokens", "BIGINT"),
            ("reasoning_output_tokens", "BIGINT"),
            ("records", "UBIGINT"),
        ]));
    }
    let source =
        crate::review_storage::parquet_source(&reader.root, &reader.manifest.token_rollup_files)?;
    let mut predicates = vec![format!(
        "first_at_ms >= {} AND last_at_ms < {}",
        range.start_ms(),
        range.end_ms()
    )];
    for (column, value) in [
        ("account_id", &filter.account_id),
        ("thread_id", &filter.thread_id),
        ("repository_id", &filter.repository_id),
        ("model", &filter.model),
    ] {
        if let Some(value) = value {
            predicates.push(format!("{column} = {}", quoted(value)));
        }
    }
    if let Some(threads) = &filter.thread_ids {
        predicates.push(if threads.is_empty() {
            "false".into()
        } else {
            format!(
                "thread_id IN ({})",
                threads
                    .iter()
                    .map(|id| quoted(id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
    }
    Ok(format!(
        "SELECT * FROM {source} WHERE {}",
        predicates.join(" AND ")
    ))
}

pub(crate) fn write_query(
    connection: &Connection,
    root: &Path,
    file: &str,
    query: &str,
) -> Result<()> {
    let path = segment_path(root, file)?;
    let target = quoted(&path.to_string_lossy());
    connection
        .execute_batch(&format!(
            "COPY ({query}) TO {target} (FORMAT PARQUET, COMPRESSION ZSTD)"
        ))
        .map_err(query_error)?;
    storage_file::finish_generated(&path)
}
