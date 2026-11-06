//! Immutable operation-state partitions and append-only observation projections.
//! A manifest replacement publishes all files together; old readers keep their
//! exact descriptor and files even when an open operation subsequently finishes.
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use duckdb::Connection;
use serde::Deserialize;
use serde::Serialize;

use crate::JournalCheckpoint;
use crate::Result;
use crate::UsageManifest;
use crate::UsageRecord;
use crate::id_index::id_hash;
use crate::review_counts::ReviewCountsUpdate;
use crate::review_index::IndexedInterval;
use crate::review_index::ReviewIntervalBounds;
use crate::review_index::write_membership;
use crate::review_state::REVIEW_PARTITION_SIZE;
use crate::review_state::ReviewClassification;
use crate::review_state::ReviewContribution;
use crate::review_state::ReviewOperation;
use crate::review_state::invalid;
use crate::review_state::lifecycle;
use crate::review_storage::CLASSIFICATION_COLUMNS;
use crate::review_storage::CONTRIBUTION_COLUMNS;
use crate::review_storage::OPERATION_COLUMNS;
use crate::review_storage::OPERATION_INDEX_COLUMNS;
use crate::review_storage::parquet_source;
use crate::review_storage::query_rows;
use crate::review_storage::quoted;
use crate::review_storage::segment_path;
use crate::review_storage::write_json;
use crate::review_storage::write_rows;
use crate::storage_file;

/// One current bounded operation partition; replaced files remain pinned by
/// previous descriptors. Bounds cover every row, including unknown endpoints.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewPartition {
    pub partition: u64,
    pub file: String,
    /// Sorted id/first-sequence lookup; contains no operation observation data.
    pub operation_index_file: String,
    pub membership_file: String,
    pub operation_count: u32,
    pub open_operations: u32,
    pub bounds: ReviewIntervalBounds,
    /// A conservative Bloom filter for operation ids in this partition. An
    /// empty filter identifies descriptors written before the filter was
    /// introduced and therefore keeps the lookup conservative.
    #[serde(default)]
    pub operation_id_bloom: Vec<u64>,
}

/// Canonical record contributions in one immutable bounded publication batch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewContributionFragment {
    pub file: String,
    pub membership_file: String,
    pub min_observed_at_ms: i64,
    pub max_observed_at_ms: i64,
}

/// Pinned review schema and exact consumed journal frontier. A value other than
/// one for complete_from_sequence explicitly identifies unobserved older facts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewProjectionDescriptor {
    pub version: u32,
    pub watermark: JournalCheckpoint,
    pub complete_from_sequence: u64,
    pub partitions: Vec<ReviewPartition>,
    pub contribution_fragments: Vec<ReviewContributionFragment>,
    /// Compact dimension summaries, independent of operation data files.
    #[serde(default)]
    pub counts_file: Option<String>,
    /// Never filter taxonomy sidecars by correction observation time.
    pub classification_files: Vec<String>,
    /// Compact dimension-to-partition routing for finite repository/account
    /// review selection; missing entries fall back to membership files.
    #[serde(default)]
    pub dimension_partitions: BTreeMap<String, Vec<u64>>,
    /// One compact membership index for foreground finite-window selection.
    #[serde(default)]
    pub membership_index_file: Option<String>,
}

const BLOOM_BITS_PER_OPERATION: usize = 8;
const BLOOM_HASHES: usize = 4;

fn operation_bloom(rows: &[ReviewOperation]) -> Vec<u64> {
    let bits = (rows.len().max(1) * BLOOM_BITS_PER_OPERATION).next_power_of_two();
    let mut bloom = vec![0; bits.div_ceil(64)];
    for row in rows {
        bloom_insert(&mut bloom, &row.operation_id);
    }
    bloom
}

fn bloom_insert(bloom: &mut [u64], id: &str) {
    let bits = bloom.len() * 64;
    let hash = id_hash(id);
    for index in bloom_hashes(hash, bits) {
        bloom[index / 64] |= 1u64 << (index % 64);
    }
}

