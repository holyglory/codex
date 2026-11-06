use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use duckdb::Connection;

use crate::error::Result;
use crate::error::StorageError;
use crate::id_index::RecordIdEntry;
use crate::id_index::load_id_index;
use crate::id_index::lookup_ids;
use crate::id_index::publish_id_index;
use crate::journal::JournalCheckpoint;
use crate::journal::JournalPayload;
use crate::journal::JournalReadLimits;
use crate::journal::JournalReadStatus;
use crate::journal::UsageJournal;
use crate::lineage::publish_thread_lineage;
use crate::manifest::MANIFEST_FORMAT_VERSION;
use crate::manifest::ManifestStore;
use crate::manifest::UsageManifest;
use crate::model::Freshness;
use crate::model::UsageCorrection;
use crate::model::UsageRecord;
use crate::model::UsageTotals;
use crate::review_counts::read_review_counts_map;
use crate::review_projection::load_review_projection;
use crate::review_projection::publish_review_projection;
use crate::segment::ParquetCorrectionSegmentCodec;
use crate::segment::ParquetSegmentCodec;
use crate::segment::SegmentCodec;
use crate::segment::UsageCorrectionSegment;
use crate::segment::UsageSegment;
use crate::segment::hour_start;
use crate::storage_file;

/// Independent incremental publication; capture and readers never take its lock.
pub struct UsagePublisher {
    root: PathBuf,
    journal: UsageJournal,
    manifests: ManifestStore,
}

// Small fixture indexes remain cumulative for compatibility. Large stores use
// immutable fragments so publication never rewrites historical membership rows.
const CUMULATIVE_MEMBERSHIP_SEGMENT_LIMIT: usize = 4;

impl UsagePublisher {
    /// Validate published metadata without decoding historical operation rows.
    pub fn open_at(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let manifests = ManifestStore::new(root.join("manifest.json"));
        validate_manifest(&manifests.load()?)?;
        Ok(Self {
            journal: UsageJournal::new(root.join("events.jsonl")),
            root,
            manifests,
        })
    }

