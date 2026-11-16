//! Optional immutable read runs. Canonical fragments remain the retention truth.
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::fs::TryLockError;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::Deserialize;
use serde::Serialize;

use crate::ManifestStore;
use crate::Result;
use crate::StorageError;
use crate::TimeRange;
use crate::reclamation::SnapshotPin;
use crate::review_projection::ReviewContributionFragment;
use crate::review_projection::ReviewProjectionDescriptor;
use crate::review_projection::load_review_projection_shared;
use crate::review_state::invalid;
use crate::review_storage::parquet_source;
use crate::review_storage::query_error;
use crate::review_storage::quoted;
use crate::review_storage::segment_path;
use crate::storage_file;

const INDEX: &str = "review-compaction.json";
const MAX_INPUT_FILES: usize = 128;
const INGEST_BATCH_FILES: usize = 32;
const MIN_FRAGMENTS: usize = 16;
const MAX_ROWS: u64 = 65_536;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SOURCES: usize = 16_384;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Run {
    sources: Vec<String>,
    data_file: String,
    membership_file: String,
    min_observed_at_ms: i64,
    max_observed_at_ms: i64,
    rows: u64,
    data_bytes: u64,
    membership_bytes: u64,
    data_modified_ns: Option<u128>,
    membership_modified_ns: Option<u128>,
    level: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Index {
    version: u32,
    runs: Vec<Run>,
    /// Sources without a coalescing partner in a bounded scan remain canonical.
    /// These are planning hints, never an exclusion from foreground reads.
    unmergeable_sources: BTreeSet<String>,
}

enum FragmentInput {
    Eligible(Run),
    Unmergeable,
}

type Canonical<'a> = HashMap<&'a str, &'a ReviewContributionFragment>;

impl Run {
    fn matches(&self, root: &Path, canonical: &Canonical<'_>) -> bool {
        if self.sources.is_empty()
            || self.sources.len() > MAX_SOURCES
            || self.sources.windows(2).any(|pair| pair[0] >= pair[1])
            || !self.data_file.starts_with("review-compacted-")
            || self.data_file.starts_with("review-compacted-membership-")
            || !self
                .membership_file
                .starts_with("review-compacted-membership-")
            || self.rows == 0
            || self.rows > MAX_ROWS
            || self.data_bytes > MAX_BYTES
            || self.data_bytes.saturating_add(self.membership_bytes) > MAX_BYTES
            || self.level == u32::MAX
        {
            return false;
        }
        let mut first = i64::MAX;
        let mut last = i64::MIN;
        for source in &self.sources {
            let Some(fragment) = canonical.get(source.as_str()) else {
                return false;
            };
            first = first.min(fragment.min_observed_at_ms);
            last = last.max(fragment.max_observed_at_ms);
        }
        first == self.min_observed_at_ms
            && last == self.max_observed_at_ms
            && [
                (&self.data_file, self.data_bytes, self.data_modified_ns),
                (
                    &self.membership_file,
                    self.membership_bytes,
                    self.membership_modified_ns,
                ),
            ]
            .into_iter()
            .all(|(file, bytes, modified)| {
                segment_path(root, file)
                    .ok()
                    .and_then(|path| std::fs::metadata(path).ok())
                    .is_some_and(|metadata| {
                        metadata.is_file()
                            && metadata.len() == bytes
                            && bytes > 0
                            && modified.is_some()
                            && modified_ns(&metadata) == modified
                    })
            })
    }
}

fn modified_ns(metadata: &std::fs::Metadata) -> Option<u128> {
    metadata
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|time| time.as_nanos())
}

fn load(root: &Path) -> Result<Option<Arc<Index>>> {
    static CACHE: crate::metadata_cache::JsonCache<Index> = crate::metadata_cache::JsonCache::new();
    let path = root.join(INDEX);
    match std::fs::metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let index = CACHE.load(&path)?.value;
    if index.version != 1 {
        return Err(invalid("invalid review compaction index"));
    }
    Ok(Some(index))
}

/// Routing hint only. Selection still proves every run against the reader's source set.
pub(crate) fn has_index(root: &Path) -> bool {
    load(root)
        .ok()
        .flatten()
        .is_some_and(|index| !index.runs.is_empty())
}

