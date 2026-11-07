//! Immutable exact record-id runs.
//!
//! Hashes route a lookup to a small sorted page, but a hash is never treated
//! as identity: every candidate is compared with the complete record id.
//! Runs are immutable and a descriptor pins their filenames for readers.
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::JournalCheckpoint;
use crate::Result;
use crate::StorageError;
use crate::id_index_run::RunWriter;
use crate::id_index_run::next_frame;
use crate::storage_file;

/// Version of the stable id-routing hash. Changing it requires a new index.
pub const ID_HASH_VERSION: u32 = 1;
/// Maximum serialized entry size accepted by a run writer.
pub const MAX_ID_ENTRY_BYTES: usize = 16 * 1024 * 1024;
/// Number of entries represented by one fence page.
pub const ID_FENCE_STRIDE: u32 = 64;
/// Maximum number of immutable runs a foreground lookup will route across.
pub const MAX_ID_RUNS: usize = 256;
const FENCE_MAGIC: &[u8; 8] = b"CIDX1FEN";
const FENCE_BYTES: u64 = 16;
const MAX_FENCE_BYTES: u64 = 16 * 1024 * 1024;

/// Exact first-write identity and dimensions used by publisher deduplication.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordIdEntry {
    pub id: String,
    pub first_sequence: u64,
    pub account_id: Option<String>,
    pub thread_id: Option<String>,
    pub repository_id: Option<String>,
    pub segment_file: String,
}

/// Immutable sorted run metadata. Data and fence files are relative to the
/// private usage `segments` directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdRunDescriptor {
    pub data_file: String,
    pub fence_file: String,
    /// Optional immutable routing hint. Invalid hints fall back to exact probes.
    #[serde(default)]
    pub bloom_file: Option<String>,
    pub level: u8,
    pub entry_count: u64,
    pub fence_count: u64,
    pub min_hash: u64,
    pub max_hash: u64,
    pub min_sequence: u64,
    pub max_sequence: u64,
    pub data_bytes: u64,
    /// Whole-hour admission frontier already filtered from this run.
    #[serde(default)]
    pub retained_from_at_ms: Option<i64>,
}

/// Atomic snapshot of all runs visible to one reader.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdIndexDescriptor {
    pub version: u32,
    pub hash_version: u32,
    pub watermark: JournalCheckpoint,
    pub complete_from_sequence: u64,
    /// Absence below this event-time admission floor is intentionally unknown.
    /// Exact replay identity is retained for facts admitted at or above it.
    #[serde(default)]
    pub retained_from_at_ms: Option<i64>,
    pub runs: Vec<IdRunDescriptor>,
}

/// Result of a bounded lookup. A missing id is conclusive only when the
/// descriptor's sequence coverage and retained event-time admission floor cover
/// the caller's requested facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdLookup {
    pub found: BTreeMap<String, RecordIdEntry>,
    pub watermark: JournalCheckpoint,
    pub complete_from_sequence: u64,
}

/// Result of publishing an immutable run and its unreferenced descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdIndexWrite {
    pub file: String,
    pub descriptor: IdIndexDescriptor,
    pub run: Option<IdRunDescriptor>,
}