    /// Publish one bounded journal prefix, retaining every previous immutable file.
    /// A failed attempt leaves its files unreferenced and its journal entries intact.
    pub fn publish_batch(&mut self, limits: JournalReadLimits) -> Result<UsageManifest> {
        let directory = self.root.join("segments");
        storage_file::ensure_usage_directory(&self.root)?;
        let lock = storage_file::open_read_write(&self.root.join(".snapshot.lock"))?;
        lock.lock()?;
        // Reload after locking so independently opened publishers cannot regress a generation.
        let mut manifest = self.manifests.load()?;
        validate_manifest(&manifest)?;
        let target = self.journal.checkpoint()?;
        let batch = self.journal.read_batch(
            JournalCheckpoint {
                sequence: manifest.watermark,
                byte_offset: manifest.journal_byte_offset,
            },
            target,
            limits,
        )?;
        if let JournalReadStatus::EntryExceedsByteLimit { minimum_bytes } = batch.status {
            return Err(StorageError::InvalidData(format!(
                "publisher byte limit must be at least {minimum_bytes}"
            )));
        }
        if batch.entries.is_empty()
            && manifest.pending_corrections.is_empty()
            && manifest.summaries_include_corrections
        {
            return Ok(manifest);
        }
        let generation = manifest
            .snapshot_generation
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("snapshot generation exhausted".into()))?;
        let stamp = format!(
            "{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let connection = Connection::open_in_memory().map_err(query_error)?;
        let index_complete = manifest.membership_file.is_some()
            || !manifest.membership_files.is_empty()
            || manifest.segment_files.is_empty();
        let mut records = BTreeMap::new();
        let mut corrections = BTreeMap::new();
        for entry in batch.entries {
            match entry.payload {
                JournalPayload::Record(record) => {
                    records
                        .entry(record.id.clone())
                        .or_insert((entry.sequence, record));
                }
                JournalPayload::Correction(correction) => {
                    corrections
                        .entry(correction.id.clone())
                        .or_insert(correction);
                }
            }
        }
        let requested_ids = records.keys().cloned().collect::<BTreeSet<_>>();
        let existing = if let Some(file) = manifest.id_index_file.as_deref() {
            let descriptor = load_id_index(
                &self.root,
                Some(file),
                JournalCheckpoint {
                    sequence: manifest.watermark,
                    byte_offset: manifest.journal_byte_offset,
                },
            )?;
            let lookup = lookup_ids(&self.root, &descriptor, &requested_ids)?;
            if lookup.complete_from_sequence <= manifest.watermark.saturating_add(1) {
                lookup
                    .found
                    .into_iter()
                    .map(|(id, entry)| {
                        (
                            id,
                            Identity {
                                account: entry.account_id,
                                thread: entry.thread_id,
                                repository: entry.repository_id,
                            },
                        )
                    })
                    .collect()
            } else {
                find_records(
                    &connection,
                    &directory,
                    &manifest.segment_files,
                    requested_ids.iter().cloned().collect(),
                )?
            }
        } else {
            find_records(
                &connection,
                &directory,
                &manifest.segment_files,
                requested_ids.iter().cloned().collect(),
            )?
        };
        records.retain(|id, _| !existing.contains_key(id));
        let records = records.into_values().collect::<Vec<_>>();
        let index_candidates = records.clone();
        let refresh_review_counts = records.len() > 16_384;
        manifest.review_projection_file = Some(publish_review_projection(
            &self.root,
            &manifest,
            batch.next,
            &format!("{generation:020}_{stamp}"),
            &records,
            &connection,
        )?);
        let mut touched = BTreeMap::<i64, Vec<UsageRecord>>::new();
        let mut lineage = Vec::new();
        for (_, record) in records {
            if let Some(parent) = &record.parent_thread_id {
                let child = record.thread_id.clone().ok_or_else(|| {
                    StorageError::InvalidData("parent fact has no child thread id".into())
                })?;
                lineage.push((child, parent.clone()));
            }
            if [
                record.input_tokens,
                record.cached_input_tokens,
                record.output_tokens,
                record.reasoning_output_tokens,
            ]
            .iter()
            .any(|value| *value > i64::MAX as u64)
            {
                return Err(StorageError::InvalidData(
                    "observation exceeds signed summary range".into(),
                ));
            }
            apply_delta(&mut manifest, &Identity::from(&record), &record.totals())?;
            touched
                .entry(hour_start(record.observed_at))
                .or_default()
                .push(record);
        }
        manifest.lineage_file = publish_thread_lineage(
            &self.root,
            manifest.lineage_file.as_deref(),
            &format!("lineage-{generation:020}_{stamp}.parquet"),
            &lineage,
        )?;
        let mut membership = BTreeSet::new();
        let mut segment_by_hour = BTreeMap::new();
        let membership_changed = !touched.is_empty();
        for (hour, additions) in touched {
            // A fragment contains only this bounded batch; prior hours are never decoded.
            let segment = UsageSegment::new(hour, additions)?;
            let file = format!("segment-{generation:020}_{stamp}-{hour}.parquet");
            write_immutable(&directory, &file, &ParquetSegmentCodec.encode(&segment)?)?;
            if index_complete {
                for record in segment.records() {
                    for (kind, id) in [
                        ("account", &record.account_id),
                        ("thread", &record.thread_id),
                        ("repository", &record.repository_id),
                    ] {
                        if let Some(id) = id {
                            membership.insert((kind, id.clone(), hour, file.clone()));
                        }
                    }
                }
            }
            manifest.segment_files.push(file.clone());
            segment_by_hour.insert(hour, file);
        }
        if !index_candidates.is_empty() {
            let previous = manifest
                .id_index_file
                .as_deref()
                .map(|file| {
                    load_id_index(
                        &self.root,
                        Some(file),
                        JournalCheckpoint {
                            sequence: manifest.watermark,
                            byte_offset: manifest.journal_byte_offset,
                        },
                    )
                })
                .transpose()?;
            let entries = index_candidates
                .iter()
                .filter_map(|(sequence, record)| {
                    segment_by_hour
                        .get(&hour_start(record.observed_at))
                        .map(|segment_file| RecordIdEntry {
                            id: record.id.clone(),
                            first_sequence: *sequence,
                            account_id: record.account_id.clone(),
                            thread_id: record.thread_id.clone(),
                            repository_id: record.repository_id.clone(),
                            segment_file: segment_file.clone(),
                        })
                })
                .collect::<Vec<_>>();
            let write = publish_id_index(
                &self.root,
                previous.as_ref(),
                batch.next,
                &format!("{generation:020}_{stamp}"),
                &entries,
            )?;
            manifest.id_index_file = Some(write.file);
        }

        let seen = find_corrections(
            &connection,
            &directory,
            &manifest.correction_files,
            corrections.keys().cloned().collect(),
        )?;
        let seen: BTreeSet<_> = seen.into_iter().map(|correction| correction.id).collect();
        let mut correction_hours = BTreeMap::<i64, Vec<UsageCorrection>>::new();
        for correction in corrections.into_values() {
            if seen.contains(&correction.id)
                || manifest.applied_correction_ids.contains(&correction.id)
                || manifest.pending_corrections.contains(&correction.id)
            {
                continue;
            }
            manifest.add_pending_correction(correction.id.clone());
            correction_hours
                .entry(hour_start(correction.observed_at))
                .or_default()
                .push(correction);
        }
        for (hour, rows) in correction_hours {
            let segment = UsageCorrectionSegment::new(hour, rows)?;
            let file = format!("correction-{generation:020}_{stamp}-{hour}.parquet");
            write_immutable(
                &directory,
                &file,
                &ParquetCorrectionSegmentCodec.encode_corrections(&segment)?,
            )?;
            manifest.correction_files.push(file);
        }
        let count = limits.max_entries.min(manifest.pending_corrections.len());
        let pending: Vec<_> = manifest.pending_corrections.drain(..count).collect();
        let corrections = find_corrections(
            &connection,
            &directory,
            &manifest.correction_files,
            pending.clone(),
        )?;
        let identities = find_records(
            &connection,
            &directory,
            &manifest.segment_files,
            corrections
                .iter()
                .map(|correction| correction.record_id.clone())
                .collect(),
        )?;
        let mut corrections: BTreeMap<_, _> = corrections
            .into_iter()
            .map(|correction| (correction.id.clone(), correction))
            .collect();
        for id in pending {
            if manifest.applied_correction_ids.contains(&id) {
                continue;
            }
            let correction = corrections.remove(&id).ok_or_else(|| {
                StorageError::InvalidData("pending correction sidecar missing".into())
            })?;
            if let Some(identity) = identities.get(&correction.record_id) {
                apply_delta(&mut manifest, identity, &correction.totals())?;
                manifest.applied_correction_ids.insert(id);
            } else {
                // Rotate unresolved targets so they cannot starve later resolvable IDs.
                manifest.pending_corrections.push(id);
            }
        }
        manifest.summaries_include_corrections = manifest.pending_corrections.is_empty();
        if index_complete && membership_changed {
            let file = format!("membership-{generation:020}_{stamp}.parquet");
            let cumulative = manifest.segment_files.len() < CUMULATIVE_MEMBERSHIP_SEGMENT_LIMIT;
            write_membership(
                &connection,
                &directory,
                cumulative
                    .then_some(manifest.membership_file.as_deref())
                    .flatten(),
                &file,
                membership,
            )?;
            if cumulative {
                manifest.membership_file = Some(file.clone());
                manifest.membership_files = vec![file];
            } else {
                if manifest.membership_files.is_empty()
                    && let Some(previous) = &manifest.membership_file
                {
                    manifest.membership_files.push(previous.clone());
                }
                manifest.membership_files.push(file);
            }
        }
        manifest.mark_snapshot(
            batch.next.sequence,
            if batch.next == target && manifest.pending_corrections.is_empty() {
                Freshness::Fresh
            } else {
                Freshness::Stale
            },
        );
        manifest.journal_byte_offset = batch.next.byte_offset;
        // Read the freshly published count sidecar only after the manifest
        // frontier has advanced to the same checkpoint. Small recovery tails
        // are overlaid directly by readers to keep startup bounded.
        if refresh_review_counts {
            let descriptor = load_review_projection(&self.root, &manifest)?.ok_or_else(|| {
                StorageError::InvalidData("published review descriptor missing".into())
            })?;
            manifest.review_counts = read_review_counts_map(&self.root, &descriptor)?;
        }
        #[cfg(unix)]
        std::fs::File::open(&directory)?.sync_all()?;
        self.manifests.save(&manifest)?;
        Ok(manifest)
    }
}