fn bloom_maybe_contains(bloom: &[u64], id: &str) -> bool {
    if bloom.is_empty() {
        return true;
    }
    let bits = bloom.len() * 64;
    let hash = id_hash(id);
    bloom_hashes(hash, bits)
        .into_iter()
        .all(|index| bloom[index / 64] & (1u64 << (index % 64)) != 0)
}

fn bloom_hashes(hash: u64, bits: usize) -> [usize; BLOOM_HASHES] {
    [
        hash,
        hash.rotate_left(17) ^ 0x9e3779b97f4a7c15,
        hash.rotate_left(31) ^ 0xc2b2ae3d27d4eb4f,
        hash.rotate_left(47) ^ 0x165667b19e3779f9,
    ]
    .map(|value| value as usize % bits)
}

/// Open only the immutable descriptor referenced by this manifest generation.
/// None means no operation coverage was published, never a history fallback.
pub fn load_review_projection(
    root: &Path,
    manifest: &UsageManifest,
) -> Result<Option<ReviewProjectionDescriptor>> {
    let descriptor = manifest
        .review_projection_file
        .as_deref()
        .map(|file| load_descriptor(root, file))
        .transpose()?;
    if descriptor.as_ref().is_some_and(|descriptor| {
        descriptor.watermark.sequence != manifest.watermark
            || descriptor.watermark.byte_offset != manifest.journal_byte_offset
    }) {
        return Err(invalid("review projection and manifest frontiers disagree"));
    }
    Ok(descriptor)
}