/// Stable, versioned FNV-1a routing hash. It only chooses a candidate page;
/// callers must compare full ids before accepting a match.
pub fn id_hash(id: &str) -> u64 {
    id.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

/// Build an explicit incomplete descriptor when no index pointer exists.
pub fn missing_id_index(checkpoint: JournalCheckpoint) -> IdIndexDescriptor {
    IdIndexDescriptor {
        version: 1,
        hash_version: ID_HASH_VERSION,
        watermark: checkpoint,
        complete_from_sequence: checkpoint.sequence.saturating_add(1),
        retained_from_at_ms: None,
        runs: Vec::new(),
    }
}

/// Load and validate a pinned index descriptor without reading run contents.
pub fn load_id_index(
    root: &Path,
    file: Option<&str>,
    checkpoint: JournalCheckpoint,
) -> Result<IdIndexDescriptor> {
    let Some(file) = file else {
        return Ok(missing_id_index(checkpoint));
    };
    let path = index_path(root, file)?;
    storage_file::protect_existing(&path)?;
    let descriptor: IdIndexDescriptor = serde_json::from_slice(&std::fs::read(path)?)?;
    validate_descriptor(root, &descriptor)?;
    if descriptor.watermark.sequence > checkpoint.sequence
        || (descriptor.watermark.sequence == checkpoint.sequence
            && descriptor.watermark.byte_offset > checkpoint.byte_offset)
    {
        return Err(invalid(
            "record-id index is ahead of its journal checkpoint",
        ));
    }
    Ok(descriptor)
}

/// Lookup exact ids through immutable fences, including complete hash-collision groups.
pub fn lookup_ids(
    root: &Path,
    descriptor: &IdIndexDescriptor,
    ids: &BTreeSet<String>,
) -> Result<IdLookup> {
    lookup_ids_with_hash(root, descriptor, ids, id_hash)
}

fn lookup_ids_with_hash(
    root: &Path,
    descriptor: &IdIndexDescriptor,
    ids: &BTreeSet<String>,
    hash_id: impl Fn(&str) -> u64,
) -> Result<IdLookup> {
    #[cfg(test)]
    let mut profile = crate::performance_tests::RecoveryProfile::new("record-id-lookup");
    validate_descriptor(root, descriptor)?;
    if ids.is_empty() {
        return Ok(IdLookup {
            found: BTreeMap::new(),
            watermark: descriptor.watermark,
            complete_from_sequence: descriptor.complete_from_sequence,
        });
    }
    let mut found = BTreeMap::<String, RecordIdEntry>::new();
    let mut by_hash = BTreeMap::<u64, Vec<&str>>::new();
    for id in ids {
        by_hash.entry(hash_id(id)).or_default().push(id);
    }
    for run in &descriptor.runs {
        let bloom = crate::id_index_bloom::load(root, run);
        #[cfg(test)]
        profile.mark("bloom-read");
        let candidates = by_hash
            .keys()
            .copied()
            .filter(|hash| bloom.as_ref().is_none_or(|hint| hint.might_contain(*hash)))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            continue;
        }
        let path = data_path(root, &run.data_file)?;
        let fence_path = data_path(root, &run.fence_file)?;
        storage_file::protect_existing(&path)?;
        storage_file::protect_existing(&fence_path)?;
        let mut data = BufReader::with_capacity(/*capacity*/ 64 * 1024, File::open(path)?);
        let fences = read_fences(&fence_path, run)?;
        #[cfg(test)]
        profile.mark("fence-read");
        let mut pages = BTreeMap::<u64, Vec<u64>>::new();
        for hash in candidates {
            if let Some(offset) = candidate_offset(&fences, hash) {
                pages.entry(offset).or_default().push(hash);
            }
        }
        for (offset, hashes) in pages {
            if offset < 8 || offset >= run.data_bytes {
                return Err(invalid("record-id fence points outside its run"));
            }
            data.seek(SeekFrom::Start(offset))?;
            let mut entry = next_frame(&mut data)?;
            for hash in hashes {
                while let Some(current) = entry.as_ref() {
                    let current_hash = hash_id(&current.id);
                    if current_hash > hash {
                        break;
                    }
                    if current_hash == hash && by_hash[&hash].contains(&current.id.as_str()) {
                        match found.get(&current.id) {
                            Some(previous) if previous.first_sequence < current.first_sequence => {}
                            Some(previous) if previous.first_sequence == current.first_sequence => {
                                if previous != current {
                                    return Err(invalid(
                                        "conflicting record-id facts at one sequence",
                                    ));
                                }
                            }
                            _ => {
                                found.insert(current.id.clone(), current.clone());
                            }
                        }
                    }
                    entry = next_frame(&mut data)?;
                }
                if entry.is_none() {
                    break;
                }
            }
        }
        #[cfg(test)]
        profile.mark("exact-page-probes");
    }
    Ok(IdLookup {
        found,
        watermark: descriptor.watermark,
        complete_from_sequence: descriptor.complete_from_sequence,
    })
}

