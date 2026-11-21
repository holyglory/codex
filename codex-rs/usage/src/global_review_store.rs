//! Background factual analytics publication. Coordinator review jobs and policy
//! remain separate; this worker only materializes the existing native report.
use std::collections::BTreeSet;
use std::fs::File;
use std::fs::TryLockError;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crate::Freshness;
use crate::GlobalReviewSnapshot;
use crate::Result;
use crate::StorageError;
use crate::TimeRange;
use crate::UsageJournal;
use crate::UsageReader;
use crate::global_review::MAX_PUBLICATION_BYTES;
use crate::global_review::PUBLICATION;
use crate::global_review::Publication;
use crate::global_review_cancel::GlobalReviewCancellation;
use crate::reader_query::query_error;
use crate::review_storage::segment_path;
use crate::storage_file;

const COALESCE: Duration = Duration::from_secs(60);
const FOOTER_BUDGET: u64 = 64 * 1024 * 1024;

/// Bounded preparation can advance without attempting a full report build.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GlobalReviewPublication {
    Published,
    AwaitingCompaction,
}

struct WorkDirectory(PathBuf);
impl Drop for WorkDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One independent complete candidate. Capture never takes its writer lock.
pub struct GlobalReviewPublisher {
    reader: UsageReader,
    work: WorkDirectory,
    _writer: File,
    root: PathBuf,
    cancellation: Arc<GlobalReviewCancellation>,
    source_manifest_identity: String,
}

impl GlobalReviewPublisher {
    /// None means another builder owns the lock or the exact prefix is current.
    pub fn open_at(root: impl Into<PathBuf>) -> Result<Option<Self>> {
        Self::open_cancellable(root.into(), Arc::default())
    }