pub(crate) fn publish_review_projection(
    root: &Path,
    manifest: &UsageManifest,
    next: JournalCheckpoint,
    stamp: &str,
    records: &[(u64, UsageRecord)],
    connection: &Connection,
) -> Result<String> {
    let prior = JournalCheckpoint {
        sequence: manifest.watermark,
        byte_offset: manifest.journal_byte_offset,
    };
    let mut descriptor =
        load_review_projection(root, manifest)?.unwrap_or_else(|| ReviewProjectionDescriptor {
            version: 1,
            watermark: prior,
            complete_from_sequence: prior.sequence.saturating_add(1),
            partitions: Vec::new(),
            contribution_fragments: Vec::new(),
            counts_file: None,
            classification_files: Vec::new(),
            dimension_partitions: BTreeMap::new(),
            membership_index_file: None,
        });
    if descriptor.watermark != prior || next.sequence < prior.sequence {
        return Err(invalid("review projection and manifest frontiers disagree"));
    }
    let mut updates = BTreeMap::<String, Vec<(u64, &UsageRecord)>>::new();
    let mut contributions = Vec::new();
    let mut classifications = Vec::new();
    for (sequence, record) in records {
        if *sequence <= prior.sequence || *sequence > next.sequence {
            return Err(invalid("review fact lies outside publication frontier"));
        }
        contributions.push(ReviewContribution::from_fact(*sequence, record));
        if lifecycle(record).is_some() {
            let id = record
                .operation_id
                .as_ref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| invalid("operation lifecycle has no operation id"))?;
            updates
                .entry(id.clone())
                .or_default()
                .push((*sequence, record));
        }
        if record.event.as_deref() == Some("classification_correction") {
            classifications.push(ReviewClassification {
                record_id: record.id.clone(),
                operation_id: record
                    .operation_id
                    .clone()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| invalid("classification correction has no operation id"))?,
                source_sequence: *sequence,
                observed_at_ms: record.effective_observed_at_ms(),
                phase: record.phase.clone(),
                activity: record.activity.clone(),
                classification: record.classification.clone(),
            });
        }
    }
    let mut counts =
        ReviewCountsUpdate::new(root, descriptor.clone(), &classifications, connection);
    let mut partitions = descriptor
        .partitions
        .into_iter()
        .map(|partition| (partition.partition, partition))
        .collect::<BTreeMap<_, _>>();
    let addresses = find_addresses(
        root,
        &partitions,
        updates.keys().cloned().collect(),
        connection,
    )?;
    let mut affected = BTreeMap::<u64, Vec<(u64, &UsageRecord)>>::new();
    for (id, mut facts) in updates {
        facts.sort_by_key(|(sequence, _)| *sequence);
        let first = addresses.get(&id).copied().unwrap_or(facts[0].0);
        let partition = first
            .checked_sub(1)
            .ok_or_else(|| invalid("zero operation sequence"))?
            / REVIEW_PARTITION_SIZE;
        affected.entry(partition).or_default().extend(facts);
    }
    for (partition, mut facts) in affected {
        let mut states = BTreeMap::<String, ReviewOperation>::new();
        if let Some(previous) = partitions.get(&partition) {
            let source = parquet_source(root, std::slice::from_ref(&previous.file))?;
            for state in query_rows::<ReviewOperation>(
                connection,
                &format!("SELECT * FROM {source}"),
                REVIEW_PARTITION_SIZE as usize,
            )? {
                if state.first_sequence == 0
                    || (state.first_sequence - 1) / REVIEW_PARTITION_SIZE != partition
                    || states.insert(state.operation_id.clone(), state).is_some()
                {
                    return Err(invalid("invalid operation partition identity"));
                }
            }
            if states.len() != previous.operation_count as usize {
                return Err(invalid("operation partition count mismatch"));
            }
        }
        let before = states.values().cloned().collect::<Vec<_>>();
        facts.sort_by_key(|(sequence, _)| *sequence);
        for (sequence, fact) in facts {
            let id = fact
                .operation_id
                .as_ref()
                .ok_or_else(|| invalid("validated lifecycle identity is missing"))?;
            if let Some(state) = states.get_mut(id) {
                state.merge(sequence, fact)?;
            } else {
                if addresses.contains_key(id) {
                    return Err(invalid("indexed operation missing from its partition"));
                }
                states.insert(id.clone(), ReviewOperation::from_fact(sequence, fact)?);
            }
        }
        if states.len() > REVIEW_PARTITION_SIZE as usize {
            return Err(invalid("operation partition exceeds fixed capacity"));
        }
        let mut rows = states.into_values().collect::<Vec<_>>();
        let dimensions = rows
            .iter()
            .flat_map(|row| {
                [
                    row.account_id.as_deref().map(|id| format!("account:{id}")),
                    row.thread_id.as_deref().map(|id| format!("thread:{id}")),
                    row.repository_id
                        .as_deref()
                        .map(|id| format!("repository:{id}")),
                    row.model.as_deref().map(|id| format!("model:{id}")),
                ]
                .into_iter()
                .flatten()
            })
            .collect::<BTreeSet<_>>();
        counts.replace_operations(&before, &rows)?;
        rows.sort_by(|a, b| {
            (&a.repository_id, a.started_at_ms, &a.operation_id).cmp(&(
                &b.repository_id,
                b.started_at_ms,
                &b.operation_id,
            ))
        });
        let file = format!("review-operation-{stamp}-{partition:020}.parquet");
        let membership_file = format!("review-membership-{stamp}-{partition:020}.parquet");
        write_rows(root, &file, &rows, OPERATION_COLUMNS)?;
        let operation_index_file = if let Some(previous) = partitions.get(&partition)
            && previous.operation_count as usize == rows.len()
        {
            previous.operation_index_file.clone()
        } else {
            let index_file = format!("review-operation-index-{stamp}-{partition:020}.parquet");
            let mut addresses = rows
                .iter()
                .map(|row| OperationAddress {
                    operation_id: row.operation_id.clone(),
                    first_sequence: row.first_sequence,
                })
                .collect::<Vec<_>>();
            addresses.sort_by(|a, b| a.operation_id.cmp(&b.operation_id));
            write_rows(root, &index_file, &addresses, OPERATION_INDEX_COLUMNS)?;
            index_file
        };
        let bounds = write_membership(
            root,
            &membership_file,
            &file,
            rows.iter().map(|row| IndexedInterval {
                account_id: row.account_id.as_deref(),
                thread_id: row.thread_id.as_deref(),
                repository_id: row.repository_id.as_deref(),
                model: row.model.as_deref(),
                start: row.started_at_ms,
                end: row.ended_at_ms,
            }),
        )?;
        partitions.insert(
            partition,
            ReviewPartition {
                partition,
                file,
                operation_index_file,
                membership_file,
                operation_count: rows.len() as u32,
                open_operations: rows.iter().filter(|row| !row.terminal_observed).count() as u32,
                bounds,
                operation_id_bloom: operation_bloom(&rows),
            },
        );
        for values in descriptor.dimension_partitions.values_mut() {
            values.retain(|value| *value != partition);
        }
        for dimension in dimensions {
            descriptor
                .dimension_partitions
                .entry(dimension)
                .or_default()
                .push(partition);
        }
    }
    descriptor.partitions = partitions.into_values().collect();
    for values in descriptor.dimension_partitions.values_mut() {
        values.sort_unstable();
        values.dedup();
    }
    if !descriptor.partitions.is_empty() {
        let membership_index_file = format!("review-membership-index-{stamp}.parquet");
        let membership_source = parquet_source(
            root,
            &descriptor
                .partitions
                .iter()
                .map(|partition| partition.membership_file.clone())
                .collect::<Vec<_>>(),
        )?;
        let membership_path = segment_path(root, &membership_index_file)?;
        connection
            .execute_batch(&format!(
                "COPY (SELECT * FROM {membership_source}) TO {} (FORMAT PARQUET, COMPRESSION ZSTD)",
                quoted(&membership_path.to_string_lossy())
            ))
            .map_err(crate::review_storage::query_error)?;
        storage_file::finish_generated(&membership_path)?;
        descriptor.membership_index_file = Some(membership_index_file);
    }
    descriptor.counts_file = counts.finish(stamp)?;
    if !contributions.is_empty() {
        contributions.sort_by(|a, b| {
            (&a.repository_id, a.observed_at_ms, &a.record_id).cmp(&(
                &b.repository_id,
                b.observed_at_ms,
                &b.record_id,
            ))
        });
        let file = format!("review-contribution-{stamp}.parquet");
        let membership_file = format!("review-contribution-membership-{stamp}.parquet");
        write_rows(root, &file, &contributions, CONTRIBUTION_COLUMNS)?;
        let bounds = write_membership(
            root,
            &membership_file,
            &file,
            contributions.iter().map(|row| IndexedInterval {
                account_id: row.account_id.as_deref(),
                thread_id: row.thread_id.as_deref(),
                repository_id: row.repository_id.as_deref(),
                model: row.model.as_deref(),
                start: Some(row.observed_at_ms),
                end: Some(row.observed_at_ms),
            }),
        )?;
        let min_observed_at_ms = bounds
            .min_started_at_ms
            .ok_or_else(|| invalid("contribution membership has no start bound"))?;
        let max_ended_at_ms = bounds
            .max_ended_at_ms
            .ok_or_else(|| invalid("contribution membership has no end bound"))?;
        descriptor
            .contribution_fragments
            .push(ReviewContributionFragment {
                file,
                membership_file,
                min_observed_at_ms,
                max_observed_at_ms: max_ended_at_ms,
            });
    }
    if !classifications.is_empty() {
        classifications.sort_by(|a, b| {
            (&a.operation_id, a.source_sequence).cmp(&(&b.operation_id, b.source_sequence))
        });
        let file = format!("review-classification-{stamp}.parquet");
        write_rows(root, &file, &classifications, CLASSIFICATION_COLUMNS)?;
        descriptor.classification_files.push(file);
    }
    descriptor.watermark = next;
    let file = format!("review-projection-{stamp}.json");
    write_json(root, &file, &descriptor)?;
    Ok(file)
}

