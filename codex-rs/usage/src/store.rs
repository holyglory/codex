use std::collections::BTreeMap;
use std::fs::{self};
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use crate::duckdb_snapshot::DuckDbSnapshot;
use crate::error::Result;
use crate::error::StorageError;
use crate::journal::JournalCheckpoint;
use crate::journal::JournalPayload;
use crate::journal::JournalPosition;
use crate::journal::UsageJournal;
use crate::manifest::MANIFEST_FORMAT_VERSION;
use crate::manifest::ManifestStore;
use crate::manifest::UsageManifest;
use crate::model::Freshness;
use crate::model::JournalSequence;
use crate::model::TimeRange;
use crate::model::UsageCorrection;
use crate::model::UsageFilter;
use crate::model::UsageRecord;
use crate::model::UsageTotals;
use crate::query::InMemoryUsageStore;
use crate::query::UsageSnapshot;
use crate::segment::ParquetCorrectionSegmentCodec;
use crate::segment::ParquetSegmentCodec;
use crate::segment::SegmentCodec;
use crate::segment::UsageCorrectionSegment;
use crate::segment::UsageSegment;
use crate::storage_file;

/// Directory name reserved for the local usage-v2 storage layout.
pub const USAGE_DIRECTORY_NAME: &str = "usage-v2";
const JOURNAL_FILE_NAME: &str = "events.jsonl";
const MANIFEST_FILE_NAME: &str = "manifest.json";
const SEGMENTS_DIRECTORY_NAME: &str = "segments";

/// Durable local store combining an append-only journal with immutable hourly
/// segments and an in-memory snapshot/tail query view.
#[derive(Debug)]
pub struct UsageStore {
    root: PathBuf,
    journal: UsageJournal,
    manifest_store: ManifestStore,
    manifest: UsageManifest,
    memory: InMemoryUsageStore,
    duckdb_snapshot: DuckDbSnapshot,
}

impl UsageStore {
    /// Open a store under `<codex_home>/usage-v2`.
    pub fn open(codex_home: impl Into<PathBuf>) -> Result<Self> {
        Self::open_at(codex_home.into().join(USAGE_DIRECTORY_NAME))
    }

    /// Open a store at an already-resolved `usage-v2` directory.
    pub fn open_at(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        storage_file::ensure_usage_directory(&root)?;
        let journal = UsageJournal::new(root.join(JOURNAL_FILE_NAME));
        let manifest_store = ManifestStore::new(root.join(MANIFEST_FILE_NAME));
        let mut manifest = manifest_store.load()?;
        if manifest.format_version != MANIFEST_FORMAT_VERSION {
            return Err(StorageError::InvalidData(format!(
                "unsupported usage manifest version {}",
                manifest.format_version
            )));
        }
        let segments = load_manifest_segments(&root, &manifest)?;
        let snapshot = UsageSnapshot::new(manifest.clone(), segments)?;
        let mut memory = InMemoryUsageStore::new(snapshot);
        replay_manifest_corrections(&root, &manifest, &mut memory)?;
        replay_tail_and_corrections(&journal, &manifest, &mut memory)?;
        for correction in memory.pending_corrections() {
            manifest.add_pending_correction(correction.id.clone());
        }
        Ok(Self {
            root,
            journal,
            manifest_store,
            manifest,
            memory,
            duckdb_snapshot: DuckDbSnapshot,
        })
    }

    /// Return the dedicated store directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Borrow the current manifest metadata.
    pub fn manifest(&self) -> &UsageManifest {
        &self.manifest
    }

    /// Borrow the append-only journal handle.
    pub fn journal(&self) -> &UsageJournal {
        &self.journal
    }

    /// Append one observation and make it visible to this process immediately.
    pub fn append_record(&mut self, record: UsageRecord) -> Result<JournalPosition> {
        let position = self.journal.append_record(record.clone())?;
        self.memory.append(record);
        Ok(position)
    }

