//! Retire obsolete snapshot files after every older reader has released its pin.
//! Publication owns the snapshot lock; the short reader admission lock only
//! establishes that new readers can reference the current manifest exclusively.
use std::collections::BTreeSet;
use std::fs::File;
use std::fs::TryLockError;
use std::path::Path;
use std::path::PathBuf;

use crate::JournalCheckpoint;
use crate::ManifestStore;
use crate::Result;
use crate::UsageManifest;
use crate::storage_file;

#[derive(Debug)]
pub(crate) struct SnapshotPin {
    _file: File,
}

impl SnapshotPin {
    pub(crate) fn acquire(root: &Path) -> Result<Self> {
        let file = storage_file::open_read_write(&root.join(".readers.lock"))?;
        file.lock_shared()?;
        Ok(Self { _file: file })
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct Reclamation {
    pub files_removed: usize,
    pub journal_rotated: bool,
}

/// Maintenance never waits for an active publisher or a pinned report.
pub(crate) fn reclaim(root: &Path) -> Result<Reclamation> {
    match ReclamationPlan::prepare(root)? {
        Some(plan) => plan.reclaim(),
        None => Ok(Reclamation::default()),
    }
}

/// A closed set of candidates collected before off-lock workers can acquire a
/// new snapshot pin. Publication stays locked until deletion has finished.
pub(crate) struct ReclamationPlan {
    _publisher: File,
    root: PathBuf,
    checkpoint: JournalCheckpoint,
    candidates: Vec<std::fs::DirEntry>,
    retained: BTreeSet<String>,
}

impl ReclamationPlan {
    pub(crate) fn prepare(root: &Path) -> Result<Option<Self>> {
        let publisher = storage_file::open_read_write(&root.join(".snapshot.lock"))?;
        match publisher.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let readers = storage_file::open_read_write(&root.join(".readers.lock"))?;
        match readers.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let manifest = ManifestStore::new(root.join("manifest.json"))
            .load_shared()?
            .value;
        if manifest.snapshot_generation == 0 {
            return Ok(None);
        }
        // Every immutable producer holds the publisher lock or a snapshot pin.
        // Collect names while both admission paths are closed, then let new
        // readers and workers proceed during sidecar reads and deletion. Their
        // unique output names cannot belong to this completed directory list.
        let candidates =
            std::fs::read_dir(root.join("segments"))?.collect::<std::io::Result<Vec<_>>>()?;
        drop(readers);
        let retained = retained_files(root, &manifest)?;
        Ok(Some(Self {
            _publisher: publisher,
            root: root.to_path_buf(),
            checkpoint: JournalCheckpoint {
                sequence: manifest.watermark,
                byte_offset: manifest.journal_byte_offset,
            },
            candidates,
            retained,
        }))
    }

    pub(crate) fn reclaim(self) -> Result<Reclamation> {
        let mut result = Reclamation::default();
        for entry in self.candidates {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if entry.file_type()?.is_file()
                && !self.retained.contains(name)
                && [
                    "segment-",
                    "correction-",
                    "membership-",
                    "lineage-",
                    "record-id-",
                    "review-",
                    "rollup-",
                ]
                .iter()
                .any(|prefix| name.starts_with(prefix))
            {
                std::fs::remove_file(entry.path())?;
                result.files_removed += 1;
            }
        }
        result.journal_rotated = crate::journal_rotation::rotate_prefix(
            &self.root.join("events.jsonl"),
            self.checkpoint,
        )?;
        Ok(result)
    }
}

fn retained_files(root: &Path, manifest: &UsageManifest) -> Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    files.extend(manifest.segment_files.iter().cloned());
    files.extend(manifest.token_rollup_files.iter().cloned());
    files.extend(manifest.correction_files.iter().cloned());
    files.extend(manifest.membership_file.iter().cloned());
    files.extend(manifest.membership_files.iter().cloned());
    files.extend(manifest.lineage_file.iter().cloned());
    files.extend(manifest.id_index_file.iter().cloned());
    files.extend(manifest.review_projection_file.iter().cloned());
    if let Some(file) = &manifest.id_index_file {
        let index = crate::load_id_index(
            root,
            Some(file),
            JournalCheckpoint {
                sequence: manifest.watermark,
                byte_offset: manifest.journal_byte_offset,
            },
        )?;
        for run in index.runs {
            files.insert(run.data_file);
            files.insert(run.fence_file);
            files.extend(run.bloom_file);
        }
    }
    if let Some(review) = crate::review_projection::load_review_projection_shared(root, manifest)? {
        files.extend(review.operation_id_index_file.iter().cloned());
        if review.operation_id_index_file.is_some()
            && let Some(index) = crate::operation_id_index::load(root, &review)?
        {
            for entry in index.runs {
                files.insert(entry.run.data_file);
                files.insert(entry.run.fence_file);
                files.extend(entry.run.bloom_file);
            }
        }
        files.extend(review.counts_file.iter().cloned());
        files.extend(review.classification_files.iter().cloned());
        files.extend(review.membership_index_file.iter().cloned());
        files.extend(review.operation_rollup_files.iter().cloned());
        files.extend(review.contribution_rollup_files.iter().cloned());
        for partition in &review.partitions {
            files.insert(partition.file.clone());
            files.insert(partition.operation_index_file.clone());
            files.insert(partition.membership_file.clone());
        }
        for fragment in &review.contribution_fragments {
            files.insert(fragment.file.clone());
            files.insert(fragment.membership_file.clone());
        }
    }
    Ok(files)
}