#[derive(Deserialize, Serialize)]
struct OperationAddress {
    operation_id: String,
    first_sequence: u64,
}

fn find_addresses(
    root: &Path,
    partitions: &BTreeMap<u64, ReviewPartition>,
    ids: Vec<String>,
    connection: &Connection,
) -> Result<BTreeMap<String, u64>> {
    if ids.is_empty() || partitions.is_empty() {
        return Ok(BTreeMap::new());
    }
    let candidate_partitions = partitions
        .values()
        .filter(|partition| {
            partition.operation_id_bloom.is_empty()
                || ids
                    .iter()
                    .any(|id| bloom_maybe_contains(&partition.operation_id_bloom, id))
        })
        .collect::<Vec<_>>();
    if candidate_partitions.is_empty() {
        return Ok(BTreeMap::new());
    }
    let source = parquet_source(
        root,
        &candidate_partitions
            .iter()
            .map(|p| p.operation_index_file.clone())
            .collect::<Vec<_>>(),
    )?;
    let candidates = ids
        .iter()
        .map(|id| quoted(id))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = query_rows::<OperationAddress>(
        connection,
        &format!(
            "SELECT operation_id, first_sequence FROM {source} WHERE operation_id IN ({candidates})"
        ),
        ids.len(),
    )?;
    let mut found = BTreeMap::new();
    for row in rows {
        if row.first_sequence == 0
            || !partitions.contains_key(&((row.first_sequence - 1) / REVIEW_PARTITION_SIZE))
            || found.insert(row.operation_id, row.first_sequence).is_some()
        {
            return Err(invalid("conflicting current operation addresses"));
        }
    }
    Ok(found)
}