    /// Append one correction, persist its pending id, and make it visible to
    /// this process immediately.
    pub fn append_correction(&mut self, correction: UsageCorrection) -> Result<JournalPosition> {
        let position = self.journal.append_correction(correction.clone())?;
        self.memory.append_correction(correction.clone());
        self.manifest.add_pending_correction(correction.id);
        Ok(position)
    }

    /// Query records across the latest immutable snapshot and hot tail.
    pub fn query(&self, range: TimeRange, filter: &UsageFilter) -> Vec<UsageRecord> {
        if self.memory.tail().is_empty()
            && self.memory.pending_corrections().is_empty()
            && !self.manifest.segment_files.is_empty()
            && needs_duckdb_query(range, filter)
            && let Ok(records) = self.duckdb_snapshot.query(
                &self.root.join(SEGMENTS_DIRECTORY_NAME),
                &self.manifest.segment_files,
                range,
                filter,
            )
        {
            return records;
        }
        self.memory.query(range, filter)
    }

    /// Aggregate records across the latest immutable snapshot and hot tail,
    /// applying pending corrections.
    pub fn totals(&self, range: TimeRange, filter: &UsageFilter) -> UsageTotals {
        let immutable_only = self.memory.tail().is_empty()
            && self.memory.pending_corrections().is_empty()
            && !self.manifest.segment_files.is_empty();
        if immutable_only
            && needs_duckdb_query(range, filter)
            && let Ok(totals) = self.duckdb_snapshot.totals(
                &self.root.join(SEGMENTS_DIRECTORY_NAME),
                &self.manifest.segment_files,
                range,
                filter,
            )
        {
            return totals;
        }
        if range == TimeRange::all()
            && self.memory.tail().is_empty()
            && self.memory.pending_corrections().is_empty()
            && filter.model.is_none()
            && filter.thread_ids.is_none()
        {
            if let Some(account) = filter.account_id.as_deref()
                && filter.thread_id.is_none()
                && filter.repository_id.is_none()
            {
                return self
                    .manifest
                    .account_totals
                    .get(account)
                    .cloned()
                    .unwrap_or_default();
            }
            if let Some(thread) = filter.thread_id.as_deref()
                && filter.account_id.is_none()
                && filter.repository_id.is_none()
            {
                return self
                    .manifest
                    .thread_totals
                    .get(thread)
                    .cloned()
                    .unwrap_or_default();
            }
            if let Some(repository) = filter.repository_id.as_deref()
                && filter.account_id.is_none()
                && filter.thread_id.is_none()
            {
                return self
                    .manifest
                    .repository_totals
                    .get(repository)
                    .cloned()
                    .unwrap_or_default();
            }
            if *filter == UsageFilter::default() {
                return self.manifest.snapshot_totals.clone();
            }
        }
        self.memory.totals(range, filter)
    }

    /// Borrow the in-memory view for advanced callers.
    pub fn memory(&self) -> &InMemoryUsageStore {
        &self.memory
    }

    /// Borrow corrections that have not yet been folded into a snapshot.
    pub fn pending_corrections(&self) -> &[UsageCorrection] {
        self.memory.pending_corrections()
    }