#[derive(Clone)]
struct Identity {
    account: Option<String>,
    thread: Option<String>,
    repository: Option<String>,
}

impl From<&UsageRecord> for Identity {
    fn from(record: &UsageRecord) -> Self {
        Self {
            account: record.account_id.clone(),
            thread: record.thread_id.clone(),
            repository: record.repository_id.clone(),
        }
    }
}

fn apply_delta(
    manifest: &mut UsageManifest,
    identity: &Identity,
    delta: &UsageTotals,
) -> Result<()> {
    add_totals(&mut manifest.snapshot_totals, delta)?;
    for (totals, id) in [
        (&mut manifest.account_totals, &identity.account),
        (&mut manifest.thread_totals, &identity.thread),
        (&mut manifest.repository_totals, &identity.repository),
    ] {
        if let Some(id) = id {
            add_totals(totals.entry(id.clone()).or_default(), delta)?;
        }
    }
    Ok(())
}

fn add_totals(left: &mut UsageTotals, right: &UsageTotals) -> Result<()> {
    let add = |a: i64, b: i64| {
        a.checked_add(b)
            .ok_or_else(|| StorageError::InvalidData("summary token count overflow".into()))
    };
    left.input_tokens = add(left.input_tokens, right.input_tokens)?;
    left.cached_input_tokens = add(left.cached_input_tokens, right.cached_input_tokens)?;
    left.output_tokens = add(left.output_tokens, right.output_tokens)?;
    left.reasoning_output_tokens =
        add(left.reasoning_output_tokens, right.reasoning_output_tokens)?;
    left.records = left
        .records
        .checked_add(right.records)
        .ok_or_else(|| StorageError::InvalidData("summary record count overflow".into()))?;
    Ok(())
}

