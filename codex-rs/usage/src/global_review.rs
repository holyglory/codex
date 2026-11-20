//! Completed factual global analytics, independent of foreground history reads.
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::Freshness;
use crate::Result;
use crate::StorageError;
use crate::UsageJournal;
use crate::storage_file;

pub(crate) const MAX_PUBLICATION_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const PUBLICATION: &str = "global-review.json";

/// Source identity of a completed all-time report with no identity filters.
/// Latest-journal fields describe backlog only; they never advance report facts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalReviewSnapshot {
    pub snapshot_id: String,
    pub source_snapshot_generation: u64,
    pub source_watermark: u64,
    pub included_journal_watermark: u64,
    pub source_watermark_at_ms: i64,
    pub source_updated_at_ms: i64,
    pub source_freshness: Freshness,
    pub published_at_ms: i64,
    pub pending_corrections: u64,
    pub retired_observations: u64,
    pub retired_review_operations: u64,
    pub retired_review_contributions: u64,
    pub unapplied_retention_records: u64,
    pub unapplied_retention_corrections: u64,
    pub latest_journal_watermark: u64,
    pub pending_entries: u64,
    pub freshness: Freshness,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Publication {
    pub version: u32,
    pub source_manifest_identity: String,
    pub snapshot: GlobalReviewSnapshot,
    pub report: Value,
}

pub(crate) fn load(root: &Path) -> Result<Option<Publication>> {
    let path = root.join(PUBLICATION);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    storage_file::protect_existing(&path)?;
    let mut bytes = Vec::new();
    file.take(MAX_PUBLICATION_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PUBLICATION_BYTES {
        return Err(StorageError::InvalidData(
            "oversized global analytics publication".into(),
        ));
    }
    let publication: Publication = serde_json::from_slice(&bytes)?;
    let report = &publication.report;
    let rows = report["outcomes"]["rows"].as_array();
    let complete_shape = report["counts"]["operations"].as_u64().is_some()
        && report["tokens"].is_array()
        && [
            "timing",
            "cost",
            "contextSize",
            "coverage",
            "window",
            "evidence",
            "outcomes",
        ]
        .iter()
        .all(|field| report[*field].is_object())
        && report["outcomes"]["totals"].is_object()
        && rows.is_some_and(|rows| {
            report["outcomes"]["totalRows"].as_u64() == Some(rows.len() as u64)
        });
    if publication.version != 1
        || !complete_shape
        || publication.snapshot.source_watermark > publication.snapshot.included_journal_watermark
        || publication.report["schemaVersion"] != 1
        || publication.report["kind"] != "performanceReview"
        || publication.report["window"]["fromAtMs"] != Value::Null
        || publication.report["window"]["toAtMs"] != Value::Null
        || publication.report["evidence"]["repository"] != Value::Null
        || publication.report["evidence"]["thread_id"] != Value::Null
    {
        return Err(StorageError::InvalidData(
            "invalid global analytics publication".into(),
        ));
    }
    Ok(Some(publication))
}

/// Owns one bounded JSON object. It holds no canonical-file pin or query engine.
#[derive(Debug)]
pub struct GlobalReviewReader {
    publication: Publication,
}

impl GlobalReviewReader {
    /// Never starts publication, opens the large manifest, or scans history.
    pub fn open_at(root: impl Into<PathBuf>) -> Result<Option<Self>> {
        let root = root.into();
        let Some(mut publication) = load(&root)? else {
            return Ok(None);
        };
        let latest = UsageJournal::new(root.join("events.jsonl")).checkpoint()?;
        let snapshot = &mut publication.snapshot;
        snapshot.latest_journal_watermark = latest.sequence;
        snapshot.pending_entries = latest
            .sequence
            .checked_sub(snapshot.included_journal_watermark)
            .ok_or_else(|| {
                StorageError::InvalidData("global analytics exceeds journal frontier".into())
            })?;
        snapshot.freshness = if snapshot.pending_entries != 0 || snapshot.pending_corrections != 0 {
            Freshness::Stale
        } else {
            snapshot.source_freshness
        };
        Ok(Some(Self { publication }))
    }

    pub fn snapshot(&self) -> &GlobalReviewSnapshot {
        &self.publication.snapshot
    }

    /// The unchanged native report calculated at this snapshot's included prefix.
    pub fn report(&self) -> &Value {
        &self.publication.report
    }
}
