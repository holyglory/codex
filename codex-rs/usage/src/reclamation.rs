//! Retire obsolete snapshot files after every older reader has released its pin.
//! Publication owns the snapshot lock; the short reader admission lock only
//! establishes that new readers can reference the current manifest exclusively.
use std::collections::BTreeSet;
use std::fs::File;
use std::fs::TryLockError;
use std::path::Path;

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
    let publisher = storage_file::open_read_write(&root.join(".snapshot.lock"))?;
    match publisher.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(Reclamation::default()),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    let readers = storage_file::open_read_write(&root.join(".readers.lock"))?;
    match readers.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(Reclamation::default()),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    let manifest = ManifestStore::new(root.join("manifest.json"))
        .load_shared()?
        .value;
    // No previous reader exists and the publisher lock prevents changes to the
    // manifest. New readers now pin the same retained files and logical frontier.
    drop(readers);
    if manifest.snapshot_generation == 0 {
        return Ok(Reclamation::default());
    }
    let retained = retained_files(root, &manifest)?;
    let mut result = Reclamation::default();
    for entry in std::fs::read_dir(root.join("segments"))? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if entry.file_type()?.is_file()
            && !retained.contains(name)
            && [
                "segment-",
                "correction-",
                "membership-",
                "lineage-",
                "record-id-",
                "review-",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            std::fs::remove_file(entry.path())?;
            result.files_removed += 1;
        }
    }
    result.journal_rotated = crate::journal_rotation::rotate_prefix(
        &root.join("events.jsonl"),
        JournalCheckpoint {
            sequence: manifest.watermark,
            byte_offset: manifest.journal_byte_offset,
        },
    )?;
    Ok(result)
}

fn retained_files(root: &Path, manifest: &UsageManifest) -> Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    files.extend(manifest.segment_files.iter().cloned());
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
        }
    }
    if let Some(review) = crate::review_projection::load_review_projection_shared(root, manifest)? {
        files.extend(review.counts_file.iter().cloned());
        files.extend(review.classification_files.iter().cloned());
        files.extend(review.membership_index_file.iter().cloned());
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