    /// Materialize all currently visible records into one immutable segment per
    /// hour, then atomically publish a new manifest generation.
    ///
    /// Existing segment files are never overwritten. A crash before manifest
    /// replacement leaves unreferenced files that are ignored on the next open.
    pub fn materialize_snapshot(&mut self, freshness: Freshness) -> Result<UsageManifest> {
        // Only publishers share this lock. Journal writers and readers never
        // acquire it, so report publication cannot stall event capture.
        let publisher_lock = storage_file::open_read_write(&self.root.join(".snapshot.lock"))?;
        publisher_lock.lock()?;
        if self.manifest_store.load()?.snapshot_generation != self.manifest.snapshot_generation {
            *self = Self::open_at(&self.root)?;
        }
        // Reconcile entries written by another journal owner before taking the
        // snapshot boundary. Otherwise `last_sequence` could advance past
        // records that are absent from this process's in-memory tail, causing
        // a restart to skip those records permanently.
        let checkpoint = self.journal.checkpoint()?;
        let watermark = checkpoint.sequence;
        replay_journal_range(
            &self.journal,
            JournalCheckpoint {
                sequence: self.manifest.watermark,
                byte_offset: self.manifest.journal_byte_offset,
            },
            watermark,
            &mut self.memory,
        )?;

        // Materialization only rewrites hours touched by the hot tail. Older
        // immutable segments are retained by reference, so publication never
        // performs a foreground full-history scan.
        let mut tail_by_hour: BTreeMap<i64, Vec<UsageRecord>> = BTreeMap::new();
        for record in self.memory.tail().records().iter().cloned() {
            tail_by_hour
                .entry(crate::hour_start(record.observed_at))
                .or_default()
                .push(record);
        }

        let existing_segments = self
            .memory
            .snapshot()
            .segments()
            .iter()
            .cloned()
            .map(|segment| (segment.hour_start(), segment))
            .collect::<BTreeMap<_, _>>();
        let existing_files = self
            .manifest
            .segment_files
            .iter()
            .zip(self.memory.snapshot().segments())
            .map(|(file, segment)| (segment.hour_start(), file.clone()))
            .collect::<BTreeMap<_, _>>();
        let generation = self.manifest.snapshot_generation.saturating_add(1);
        let mut segments_by_hour = existing_segments;
        let mut segment_files_by_hour = existing_files;
        let mut touched_segments = BTreeMap::<i64, (Option<UsageSegment>, UsageSegment)>::new();
        for (hour, tail_records) in tail_by_hour {
            let mut records_by_id = BTreeMap::new();
            let previous = segments_by_hour.get(&hour).cloned();
            if let Some(existing) = &previous {
                for record in existing.records() {
                    records_by_id.insert(record.id.clone(), record.clone());
                }
            }
            for record in tail_records {
                records_by_id.insert(record.id.clone(), record);
            }
            let segment = UsageSegment::new(hour, records_by_id.into_values().collect())?;
            let file_name = segment_file_name(generation, hour);
            write_segment_file(&self.root, &file_name, &segment)?;
            touched_segments.insert(hour, (previous, segment.clone()));
            segments_by_hour.insert(hour, segment);
            segment_files_by_hour.insert(hour, file_name);
        }

        let mut segments = Vec::with_capacity(segments_by_hour.len());
        let mut segment_files = Vec::with_capacity(segments_by_hour.len());
        for (hour, segment) in segments_by_hour {
            segments.push(segment);
            segment_files.push(segment_files_by_hour.remove(&hour).ok_or_else(|| {
                StorageError::InvalidData("snapshot segment file missing".into())
            })?);
        }

        let mut manifest = self.manifest.clone();
        manifest.mark_snapshot(watermark, freshness);
        manifest.journal_byte_offset = checkpoint.byte_offset;
        // The compatibility materializer rebuilds summaries from raw records.
        // Its eager view does not maintain the publisher's membership sidecar.
        manifest.membership_file = None;
        manifest.membership_files.clear();
        manifest.review_projection_file = None;
        manifest.review_counts.clear();
        manifest.id_index_file = None;
        if manifest.summaries_include_corrections || !manifest.applied_correction_ids.is_empty() {
            manifest.snapshot_totals = UsageTotals::default();
            manifest.account_totals.clear();
            manifest.thread_totals.clear();
            manifest.repository_totals.clear();
        }
        manifest.summaries_include_corrections = false;
        manifest.applied_correction_ids.clear();
        let summaries_initialized = manifest.snapshot_totals.records != 0
            || !manifest.account_totals.is_empty()
            || !manifest.repository_totals.is_empty()
            || !manifest.thread_totals.is_empty();
        if !summaries_initialized {
            manifest.snapshot_totals = UsageTotals::default();
            manifest.account_totals.clear();
            manifest.repository_totals.clear();
            manifest.thread_totals.clear();
            for segment in &segments {
                apply_segment_summary_delta(&mut manifest, segment, true);
            }
        } else {
            for (_, (previous, current)) in touched_segments {
                if let Some(previous) = previous {
                    apply_segment_summary_delta(&mut manifest, &previous, false);
                }
                apply_segment_summary_delta(&mut manifest, &current, true);
            }
        }
        for correction in self.memory.pending_corrections() {
            manifest.add_pending_correction(correction.id.clone());
        }
        let mut correction_files = manifest.correction_files.clone();
        let mut corrections_by_hour = BTreeMap::<i64, Vec<UsageCorrection>>::new();
        for correction in self.memory.pending_corrections() {
            corrections_by_hour
                .entry(crate::hour_start(correction.observed_at))
                .or_default()
                .push(correction.clone());
        }
        for (hour, corrections) in corrections_by_hour {
            let correction_segment = UsageCorrectionSegment::new(hour, corrections)?;
            let file_name = correction_file_name(generation, hour);
            write_correction_file(&self.root, &file_name, &correction_segment)?;
            correction_files.push(file_name);
        }
        manifest.correction_files = correction_files;
        manifest.segment_files = segment_files;
        self.manifest_store.save(&manifest)?;
        self.manifest = manifest.clone();
        self.memory
            .install_snapshot(UsageSnapshot::new(manifest.clone(), segments)?);
        let _ = self.memory.take_tail();

        // Preserve records appended while the segments were being written as
        // the new hot tail. They remain above the published watermark and are
        // therefore also replayed after a restart.
        replay_journal_range(&self.journal, checkpoint, u64::MAX, &mut self.memory)?;
        Ok(manifest)
    }
}