/// Substitute only whole runs from this exact canonical file set, before opening memberships.
pub(crate) fn selection(
    root: &Path,
    descriptor: &ReviewProjectionDescriptor,
    range: TimeRange,
) -> Option<Vec<(String, String)>> {
    let index = load(root).ok().flatten()?;
    if range.start_ms() >= range.end_ms() {
        return Some(Vec::new());
    }
    let canonical = descriptor
        .contribution_fragments
        .iter()
        .map(|fragment| (fragment.file.as_str(), fragment))
        .collect::<Canonical<'_>>();
    let mut covered = HashSet::new();
    let mut result = Vec::new();
    let mut substituted = false;
    for run in &index.runs {
        if !run.matches(root, &canonical)
            || run
                .sources
                .iter()
                .any(|source| covered.contains(source.as_str()))
        {
            continue;
        }
        substituted = true;
        covered.extend(run.sources.iter().map(String::as_str));
        if run.min_observed_at_ms < range.end_ms() && run.max_observed_at_ms >= range.start_ms() {
            result.push((run.data_file.clone(), run.membership_file.clone()));
        }
    }
    if !substituted {
        return None;
    }
    result.extend(
        descriptor
            .contribution_fragments
            .iter()
            .filter(|fragment| {
                !covered.contains(fragment.file.as_str())
                    && fragment.min_observed_at_ms < range.end_ms()
                    && fragment.max_observed_at_ms >= range.start_ms()
            })
            .map(|fragment| (fragment.file.clone(), fragment.membership_file.clone())),
    );
    Some(result)
}

/// Reclamation must keep every currently referenced output, including stale runs.
/// A corrupt pointer withholds reclamation rather than risking pinned data.
pub(crate) fn retained_files(root: &Path) -> Result<Vec<String>> {
    let Some(index) = load(root)? else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for run in &index.runs {
        for file in [&run.data_file, &run.membership_file] {
            segment_path(root, file)?;
            result.push(file.clone());
        }
    }
    Ok(result)
}

fn bounded(inputs: impl IntoIterator<Item = Run>) -> Vec<Run> {
    let mut result = Vec::new();
    let mut rows = 0_u64;
    let mut bytes = 0_u64;
    let mut sources = 0_usize;
    for input in inputs {
        if result.len() == MAX_INPUT_FILES {
            break;
        }
        let input_bytes = input.data_bytes.saturating_add(input.membership_bytes);
        if rows.saturating_add(input.rows) > MAX_ROWS
            || bytes.saturating_add(input_bytes) > MAX_BYTES
            || sources + input.sources.len() > MAX_SOURCES
        {
            continue;
        }
        rows += input.rows;
        bytes += input_bytes;
        sources += input.sources.len();
        result.push(input);
    }
    result
}

fn fragment_input(root: &Path, fragment: &ReviewContributionFragment) -> Result<FragmentInput> {
    let path = segment_path(root, &fragment.file)?;
    let data_metadata = std::fs::metadata(&path)?;
    let data_bytes = data_metadata.len();
    if data_bytes > MAX_BYTES {
        return Ok(FragmentInput::Unmergeable);
    }
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)
        .map_err(|error| StorageError::Columnar(error.to_string()))?;
    let rows = u64::try_from(reader.metadata().file_metadata().num_rows())
        .map_err(|_| invalid("negative review fragment row count"))?;
    if rows == 0 || rows > MAX_ROWS {
        return Ok(FragmentInput::Unmergeable);
    }
    let membership_metadata = std::fs::metadata(segment_path(root, &fragment.membership_file)?)?;
    if data_bytes.saturating_add(membership_metadata.len()) > MAX_BYTES {
        return Ok(FragmentInput::Unmergeable);
    }
    Ok(FragmentInput::Eligible(Run {
        sources: vec![fragment.file.clone()],
        data_file: fragment.file.clone(),
        membership_file: fragment.membership_file.clone(),
        min_observed_at_ms: fragment.min_observed_at_ms,
        max_observed_at_ms: fragment.max_observed_at_ms,
        rows,
        data_bytes,
        membership_bytes: membership_metadata.len(),
        data_modified_ns: modified_ns(&data_metadata),
        membership_modified_ns: modified_ns(&membership_metadata),
        level: 0,
    }))
}

fn scan_fragments<'a>(
    fragments: impl IntoIterator<Item = &'a ReviewContributionFragment>,
    unmergeable: &mut BTreeSet<String>,
    mut read: impl FnMut(&ReviewContributionFragment) -> Result<FragmentInput>,
) -> Result<(Vec<Run>, bool)> {
    let candidates = fragments
        .into_iter()
        .filter(|fragment| !unmergeable.contains(&fragment.file))
        .take(MAX_INPUT_FILES)
        .collect::<Vec<_>>();
    let mut inputs = Vec::new();
    let mut advanced = false;
    for fragment in &candidates {
        match read(fragment)? {
            FragmentInput::Eligible(input) => inputs.push(input),
            FragmentInput::Unmergeable => advanced |= unmergeable.insert(fragment.file.clone()),
        }
    }
    inputs.sort_by_key(|input| {
        (
            input.rows,
            input.data_bytes.saturating_add(input.membership_bytes),
        )
    });
    let inputs = bounded(inputs);
    if inputs.len() < 2 {
        // A prefix can also contain individually valid files that cannot be
        // combined under the group bound. Re-reading that prefix would starve
        // every later file just as surely as individually oversized sources.
        for fragment in candidates {
            advanced |= unmergeable.insert(fragment.file.clone());
        }
    }
    Ok((inputs, advanced))
}