/// Publish one immutable run and a new descriptor. The caller atomically pins
/// the returned descriptor from its manifest after its snapshot lock is held.
pub fn publish_id_index(
    root: &Path,
    previous: Option<&IdIndexDescriptor>,
    next: JournalCheckpoint,
    stamp: &str,
    entries: &[RecordIdEntry],
) -> Result<IdIndexWrite> {
    storage_file::ensure_usage_directory(root)?;
    let had_previous = previous.is_some();
    let previous = previous
        .cloned()
        .unwrap_or_else(|| missing_id_index(JournalCheckpoint::default()));
    validate_descriptor(root, &previous)?;
    if next.sequence < previous.watermark.sequence
        || (next.sequence == previous.watermark.sequence
            && next.byte_offset < previous.watermark.byte_offset)
    {
        return Err(invalid("record-id index checkpoint moved backwards"));
    }
    let canonical = canonical_entries(entries)?;
    if canonical.iter().any(|entry| {
        entry.first_sequence <= previous.watermark.sequence || entry.first_sequence > next.sequence
    }) {
        return Err(invalid("record-id index entry lies outside its checkpoint"));
    }
    let run = if canonical.is_empty() {
        None
    } else {
        Some(write_run(root, stamp, 0, &canonical)?)
    };
    let mut descriptor = previous;
    descriptor.watermark = next;
    if !had_previous {
        descriptor.complete_from_sequence = 1;
    }
    if let Some(run) = &run {
        descriptor.runs.push(run.clone());
    }
    if descriptor.runs.len() > MAX_ID_RUNS {
        // Publication remains valid; a separate compactor can reduce fanout.
        descriptor
            .runs
            .sort_by_key(|run| (run.level, run.min_sequence, run.data_file.clone()));
    }
    let file = format!("record-id-index-{stamp}.json");
    write_descriptor(root, &file, &descriptor)?;
    Ok(IdIndexWrite {
        file,
        descriptor,
        run,
    })
}

fn canonical_entries(entries: &[RecordIdEntry]) -> Result<Vec<RecordIdEntry>> {
    let mut canonical = BTreeMap::<String, RecordIdEntry>::new();
    for entry in entries {
        if entry.id.is_empty() || entry.first_sequence == 0 {
            return Err(invalid(
                "record-id index entry has empty id or zero sequence",
            ));
        }
        if entry.segment_file.is_empty() {
            return Err(invalid("record-id index entry has no segment file"));
        }
        match canonical.get(&entry.id) {
            Some(previous) if previous.first_sequence < entry.first_sequence => {}
            Some(previous)
                if previous.first_sequence == entry.first_sequence && previous != entry =>
            {
                return Err(invalid("conflicting record-id facts at one sequence"));
            }
            _ => {
                canonical.insert(entry.id.clone(), entry.clone());
            }
        }
    }
    let mut values = canonical.into_values().collect::<Vec<_>>();
    values.sort_by(|left, right| {
        id_hash(&left.id)
            .cmp(&id_hash(&right.id))
            .then_with(|| left.id.cmp(&right.id))
            .then_with(|| left.first_sequence.cmp(&right.first_sequence))
    });
    Ok(values)
}

pub(crate) fn write_run(
    root: &Path,
    stamp: &str,
    level: u8,
    entries: &[RecordIdEntry],
) -> Result<IdRunDescriptor> {
    if entries.is_empty() {
        return Err(invalid("cannot write an empty record-id run"));
    }
    let data_file = format!("record-id-run-{stamp}-l{level}.bin");
    let fence_file = format!("record-id-fence-{stamp}-l{level}.bin");
    let mut writer = RunWriter::new(root, &data_file, &fence_file, level, entries.len() as u64)?;
    for entry in entries {
        writer.push(entry)?;
    }
    writer.finish()
}

