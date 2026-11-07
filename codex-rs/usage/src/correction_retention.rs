//! Bounded correction retirement with scan progress separate from snapshot work.
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::fs::TryLockError;
use std::io::Write;
use std::path::Path;

use duckdb::Connection;

use crate::ManifestStore;
use crate::Result;
use crate::UsageManifest;
use crate::detail_retention::segment_hour;
use crate::detail_retention::stamp;
use crate::reader_query::query_error;
use crate::reader_query::quoted;
use crate::review_storage::segment_path;
use crate::segment::ParquetCorrectionSegmentCodec;
use crate::segment::UsageCorrectionSegment;
use crate::storage_file;

const MAX_CACHED_FILES: usize = 16;
const MAX_BLOCKER_FILES: usize = 256;

/// A worker-owned bounded cache skips queries while the same immutable target
/// files still protect every correction in a previously checked sidecar.
#[derive(Default)]
pub(crate) struct Maintenance {
    protected: VecDeque<ProtectedFile>,
}

struct ProtectedFile {
    correction: String,
    blockers: BTreeSet<String>,
}

#[derive(Clone, Copy)]
enum Change {
    Cursor,
    Detail,
}

impl Maintenance {
    /// Inspect at most 32 file names and one bounded capture sidecar. Cursor
    /// movement persists without reporting retirement or advancing generation.
    pub(crate) fn step(&mut self, root: &Path) -> Result<bool> {
        let _pin = crate::reclamation::SnapshotPin::acquire(root)?;
        let store = ManifestStore::new(root.join("manifest.json"));
        let source = store.load()?;
        let Some(cutoff) = source.raw_admission_cutoff_at_ms else {
            return Ok(false);
        };
        let count = source.correction_files.len();
        if count == 0 {
            self.protected.clear();
            return Ok(false);
        }
        let start = source.correction_retention_scan_offset % count;
        let scanned = count.min(32);
        let selected = (0..scanned)
            .map(|offset| (start + offset) % count)
            .find(|index| {
                segment_hour(&source.correction_files[*index], "correction-").is_some_and(|hour| {
                    hour.saturating_mul(1_000).saturating_add(3_600_000) <= cutoff
                })
            });
        let mut candidate = source.clone();
        candidate.correction_retention_scan_offset =
            selected.map_or((start + scanned) % count, |i| (i + 1) % count);
        let Some(index) = selected else {
            return install_candidate(root, &store, &source, candidate, Change::Cursor);
        };
        let file = &source.correction_files[index];
        let review_files = crate::review_projection::load_review_projection(root, &source)?
            .map(|review| {
                review
                    .contribution_fragments
                    .into_iter()
                    .map(|fragment| fragment.file)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let live_files = source
            .segment_files
            .iter()
            .chain(&review_files)
            .cloned()
            .collect::<BTreeSet<_>>();
        self.protected
            .retain(|entry| source.correction_files.contains(&entry.correction));
        if self
            .protected
            .iter()
            .any(|entry| entry.correction == *file && entry.blockers.is_subset(&live_files))
        {
            return install_candidate(root, &store, &source, candidate, Change::Cursor);
        }
        let segment = ParquetCorrectionSegmentCodec
            .decode_corrections(&std::fs::read(segment_path(root, file)?)?)?;
        let ids = segment
            .corrections()
            .iter()
            .map(|row| quoted(&row.record_id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", ");
        let connection = Connection::open_in_memory().map_err(query_error)?;
        let mut retained_targets = BTreeSet::new();
        let mut blockers = BTreeSet::new();
        for (column, files) in [("id", &source.segment_files), ("record_id", &review_files)] {
            if files.is_empty() {
                continue;
            }
            let paths = files
                .iter()
                .map(|file| Ok(quoted(&segment_path(root, file)?.to_string_lossy())))
                .collect::<Result<Vec<_>>>()?
                .join(", ");
            let mut statement = connection.prepare(&format!(
                "SELECT DISTINCT {column}, filename FROM read_parquet([{paths}], union_by_name=true, filename=true) WHERE {column} IN ({ids})"
            )).map_err(query_error)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(query_error)?;
            for row in rows {
                let (id, path) = row.map_err(query_error)?;
                retained_targets.insert(id);
                let file = Path::new(&path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        crate::StorageError::InvalidData(
                            "correction target file has no name".into(),
                        )
                    })?;
                blockers.insert(file.to_string());
            }
        }
        let mut retained = Vec::new();
        for correction in segment.corrections() {
            if retained_targets.contains(&correction.record_id)
                && !source.expired_correction_ids.contains(&correction.id)
            {
                retained.push(correction.clone());
                continue;
            }
            if candidate.pending_corrections.contains(&correction.id) {
                candidate.unapplied_retention_corrections = candidate
                    .unapplied_retention_corrections
                    .checked_add(1)
                    .ok_or_else(|| {
                        crate::StorageError::InvalidData("expired correction count overflow".into())
                    })?;
                candidate
                    .pending_corrections
                    .retain(|id| id != &correction.id);
            }
            candidate.applied_correction_ids.remove(&correction.id);
            candidate.expired_correction_ids.remove(&correction.id);
        }
        if retained.len() == segment.corrections().len() {
            if blockers.len() <= MAX_BLOCKER_FILES {
                self.protected.retain(|entry| entry.correction != *file);
                self.protected.push_back(ProtectedFile {
                    correction: file.clone(),
                    blockers,
                });
                if self.protected.len() > MAX_CACHED_FILES {
                    self.protected.pop_front();
                }
            }
            return install_candidate(root, &store, &source, candidate, Change::Cursor);
        }
        if retained.is_empty() {
            candidate.correction_files.remove(index);
        } else {
            let replacement = format!("correction-{}-{}.parquet", stamp(), segment.hour_start());
            let rows = UsageCorrectionSegment::new(segment.hour_start(), retained)?;
            let path = segment_path(root, &replacement)?;
            let mut output = storage_file::create_new(&path)?;
            output.write_all(&ParquetCorrectionSegmentCodec.encode_corrections(&rows)?)?;
            output.sync_all()?;
            storage_file::finish_generated(&path)?;
            candidate.correction_files[index] = replacement;
        }
        candidate.summaries_include_corrections = candidate.pending_corrections.is_empty();
        install_candidate(root, &store, &source, candidate, Change::Detail)
    }
}

fn install_candidate(
    root: &Path,
    store: &ManifestStore,
    source: &UsageManifest,
    mut candidate: UsageManifest,
    change: Change,
) -> Result<bool> {
    if &candidate == source {
        return Ok(false);
    }
    let lock = storage_file::open_read_write(&root.join(".snapshot.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    if store.load()? != *source {
        return Ok(false);
    }
    let _writer = match change {
        Change::Cursor => None,
        Change::Detail => {
            let writer = storage_file::open_read_write(&root.join("events.lock"))?;
            match writer.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Ok(false),
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
            if crate::UsageJournal::new(root.join("events.jsonl"))
                .checkpoint()?
                .sequence
                != source.watermark
            {
                return Ok(false);
            }
            #[cfg(unix)]
            std::fs::File::open(root.join("segments"))?.sync_all()?;
            candidate.mark_snapshot(candidate.watermark, candidate.freshness);
            Some(writer)
        }
    };
    store.save(&candidate)?;
    Ok(matches!(change, Change::Detail))
}