fn stamp() -> Result<String> {
    Ok(format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| invalid("review compaction clock precedes epoch"))?
            .as_nanos()
    ))
}

/// The caller holds the snapshot lock and a pin through source revalidation.
fn save(root: &Path, index: &Index, stamp: &str) -> Result<()> {
    let temporary = root.join(format!("review-compaction-{stamp}.tmp"));
    let mut output = storage_file::create_new(&temporary)?;
    output.write_all(&serde_json::to_vec(index)?)?;
    output.sync_all()?;
    drop(output);
    #[cfg(unix)]
    File::open(root.join("segments"))?.sync_all()?;
    std::fs::rename(temporary, root.join(INDEX))?;
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    Ok(())
}

/// Perform one bounded derived rewrite; never change canonical fragments or their mappings.
/// Returns whether admission or publication advanced. The existing worker owns scheduling.
pub fn compact_one(root: &Path) -> Result<bool> {
    storage_file::ensure_usage_directory(root)?;
    let writer = storage_file::open_read_write(&root.join(".review-compaction.lock"))?;
    match writer.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    let _pin = SnapshotPin::acquire(root)?;
    let manifest = ManifestStore::new(root.join("manifest.json"))
        .load_shared()?
        .value;
    let Some(descriptor) = load_review_projection_shared(root, &manifest)? else {
        return Ok(false);
    };
    let canonical = descriptor
        .contribution_fragments
        .iter()
        .map(|fragment| (fragment.file.as_str(), fragment))
        .collect::<Canonical<'_>>();
    let mut index = load(root).ok().flatten().map_or(
        Index {
            version: 1,
            runs: Vec::new(),
            unmergeable_sources: BTreeSet::new(),
        },
        |index| (*index).clone(),
    );
    let previous_unmergeable = index.unmergeable_sources.len();
    index
        .unmergeable_sources
        .retain(|source| canonical.contains_key(source.as_str()));
    let mut admission_advanced = previous_unmergeable != index.unmergeable_sources.len();
    let mut covered = HashSet::new();
    index.runs.retain(|run| {
        let valid = run.matches(root, &canonical)
            && run
                .sources
                .iter()
                .all(|source| !covered.contains(source.as_str()));
        if valid {
            covered.extend(run.sources.iter().cloned());
        }
        valid
    });
    let uncovered = descriptor
        .contribution_fragments
        .iter()
        .filter(|fragment| {
            !covered.contains(fragment.file.as_str())
                && !index.unmergeable_sources.contains(&fragment.file)
        })
        .collect::<Vec<_>>();
    let minimum_fragments = if index.unmergeable_sources.is_empty() {
        MIN_FRAGMENTS
    } else {
        2
    };
    let mut inputs = Vec::new();
    let levels = index
        .runs
        .iter()
        .map(|run| run.level)
        .collect::<std::collections::BTreeSet<_>>();
    for level in levels {
        let mut runs = index
            .runs
            .iter()
            .filter(|run| run.level == level)
            .cloned()
            .collect::<Vec<_>>();
        if runs.len() < MAX_INPUT_FILES && uncovered.len() >= minimum_fragments {
            continue;
        }
        runs.sort_by_key(|run| (run.rows, run.data_bytes));
        let candidates = bounded(runs);
        if candidates.len() >= 2 {
            inputs = candidates;
            break;
        }
    }
    if inputs.is_empty() && uncovered.len() >= minimum_fragments {
        let (eligible, advanced) =
            scan_fragments(uncovered, &mut index.unmergeable_sources, |fragment| {
                fragment_input(root, fragment)
            })?;
        inputs = eligible;
        admission_advanced |= advanced;
    }
    if inputs.len() < 2 {
        if !admission_advanced {
            return Ok(false);
        }
        let publisher = storage_file::open_read_write(&root.join(".snapshot.lock"))?;
        publisher.lock()?;
        let current = ManifestStore::new(root.join("manifest.json"))
            .load_shared()?
            .value;
        let Some(current) = load_review_projection_shared(root, &current)? else {
            return Ok(false);
        };
        let canonical = current
            .contribution_fragments
            .iter()
            .map(|fragment| (fragment.file.as_str(), fragment))
            .collect::<Canonical<'_>>();
        index
            .unmergeable_sources
            .retain(|source| canonical.contains_key(source.as_str()));
        index.runs.retain(|run| run.matches(root, &canonical));
        save(root, &index, &stamp()?)?;
        return Ok(true);
    }
    let stamp = stamp()?;
    let data_file = format!("review-compacted-{stamp}.parquet");
    let membership_file = format!("review-compacted-membership-{stamp}.parquet");
    let connection = crate::duckdb_connection::open_in_memory().map_err(query_error)?;
    connection
        .execute_batch("SET threads=1; SET memory_limit='128MB'; SET max_temp_directory_size='0B'")
        .map_err(query_error)?;
    for (batch, chunk) in inputs.chunks(INGEST_BATCH_FILES).enumerate() {
        for (table, files) in [
            (
                "compaction_rows",
                chunk
                    .iter()
                    .map(|run| run.data_file.clone())
                    .collect::<Vec<_>>(),
            ),
            (
                "compaction_membership",
                chunk
                    .iter()
                    .map(|run| run.membership_file.clone())
                    .collect(),
            ),
        ] {
            let source = parquet_source(root, &files)?;
            let query = if batch == 0 {
                format!("CREATE TEMP TABLE {table} AS SELECT * FROM {source}")
            } else {
                format!("INSERT INTO {table} SELECT * FROM {source}")
            };
            connection.execute_batch(&query).map_err(query_error)?;
        }
    }
    let rows: u64 = connection
        .query_row("SELECT count(*)::UBIGINT FROM compaction_rows", [], |row| {
            row.get(0)
        })
        .map_err(query_error)?;
    if rows == 0 || rows > MAX_ROWS || rows != inputs.iter().map(|run| run.rows).sum::<u64>() {
        return Err(invalid(
            "review compaction row bound or source count mismatch",
        ));
    }
    let data_path = segment_path(root, &data_file)?;
    connection.execute_batch(&format!(
        "COPY (SELECT * FROM compaction_rows ORDER BY repository_id, observed_at_ms, record_id) \
         TO {} (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 2048)", quoted(&data_path.to_string_lossy())))
        .map_err(query_error)?;
    storage_file::finish_generated(&data_path)?;
    let membership_path = segment_path(root, &membership_file)?;
    connection.execute_batch(&format!(
        "COPY (SELECT dimension_kind, dimension_id, {} AS segment_file, \
         min(min_started_at_ms)::BIGINT min_started_at_ms, max(max_ended_at_ms)::BIGINT max_ended_at_ms, \
         bool_or(unknown_start) unknown_start, bool_or(unknown_end) unknown_end \
         FROM compaction_membership GROUP BY dimension_kind,dimension_id ORDER BY dimension_kind,dimension_id) \
         TO {} (FORMAT PARQUET, COMPRESSION ZSTD)", quoted(&data_file), quoted(&membership_path.to_string_lossy())))
        .map_err(query_error)?;
    storage_file::finish_generated(&membership_path)?;
    let mut sources = inputs
        .iter()
        .flat_map(|run| run.sources.iter().cloned())
        .collect::<Vec<_>>();
    sources.sort();
    let data_metadata = std::fs::metadata(data_path)?;
    let membership_metadata = std::fs::metadata(membership_path)?;
    let merged = Run {
        sources,
        data_file,
        membership_file,
        min_observed_at_ms: inputs
            .iter()
            .map(|run| run.min_observed_at_ms)
            .min()
            .unwrap_or_default(),
        max_observed_at_ms: inputs
            .iter()
            .map(|run| run.max_observed_at_ms)
            .max()
            .unwrap_or_default(),
        rows,
        data_bytes: data_metadata.len(),
        membership_bytes: membership_metadata.len(),
        data_modified_ns: modified_ns(&data_metadata),
        membership_modified_ns: modified_ns(&membership_metadata),
        level: inputs.iter().map(|run| run.level).max().unwrap_or_default() + 1,
    };
    let publisher = storage_file::open_read_write(&root.join(".snapshot.lock"))?;
    publisher.lock()?;
    let current = ManifestStore::new(root.join("manifest.json"))
        .load_shared()?
        .value;
    let Some(current) = load_review_projection_shared(root, &current)? else {
        return Ok(false);
    };
    let canonical = current
        .contribution_fragments
        .iter()
        .map(|fragment| (fragment.file.as_str(), fragment))
        .collect::<Canonical<'_>>();
    if !merged.matches(root, &canonical) {
        return Ok(false);
    }
    index
        .unmergeable_sources
        .retain(|source| canonical.contains_key(source.as_str()));
    index.runs.retain(|run| {
        run.matches(root, &canonical) && inputs.iter().all(|input| input.data_file != run.data_file)
    });
    index.runs.push(merged);
    save(root, &index, &stamp)?;
    Ok(true)
}

#[cfg(test)]
#[path = "review_compaction_tests.rs"]
mod tests;