fn read_fences(path: &Path, run: &IdRunDescriptor) -> Result<Vec<(u64, u64)>> {
    let file = File::open(path)?;
    let expected_bytes = 8_u64
        .checked_add(run.fence_count.saturating_mul(FENCE_BYTES))
        .ok_or_else(|| invalid("record-id fence size overflow"))?;
    if file.metadata()?.len() != expected_bytes {
        return Err(invalid(
            "record-id fence length disagrees with its descriptor",
        ));
    }
    if expected_bytes > MAX_FENCE_BYTES {
        return Err(invalid(
            "record-id fence exceeds its bounded metadata limit",
        ));
    }
    let mut file = BufReader::new(file);
    let mut magic = [0; 8];
    file.read_exact(&mut magic)?;
    if &magic != FENCE_MAGIC {
        return Err(invalid("record-id fence has an invalid header"));
    }
    let mut fences = Vec::with_capacity(run.fence_count as usize);
    for _ in 0..run.fence_count {
        let mut encoded = [0; 16];
        file.read_exact(&mut encoded)?;
        let mut hash_bytes = [0; 8];
        hash_bytes.copy_from_slice(&encoded[..8]);
        let mut offset_bytes = [0; 8];
        offset_bytes.copy_from_slice(&encoded[8..]);
        fences.push((
            u64::from_le_bytes(hash_bytes),
            u64::from_le_bytes(offset_bytes),
        ));
    }
    Ok(fences)
}

fn candidate_offset(fences: &[(u64, u64)], hash: u64) -> Option<u64> {
    // An equal-hash group can start before its first matching fence and span
    // several pages. Begin at the preceding fence, or the first page when the
    // group contains the run's minimum hash.
    let first_equal_or_later = fences.partition_point(|(current, _)| *current < hash);
    fences
        .get(first_equal_or_later.saturating_sub(1))
        .and_then(|(current, offset)| (*current <= hash).then_some(*offset))
}

pub(crate) fn write_descriptor(
    root: &Path,
    file: &str,
    descriptor: &IdIndexDescriptor,
) -> Result<()> {
    let path = index_path(root, file)?;
    let mut encoded = serde_json::to_vec_pretty(descriptor)?;
    encoded.push(b'\n');
    let mut output = storage_file::create_new(&path)?;
    output.write_all(&encoded)?;
    output.sync_all()?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub(crate) fn index_path(root: &Path, file: &str) -> Result<PathBuf> {
    data_path(root, file)
}

pub(crate) fn data_path(root: &Path, file: &str) -> Result<PathBuf> {
    if Path::new(file).file_name().and_then(|name| name.to_str()) != Some(file) {
        return Err(invalid("record-id index path must be a file name"));
    }
    Ok(root.join("segments").join(file))
}

pub(crate) fn validate_descriptor(root: &Path, descriptor: &IdIndexDescriptor) -> Result<()> {
    if descriptor.version != 1
        || descriptor.hash_version != ID_HASH_VERSION
        || descriptor.complete_from_sequence == 0
    {
        return Err(invalid("unsupported record-id index descriptor"));
    }
    if descriptor.runs.len() > MAX_ID_RUNS {
        return Err(invalid("record-id index has too many foreground runs"));
    }
    let mut previous = None;
    for run in &descriptor.runs {
        if run.entry_count == 0
            || run.fence_count == 0
            || run.fence_count != run.entry_count.div_ceil(u64::from(ID_FENCE_STRIDE))
            || run.min_hash > run.max_hash
            || run.min_sequence == 0
            || run.min_sequence > run.max_sequence
            || previous.is_some_and(|seen: &str| seen == run.data_file)
        {
            return Err(invalid("invalid record-id index run metadata"));
        }
        data_path(root, &run.data_file)?;
        data_path(root, &run.fence_file)?;
        previous = Some(run.data_file.as_str());
    }
    Ok(())
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidData(message.into())
}

#[cfg(test)]
#[path = "id_index_tests.rs"]
mod tests;
