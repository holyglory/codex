//! A small published token snapshot, independent of operation/review metadata.
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::Freshness;
use crate::JournalCheckpoint;
use crate::Result;
use crate::StorageError;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageJournal;
use crate::review_storage::query_error;
use crate::review_storage::quoted;
use crate::storage_file;

pub(crate) const METRICS: &[&str] = &[
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
    "token_observations",
    "known_provider_total_tokens",
    "provider_total_unknowns",
    "known_cache_write_input_tokens",
    "cache_write_unknowns",
    "missing_observations",
];
pub(crate) const DESCRIPTOR_VERSION: u32 = 2;

/// Exact source prefix of an immutable, published-only token projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageHeadlineSnapshot {
    pub snapshot_id: String,
    pub source_snapshot_generation: u64,
    pub source_watermark: JournalCheckpoint,
    pub source_watermark_at_ms: i64,
    pub source_freshness: Freshness,
    pub published_at_ms: i64,
    pub pending_corrections: u64,
    pub retired_observations: u64,
    pub raw_admission_cutoff_at_ms: Option<i64>,
    pub unapplied_retention_records: u64,
    pub unapplied_retention_corrections: u64,
    /// Observed at reader admission; newer entries are not overlaid on the index.
    pub latest_journal_watermark: u64,
    pub pending_entries: u64,
    pub freshness: Freshness,
}

/// Observed component sums and explicit provider coverage. Components may overlap.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageHeadlineTotals {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub token_observations: u64,
    pub known_provider_total_tokens: u64,
    pub provider_total_unknowns: u64,
    pub known_cache_write_input_tokens: u64,
    pub cache_write_unknowns: u64,
    pub missing_observations: u64,
    /// None when any included token observation lacks this provider value.
    pub provider_total_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Descriptor {
    pub version: u32,
    pub data_file: String,
    pub source_manifest_identity: String,
    pub snapshot: UsageHeadlineSnapshot,
}