fn find_records(
    connection: &Connection,
    directory: &Path,
    files: &[String],
    ids: Vec<String>,
) -> Result<BTreeMap<String, Identity>> {
    if files.is_empty() || ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let query = format!(
        "SELECT id, account_id, thread_id, repository_id FROM read_parquet([{}], filename=true) WHERE id IN ({}) QUALIFY row_number() OVER (PARTITION BY id ORDER BY filename) = 1",
        paths(directory, files)?,
        literals(&ids)
    );
    let mut statement = connection.prepare(&query).map_err(query_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                Identity {
                    account: row.get(1)?,
                    thread: row.get(2)?,
                    repository: row.get(3)?,
                },
            ))
        })
        .map_err(query_error)?;
    rows.collect::<std::result::Result<BTreeMap<_, _>, _>>()
        .map_err(query_error)
}

fn find_corrections(
    connection: &Connection,
    directory: &Path,
    files: &[String],
    ids: Vec<String>,
) -> Result<Vec<UsageCorrection>> {
    if files.is_empty() || ids.is_empty() {
        return Ok(Vec::new());
    }
    let query = format!(
        "SELECT id, record_id, observed_at, input_tokens_delta, cached_input_tokens_delta, output_tokens_delta, reasoning_output_tokens_delta, reason FROM read_parquet([{}], filename=true) WHERE id IN ({}) QUALIFY row_number() OVER (PARTITION BY id ORDER BY filename) = 1",
        paths(directory, files)?,
        literals(&ids)
    );
    let mut statement = connection.prepare(&query).map_err(query_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok(UsageCorrection {
                id: row.get(0)?,
                record_id: row.get(1)?,
                observed_at: row.get(2)?,
                input_tokens_delta: row.get(3)?,
                cached_input_tokens_delta: row.get(4)?,
                output_tokens_delta: row.get(5)?,
                reasoning_output_tokens_delta: row.get(6)?,
                reason: row.get(7)?,
            })
        })
        .map_err(query_error)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(query_error)
}