fn needs_duckdb_query(range: TimeRange, filter: &UsageFilter) -> bool {
    range != TimeRange::all()
        || filter.thread_ids.is_some()
        || filter.model.is_some()
        || (filter.account_id.is_some() && filter.thread_id.is_some())
        || (filter.account_id.is_some() && filter.repository_id.is_some())
        || (filter.thread_id.is_some() && filter.repository_id.is_some())
}

fn load_manifest_segments(root: &Path, manifest: &UsageManifest) -> Result<Vec<UsageSegment>> {
    let codec = ParquetSegmentCodec;
    let segments_directory = root.join(SEGMENTS_DIRECTORY_NAME);
    let mut segments = Vec::with_capacity(manifest.segment_files.len());
    for file_name in &manifest.segment_files {
        let path = segments_directory.join(file_name);
        if path.file_name().and_then(|name| name.to_str()) != Some(file_name) {
            return Err(StorageError::InvalidData(format!(
                "manifest segment path is not a file name: {file_name}"
            )));
        }
        let bytes = fs::read(path)?;
        segments.push(codec.decode(&bytes)?);
    }
    Ok(segments)
}

fn apply_segment_summary_delta(manifest: &mut UsageManifest, segment: &UsageSegment, add: bool) {
    let segment_totals = segment.totals(TimeRange::all(), &UsageFilter::default());
    if add {
        manifest.snapshot_totals.add_assign(&segment_totals);
    } else {
        manifest.snapshot_totals.subtract_assign(&segment_totals);
    }
    for record in segment.records() {
        let totals = record.totals();
        apply_dimension_delta(
            &mut manifest.account_totals,
            record.account_id.as_deref(),
            &totals,
            add,
        );
        apply_dimension_delta(
            &mut manifest.repository_totals,
            record.repository_id.as_deref(),
            &totals,
            add,
        );
        apply_dimension_delta(
            &mut manifest.thread_totals,
            record.thread_id.as_deref(),
            &totals,
            add,
        );
    }
}