pub(crate) fn descriptor(root: &Path) -> Result<Option<Descriptor>> {
    let file = match File::open(root.join("headline.json")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(65_537).read_to_end(&mut bytes)?;
    if bytes.len() > 65_536 {
        return Err(StorageError::InvalidData(
            "oversized token headline descriptor".into(),
        ));
    }
    let descriptor: Descriptor = serde_json::from_slice(&bytes)?;
    if descriptor.version != DESCRIPTOR_VERSION || !descriptor.data_file.starts_with("headline-") {
        return Err(StorageError::InvalidData(
            "invalid token headline descriptor".into(),
        ));
    }
    data_path(root, &descriptor.data_file)?;
    Ok(Some(descriptor))
}

/// Pins only the small headline descriptor and its immutable token projection.
/// Each query owns its bounded connection, so this reader can be shared safely.
#[derive(Debug)]
pub struct UsageHeadlineReader {
    _pin: File,
    root: PathBuf,
    descriptor: Descriptor,
}

impl UsageHeadlineReader {
    /// Missing publication is explicit; opening never starts a build or rich read.
    pub fn open_at(root: impl Into<PathBuf>) -> Result<Option<Self>> {
        let root = root.into();
        let admission = storage_file::open_read_write(&root.join(".headline-readers.lock"))?;
        admission.lock_shared()?;
        let Some(mut descriptor) = descriptor(&root)? else {
            return Ok(None);
        };
        let pin = storage_file::open_read_write(
            &data_path(&root, &descriptor.data_file)?.with_extension("pin"),
        )?;
        pin.lock_shared()?;
        drop(admission);
        let latest = UsageJournal::new(root.join("events.jsonl")).checkpoint()?;
        let snapshot = &mut descriptor.snapshot;
        snapshot.latest_journal_watermark = latest.sequence;
        snapshot.pending_entries = latest
            .sequence
            .checked_sub(snapshot.source_watermark.sequence)
            .ok_or_else(|| {
                StorageError::InvalidData("headline source exceeds journal frontier".into())
            })?;
        snapshot.freshness = if snapshot.pending_entries != 0 || snapshot.pending_corrections != 0 {
            Freshness::Stale
        } else {
            snapshot.source_freshness
        };
        Ok(Some(Self {
            _pin: pin,
            root,
            descriptor,
        }))
    }

    pub fn snapshot(&self) -> &UsageHeadlineSnapshot {
        &self.descriptor.snapshot
    }

    /// None means a requested millisecond boundary bisects retired aggregate data.
    /// Such a request cannot receive an exact total from the retained source.
    pub fn totals(
        &self,
        range: TimeRange,
        filter: &UsageFilter,
    ) -> Result<Option<UsageHeadlineTotals>> {
        if range.start_ms() == range.end_ms() {
            return Ok(Some(UsageHeadlineTotals {
                provider_total_tokens: Some(0),
                cache_write_input_tokens: Some(0),
                ..UsageHeadlineTotals::default()
            }));
        }
        let connection = crate::duckdb_connection::open_read_in_memory(
            crate::duckdb_connection::ReadBudget::TokenHeadline,
        )
        .map_err(query_error)?;
        let path = data_path(&self.root, &self.descriptor.data_file)?;
        let mut predicates = Vec::new();
        for (column, value) in [
            ("account_id", &filter.account_id),
            ("thread_id", &filter.thread_id),
            ("repository_id", &filter.repository_id),
            ("model", &filter.model),
        ] {
            if let Some(value) = value {
                predicates.push(format!("{column} = {}", quoted(value)));
            }
        }
        if let Some(threads) = &filter.thread_ids {
            predicates.push(if threads.is_empty() {
                "false".into()
            } else {
                format!(
                    "thread_id IN ({})",
                    threads
                        .iter()
                        .map(|id| quoted(id))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            });
        }
        predicates.push(format!(
            "last_at_ms >= {} AND first_at_ms < {}",
            range.start_ms(),
            range.end_ms()
        ));
        let sums = METRICS
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let kind = if index < 4 { "BIGINT" } else { "UBIGINT" };
                format!("COALESCE(sum({name}::HUGEINT),0)::{kind}")
            })
            .collect::<Vec<_>>()
            .join(",");
        let query = format!(
            "SELECT {sums}, COALESCE(bool_or(first_at_ms < {} OR last_at_ms >= {}),false) \
             FROM read_parquet({}) WHERE {}",
            range.start_ms(),
            range.end_ms(),
            quoted(&path.to_string_lossy()),
            predicates.join(" AND ")
        );
        connection
            .query_row(&query, [], |row| {
                if row.get::<_, bool>(10)? {
                    return Ok(None);
                }
                let mut totals = UsageHeadlineTotals {
                    input_tokens: row.get(0)?,
                    cached_input_tokens: row.get(1)?,
                    output_tokens: row.get(2)?,
                    reasoning_output_tokens: row.get(3)?,
                    token_observations: row.get(4)?,
                    known_provider_total_tokens: row.get(5)?,
                    provider_total_unknowns: row.get(6)?,
                    known_cache_write_input_tokens: row.get(7)?,
                    cache_write_unknowns: row.get(8)?,
                    missing_observations: row.get(9)?,
                    ..UsageHeadlineTotals::default()
                };
                totals.provider_total_tokens = (totals.provider_total_unknowns == 0)
                    .then_some(totals.known_provider_total_tokens);
                totals.cache_write_input_tokens = (totals.cache_write_unknowns == 0)
                    .then_some(totals.known_cache_write_input_tokens);
                Ok(Some(totals))
            })
            .map_err(query_error)
    }
}

pub(crate) fn data_path(root: &Path, file: &str) -> Result<PathBuf> {
    if Path::new(file).file_name().and_then(|name| name.to_str()) != Some(file)
        || !file.starts_with("headline-")
        || !file.ends_with(".parquet")
    {
        return Err(StorageError::InvalidData(
            "invalid token headline data file".into(),
        ));
    }
    Ok(root.join("headline-segments").join(file))
}

#[cfg(test)]
#[path = "headline_tests.rs"]
mod tests;