    pub(crate) fn open_cancellable(
        root: PathBuf,
        cancellation: Arc<GlobalReviewCancellation>,
    ) -> Result<Option<Self>> {
        cancellation.check()?;
        storage_file::ensure_usage_directory(&root)?;
        let writer = storage_file::open_read_write(&root.join(".global-review.lock"))?;
        match writer.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        for entry in std::fs::read_dir(&root)? {
            cancellation.check()?;
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("global-review-work-"))
                && entry.file_type()?.is_dir()
            {
                std::fs::remove_dir_all(entry.path())?;
            }
        }
        let (reader, source_manifest_identity) = source_reader(&root)?;
        if previous_publication(&root)?.is_some_and(|value| {
            let same_manifest = value.source_manifest_identity == source_manifest_identity;
            let snapshot = value.snapshot;
            same_manifest
                && snapshot.source_snapshot_generation == reader.manifest().snapshot_generation
                && snapshot.source_watermark == reader.manifest().watermark
                && snapshot.included_journal_watermark == reader.journal_watermark()
                && snapshot.source_freshness == reader.freshness()
                && snapshot.pending_corrections
                    == reader.manifest().pending_corrections.len() as u64
        }) {
            return Ok(None);
        }
        let work = WorkDirectory(root.join(format!(
            "global-review-work-{}-{}",
            std::process::id(),
            now_nanos()
        )));
        drop(storage_file::open_read_write(&work.0.join(".private"))?);
        cancellation.check()?;
        Ok(Some(Self {
            reader,
            work,
            _writer: writer,
            root,
            cancellation,
            source_manifest_identity,
        }))
    }

    /// Build only in a background worker or governed preparation helper. A
    /// failed candidate never replaces the previous completed publication.
    pub fn publish(&mut self) -> Result<GlobalReviewPublication> {
        self.cancellation.check()?;
        // Preparation is a finite bounded batch even if capture keeps adding
        // fragments. The ordinary derived index preserves progress across wakes.
        let started = Instant::now();
        for _ in 0..64 {
            self.cancellation.check()?;
            if started.elapsed() >= Duration::from_secs(10)
                || !crate::review_compaction::compact_one(&self.root)?
            {
                break;
            }
        }
        // Only this refresh can advance the candidate; every formula below uses
        // this same pinned immutable generation and bounded complete journal tail.
        (self.reader, self.source_manifest_identity) = source_reader(&self.root)?;
        if !planning_ready(&self.reader, &self.cancellation)? {
            return Ok(GlobalReviewPublication::AwaitingCompaction);
        }
        let spill = self.work.0.join("spill");
        drop(storage_file::open_read_write(&spill.join(".private"))?);
        let database = self.work.0.join("report.duckdb");
        let connection = crate::duckdb_connection::open_background_report(&database, &spill)
            .map_err(query_error)?;
        let _registration = self.cancellation.register(&connection)?;
        storage_file::protect_existing(&database)?;
        self.cancellation.check()?;
        let report = self
            .reader
            .background_performance_review(connection, &self.cancellation)?;
        self.cancellation.check()?;
        let manifest = self.reader.manifest();
        let latest = UsageJournal::new(self.root.join("events.jsonl")).checkpoint()?;
        let pending = latest
            .sequence
            .checked_sub(self.reader.journal_watermark())
            .ok_or_else(|| {
                StorageError::InvalidData("global analytics exceeds journal frontier".into())
            })?;
        let snapshot = GlobalReviewSnapshot {
            snapshot_id: format!(
                "global-g{}-j{}-{}",
                manifest.snapshot_generation,
                self.reader.journal_watermark(),
                now_nanos()
            ),
            source_snapshot_generation: manifest.snapshot_generation,
            source_watermark: manifest.watermark,
            included_journal_watermark: self.reader.journal_watermark(),
            source_watermark_at_ms: manifest.watermark_at_ms,
            source_updated_at_ms: manifest.updated_at.saturating_mul(1_000),
            source_freshness: self.reader.freshness(),
            published_at_ms: (now_nanos() / 1_000_000).min(i64::MAX as u128) as i64,
            pending_corrections: manifest.pending_corrections.len() as u64,
            retired_observations: manifest.retired_observations,
            retired_review_operations: manifest.retired_review_operations,
            retired_review_contributions: manifest.retired_review_contributions,
            unapplied_retention_records: manifest.unapplied_retention_records,
            unapplied_retention_corrections: manifest.unapplied_retention_corrections,
            latest_journal_watermark: latest.sequence,
            pending_entries: pending,
            freshness: if pending != 0 || !manifest.pending_corrections.is_empty() {
                Freshness::Stale
            } else {
                self.reader.freshness()
            },
        };
        if let Some(previous) = previous_publication(&self.root)?
            && (previous.snapshot.source_snapshot_generation > snapshot.source_snapshot_generation
                || previous.snapshot.source_watermark > snapshot.source_watermark
                || previous.snapshot.included_journal_watermark
                    > snapshot.included_journal_watermark)
        {
            return Err(StorageError::InvalidData(
                "global analytics publication would regress its prefix".into(),
            ));
        }
        let temporary = self.work.0.join("candidate.json");
        let mut file = storage_file::create_new(&temporary)?;
        serde_json::to_writer(
            LimitedWriter {
                file: &mut file,
                bytes: 0,
            },
            &Publication {
                version: 1,
                source_manifest_identity: self.source_manifest_identity.clone(),
                snapshot,
                report,
            },
        )?;
        file.sync_all()?;
        drop(file);
        self.cancellation.commit(|| {
            std::fs::rename(temporary, self.root.join(PUBLICATION))?;
            Ok(())
        })?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(GlobalReviewPublication::Published)
    }
}

/// Carry the identity of the manifest actually parsed by this pinned reader.
/// Publication may replace the path during admission; the older complete reader
/// remains valid and the next candidate observes the replacement's identity.
fn source_reader(root: &Path) -> Result<(UsageReader, String)> {
    let reader = UsageReader::open_at(root)?;
    let identity = format!("{:?}", reader.metadata_identity);
    Ok((reader, identity))
}