/// Lookup only the requested canonical states. Id indexes choose current
/// partitions first; at most ids.len() operation rows are decoded into Rust.
pub(crate) fn load_review_operations(
    root: &Path,
    descriptor: &ReviewProjectionDescriptor,
    ids: &BTreeSet<String>,
    connection: &Connection,
) -> Result<Vec<ReviewOperation>> {
    let partitions = descriptor
        .partitions
        .iter()
        .cloned()
        .map(|partition| (partition.partition, partition))
        .collect::<BTreeMap<_, _>>();
    let addresses = find_addresses(root, &partitions, ids.iter().cloned().collect(), connection)?;
    if addresses.is_empty() {
        return Ok(Vec::new());
    }
    let files = addresses
        .values()
        .map(|first| {
            partitions[&((first - 1) / REVIEW_PARTITION_SIZE)]
                .file
                .clone()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let source = parquet_source(root, &files)?;
    let candidates = addresses
        .keys()
        .map(|id| quoted(id))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = query_rows::<ReviewOperation>(
        connection,
        &format!("SELECT * FROM {source} WHERE operation_id IN ({candidates})"),
        ids.len(),
    )?;
    let mut found = BTreeSet::new();
    for row in &rows {
        if addresses.get(&row.operation_id) != Some(&row.first_sequence)
            || !found.insert(row.operation_id.as_str())
        {
            return Err(invalid(
                "operation lookup disagrees with its current id index",
            ));
        }
    }
    if rows.len() != addresses.len() {
        return Err(invalid("operation lookup is missing indexed state"));
    }
    Ok(rows)
}

fn load_descriptor(root: &Path, file: &str) -> Result<ReviewProjectionDescriptor> {
    let path = segment_path(root, file)?;
    storage_file::protect_existing(&path)?;
    let descriptor: ReviewProjectionDescriptor = serde_json::from_slice(&std::fs::read(path)?)?;
    if descriptor.version != 1 || descriptor.complete_from_sequence == 0 {
        return Err(invalid("unsupported review projection descriptor"));
    }
    let mut previous = None;
    for partition in &descriptor.partitions {
        segment_path(root, &partition.file)?;
        segment_path(root, &partition.operation_index_file)?;
        segment_path(root, &partition.membership_file)?;
        if partition.operation_count == 0
            || partition.operation_count as u64 > REVIEW_PARTITION_SIZE
            || partition.open_operations > partition.operation_count
            || previous.is_some_and(|id| id >= partition.partition)
        {
            return Err(invalid("invalid review partition descriptor"));
        }
        previous = Some(partition.partition);
    }
    for fragment in &descriptor.contribution_fragments {
        segment_path(root, &fragment.file)?;
        segment_path(root, &fragment.membership_file)?;
        if fragment.min_observed_at_ms > fragment.max_observed_at_ms {
            return Err(invalid("inverted review contribution bounds"));
        }
    }
    for file in &descriptor.classification_files {
        segment_path(root, file)?;
    }
    if let Some(file) = &descriptor.counts_file {
        segment_path(root, file)?;
    }
    if let Some(file) = &descriptor.membership_index_file {
        segment_path(root, file)?;
    }
    Ok(descriptor)
}

#[cfg(test)]
#[path = "review_projection_tests.rs"]
mod tests;