fn apply_dimension_delta(
    totals: &mut BTreeMap<String, UsageTotals>,
    key: Option<&str>,
    delta: &UsageTotals,
    add: bool,
) {
    let Some(key) = key else {
        return;
    };
    let entry = totals.entry(key.to_string()).or_default();
    if add {
        entry.add_assign(delta);
    } else {
        entry.subtract_assign(delta);
    }
}

fn replay_manifest_corrections(
    root: &Path,
    manifest: &UsageManifest,
    memory: &mut InMemoryUsageStore,
) -> Result<()> {
    let codec = ParquetCorrectionSegmentCodec;
    let segments_directory = root.join(SEGMENTS_DIRECTORY_NAME);
    for file_name in &manifest.correction_files {
        let path = segments_directory.join(file_name);
        if path.file_name().and_then(|name| name.to_str()) != Some(file_name) {
            return Err(StorageError::InvalidData(format!(
                "manifest correction path is not a file name: {file_name}"
            )));
        }
        let bytes = fs::read(path)?;
        let segment = codec.decode_corrections(&bytes)?;
        for correction in segment.corrections() {
            memory.append_correction(correction.clone());
        }
    }
    Ok(())
}

fn replay_tail_and_corrections(
    journal: &UsageJournal,
    manifest: &UsageManifest,
    memory: &mut InMemoryUsageStore,
) -> Result<()> {
    let (entries, _) = journal.read_from(JournalCheckpoint {
        sequence: manifest.watermark,
        byte_offset: manifest.journal_byte_offset,
    })?;
    for entry in entries {
        match entry.payload {
            JournalPayload::Record(record) => {
                memory.append(record);
            }
            JournalPayload::Correction(correction) => {
                memory.append_correction(correction);
            }
        }
    }
    Ok(())
}

fn replay_journal_range(
    journal: &UsageJournal,
    checkpoint: JournalCheckpoint,
    through_sequence: JournalSequence,
    memory: &mut InMemoryUsageStore,
) -> Result<()> {
    let (entries, _) = journal.read_from(checkpoint)?;
    for entry in entries {
        if entry.sequence > through_sequence {
            break;
        }
        match entry.payload {
            JournalPayload::Record(record) => memory.append(record),
            JournalPayload::Correction(correction) => memory.append_correction(correction),
        }
    }
    Ok(())
}

fn segment_file_name(generation: u64, hour: i64) -> String {
    format!("segment-{generation:020}-{hour}.parquet")
}

fn correction_file_name(generation: u64, hour: i64) -> String {
    format!("correction-{generation:020}-{hour}.parquet")
}

fn write_segment_file(root: &Path, file_name: &str, segment: &UsageSegment) -> Result<()> {
    let segments_directory = root.join(SEGMENTS_DIRECTORY_NAME);
    fs::create_dir_all(&segments_directory)?;
    let final_path = segments_directory.join(file_name);
    let temporary_path =
        segments_directory.join(format!(".{file_name}.tmp-{}", std::process::id()));
    let bytes = ParquetSegmentCodec.encode(segment)?;
    let mut file = storage_file::create_new(&temporary_path)?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = fs::rename(&temporary_path, final_path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(StorageError::Io(error));
    }
    #[cfg(unix)]
    fs::File::open(segments_directory)?.sync_all()?;
    Ok(())
}

fn write_correction_file(
    root: &Path,
    file_name: &str,
    segment: &UsageCorrectionSegment,
) -> Result<()> {
    let segments_directory = root.join(SEGMENTS_DIRECTORY_NAME);
    fs::create_dir_all(&segments_directory)?;
    let final_path = segments_directory.join(file_name);
    let temporary_path =
        segments_directory.join(format!(".{file_name}.tmp-{}", std::process::id()));
    let bytes = ParquetCorrectionSegmentCodec.encode_corrections(segment)?;
    let mut file = storage_file::create_new(&temporary_path)?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = fs::rename(&temporary_path, final_path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(StorageError::Io(error));
    }
    #[cfg(unix)]
    fs::File::open(segments_directory)?.sync_all()?;
    Ok(())
}