/// A malformed disposable result can be replaced from canonical facts. Read
/// failures and valid source identities still retain their normal protections.
fn previous_publication(root: &Path) -> Result<Option<Publication>> {
    match crate::global_review::load(root) {
        Ok(value) => Ok(value),
        Err(
            StorageError::Json(_)
            | StorageError::InvalidData(_)
            | StorageError::UnsupportedFormat(_),
        ) => Ok(None),
        Err(
            error @ (StorageError::Io(_)
            | StorageError::Columnar(_)
            | StorageError::OperationIndexAtCapacity),
        ) => Err(error),
    }
}

struct LimitedWriter<'a> {
    file: &'a mut File,
    bytes: u64,
}
impl Write for LimitedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.saturating_add(bytes.len() as u64) > MAX_PUBLICATION_BYTES {
            return Err(std::io::Error::other(
                "global analytics publication exceeds byte budget",
            ));
        }
        let written = self.file.write(bytes)?;
        self.bytes += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn planning_ready(reader: &UsageReader, cancellation: &GlobalReviewCancellation) -> Result<bool> {
    cancellation.check()?;
    let mut files = BTreeSet::new();
    if let Some(descriptor) =
        crate::review_projection::load_review_projection_shared(&reader.root, &reader.manifest)?
    {
        files.extend(
            descriptor
                .partitions
                .iter()
                .map(|partition| partition.file.clone()),
        );
        let selected =
            crate::review_compaction::selection(&reader.root, &descriptor, TimeRange::all());
        files.extend(selected.map_or_else(
            || {
                descriptor
                    .contribution_fragments
                    .iter()
                    .map(|fragment| fragment.file.clone())
                    .collect()
            },
            |rows| rows.into_iter().map(|(file, _)| file).collect::<Vec<_>>(),
        ));
        files.extend(descriptor.classification_files.iter().cloned());
        files.extend(descriptor.operation_rollup_files.iter().cloned());
        files.extend(descriptor.contribution_rollup_files.iter().cloned());
    }
    files.extend(reader.manifest.correction_files.iter().cloned());
    let mut bytes = 0u64;
    for file in files {
        cancellation.check()?;
        let mut input = File::open(segment_path(&reader.root, &file)?)?;
        let length = input.metadata()?.len();
        if length < 12 {
            return Err(StorageError::InvalidData(
                "invalid analytics Parquet footer".into(),
            ));
        }
        input.seek(SeekFrom::End(-8))?;
        let mut footer = [0; 8];
        input.read_exact(&mut footer)?;
        let size = u64::from(u32::from_le_bytes([
            footer[0], footer[1], footer[2], footer[3],
        ]));
        if &footer[4..] != b"PAR1" || size > length - 12 {
            return Err(StorageError::InvalidData(
                "invalid analytics Parquet footer".into(),
            ));
        }
        bytes = bytes
            .checked_add(size)
            .ok_or_else(|| StorageError::InvalidData("analytics footer budget overflow".into()))?;
        if bytes > FOOTER_BUDGET {
            return Ok(false);
        }
    }
    Ok(true)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

pub(crate) fn run(
    root: PathBuf,
    receiver: mpsc::Receiver<()>,
    cancellation: Arc<GlobalReviewCancellation>,
) {
    let mut next = Instant::now();
    loop {
        if cancellation.check().is_err() {
            return;
        }
        match receiver.recv_timeout(COALESCE) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        while let Some(wait) = next.checked_duration_since(Instant::now()) {
            if matches!(
                receiver.recv_timeout(wait),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ) {
                return;
            }
        }
        if GlobalReviewPublisher::open_cancellable(root.clone(), Arc::clone(&cancellation))
            .and_then(|value| {
                if let Some(mut publisher) = value {
                    publisher.publish()?;
                }
                Ok(())
            })
            .is_err()
        {
            tracing::debug!(
                "global analytics publication deferred; completed snapshot remains available"
            );
        }
        next = Instant::now() + COALESCE;
    }
}

#[cfg(test)]
#[path = "global_review_tests.rs"]
mod tests;