fn write_membership(
    connection: &Connection,
    directory: &Path,
    previous: Option<&str>,
    file: &str,
    membership: BTreeSet<(&str, String, i64, String)>,
) -> Result<()> {
    let values = membership
        .into_iter()
        .map(|(kind, id, hour, segment)| {
            format!(
                "({}, {}, {hour}, {})",
                quoted(kind),
                quoted(&id),
                quoted(&segment)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let additions = if values.is_empty() {
        "SELECT ''::VARCHAR dimension_kind, ''::VARCHAR dimension_id, 0::BIGINT hour_start, ''::VARCHAR segment_file WHERE false".to_string()
    } else {
        format!(
            "SELECT dimension_kind, dimension_id, hour_start::BIGINT hour_start, segment_file FROM (VALUES {values}) AS m(dimension_kind, dimension_id, hour_start, segment_file)"
        )
    };
    let source = if let Some(previous) = previous {
        storage_file::protect_existing(&directory.join(previous))?;
        format!(
            "SELECT * FROM read_parquet([{}]) UNION {additions}",
            paths(directory, &[previous.to_string()])?
        )
    } else {
        additions
    };
    let query = format!(
        "COPY (SELECT * FROM ({source}) ORDER BY dimension_kind, dimension_id, hour_start, segment_file) TO {} (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 2048)",
        quoted(&directory.join(file).to_string_lossy())
    );
    connection.execute_batch(&query).map_err(query_error)?;
    storage_file::finish_generated(&directory.join(file))?;
    Ok(())
}

fn paths(directory: &Path, files: &[String]) -> Result<String> {
    files
        .iter()
        .map(|file| {
            if Path::new(file).file_name().and_then(|name| name.to_str()) != Some(file) {
                return Err(StorageError::InvalidData(
                    "snapshot path must be a file name".into(),
                ));
            }
            Ok(quoted(&directory.join(file).to_string_lossy()))
        })
        .collect::<Result<Vec<_>>>()
        .map(|paths| paths.join(", "))
}

fn literals(values: &[String]) -> String {
    values
        .iter()
        .map(|value| quoted(value))
        .collect::<Vec<_>>()
        .join(", ")
}

fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn write_immutable(directory: &Path, file: &str, bytes: &[u8]) -> Result<()> {
    let mut output = storage_file::create_new(&directory.join(file))?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

fn validate_manifest(manifest: &UsageManifest) -> Result<()> {
    if manifest.format_version != MANIFEST_FORMAT_VERSION {
        return Err(StorageError::InvalidData(format!(
            "unsupported usage manifest version {}",
            manifest.format_version
        )));
    }
    Ok(())
}

fn query_error(error: duckdb::Error) -> StorageError {
    StorageError::Columnar(format!("DuckDB publisher query failed: {error}"))
}

#[cfg(test)]
#[path = "publisher_tests.rs"]
mod tests;
