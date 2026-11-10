//! Operation-owned exact addresses. Raw event admission never governs these runs.
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

use crate::JournalCheckpoint;
use crate::Result;
use crate::StorageError;
use crate::id_index::ID_HASH_VERSION;
use crate::id_index::IdRunDescriptor;
use crate::id_index::MAX_ID_RUNS;
use crate::id_index::exact_id_lookup;
use crate::id_index::exact_id_lookup::Duplicates;
use crate::id_index::id_hash;
use crate::id_index::validate_runs;
use crate::id_index_run::ExactIdEntry;
use crate::id_index_run::RunWriter;
use crate::review_projection::ReviewPartition;
use crate::review_projection::ReviewProjectionDescriptor;
use crate::review_state::REVIEW_PARTITION_SIZE;
use crate::review_state::ReviewOperation;
use crate::review_state::invalid;
use crate::review_storage::segment_path;
use crate::review_storage::write_json;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OperationIdEntry {
    pub operation_id: String,
    pub first_sequence: u64,
}

impl ExactIdEntry for OperationIdEntry {
    #[cfg(test)]
    const LOOKUP_PROFILE_COMPONENT: &'static str = "operation-id-lookup";
    fn id(&self) -> &str {
        &self.operation_id
    }
    fn first_sequence(&self) -> u64 {
        self.first_sequence
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OperationIdRun {
    pub run: IdRunDescriptor,
    /// Canonical operation retirement revision already applied to this run.
    /// This is independent of the raw observation event-time admission floor.
    pub retired_through: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OperationIdIndex {
    pub version: u32,
    pub hash_version: u32,
    pub watermark: JournalCheckpoint,
    pub complete_from_sequence: u64,
    pub runs: Vec<OperationIdRun>,
}

/// Missing pointers on nonempty legacy projections retain their exact Parquet
/// lookup. Empty projections can start the new index without inferring history.
pub(crate) fn load(
    root: &Path,
    review: &ReviewProjectionDescriptor,
) -> Result<Option<OperationIdIndex>> {
    let Some(file) = &review.operation_id_index_file else {
        return Ok(
            (review.partitions.is_empty() && review.retired_operation_rows == 0).then(|| {
                OperationIdIndex {
                    version: 1,
                    hash_version: ID_HASH_VERSION,
                    watermark: review.watermark,
                    complete_from_sequence: review.complete_from_sequence,
                    runs: Vec::new(),
                }
            }),
        );
    };
    static CACHE: crate::metadata_cache::JsonCache<OperationIdIndex> =
        crate::metadata_cache::JsonCache::new();
    let index = CACHE.load(&segment_path(root, file)?)?.value;
    if index.version != 1
        || index.hash_version != ID_HASH_VERSION
        || index.watermark != review.watermark
        || index.complete_from_sequence != review.complete_from_sequence
        || index.runs.iter().any(|entry| {
            entry.retired_through > review.retired_operation_rows
                || entry.run.max_sequence > index.watermark.sequence
                || entry.run.retained_from_at_ms.is_some()
        })
    {
        return Err(invalid("invalid operation-id index coverage"));
    }
    validate_runs(root, index.runs.iter().map(|entry| &entry.run))?;
    Ok(Some(std::sync::Arc::unwrap_or_clone(index)))
}

pub(crate) fn find_addresses(
    root: &Path,
    index: &OperationIdIndex,
    partitions: &BTreeMap<u64, ReviewPartition>,
    ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, u64>> {
    Ok(exact_id_lookup::lookup::<OperationIdEntry>(
        root,
        index.runs.iter().map(|entry| &entry.run),
        ids,
        id_hash,
        Duplicates::Reject,
        |entry| {
            validate_entry(entry, index.watermark.sequence)?;
            Ok(partitions
                .get(&((entry.first_sequence - 1) / REVIEW_PARTITION_SIZE))
                .is_some_and(|partition| contains(partition, entry.first_sequence)))
        },
    )?
    .into_iter()
    .map(|(id, entry)| (id, entry.first_sequence))
    .collect())
}

/// Append only newly created canonical operations, then advance exact coverage.
/// Existing operation updates never append duplicate addresses.
pub(crate) fn publish(
    root: &Path,
    mut index: OperationIdIndex,
    next: JournalCheckpoint,
    retired_through: u64,
    stamp: &str,
    mut entries: Vec<OperationIdEntry>,
) -> Result<String> {
    if !entries.is_empty() && index.runs.len() >= MAX_ID_RUNS {
        return Err(StorageError::OperationIndexAtCapacity);
    }
    let mut ids = BTreeSet::new();
    for entry in &entries {
        validate_entry(entry, next.sequence)?;
        if entry.first_sequence <= index.watermark.sequence || !ids.insert(&entry.operation_id) {
            return Err(invalid(
                "operation-id addition lies outside its publication",
            ));
        }
    }
    entries.sort_by(|left, right| {
        (id_hash(&left.operation_id), &left.operation_id)
            .cmp(&(id_hash(&right.operation_id), &right.operation_id))
    });
    if !entries.is_empty() {
        let mut writer = RunWriter::new(
            root,
            &format!("review-operation-id-run-{stamp}.bin"),
            &format!("review-operation-id-fence-{stamp}.bin"),
            /*level*/ 0,
            entries.len() as u64,
        )?;
        for entry in &entries {
            writer.push_entry(entry)?;
        }
        index.runs.push(OperationIdRun {
            run: writer.finish()?,
            retired_through,
        });
    }
    index.watermark = next;
    write(root, &index, stamp)
}

pub(crate) fn write(root: &Path, index: &OperationIdIndex, stamp: &str) -> Result<String> {
    validate_runs(root, index.runs.iter().map(|entry| &entry.run))?;
    let file = format!("review-operation-id-index-{stamp}.json");
    write_json(root, &file, index)?;
    Ok(file)
}

pub(crate) fn validate_entry(entry: &OperationIdEntry, watermark: u64) -> Result<()> {
    if entry.operation_id.is_empty()
        || entry.first_sequence == 0
        || entry.first_sequence > watermark
    {
        return Err(invalid("invalid operation-id entry"));
    }
    Ok(())
}

/// A partition without prior retirement needs no bitmap: all of its indexed
/// addresses remain live. Once retirement occurs, membership is always exact.
pub(crate) fn contains(partition: &ReviewPartition, sequence: u64) -> bool {
    partition.operation_sequences.as_ref().is_none_or(|bits| {
        let offset = ((sequence - 1) % REVIEW_PARTITION_SIZE) as usize;
        bits[offset / 64] & (1_u64 << (offset % 64)) != 0
    })
}

pub(crate) fn live_sequences(rows: &[ReviewOperation]) -> Vec<u64> {
    let mut bits = vec![0; REVIEW_PARTITION_SIZE as usize / 64];
    for row in rows {
        let offset = ((row.first_sequence - 1) % REVIEW_PARTITION_SIZE) as usize;
        bits[offset / 64] |= 1_u64 << (offset % 64);
    }
    bits
}
