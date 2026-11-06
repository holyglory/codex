use std::path::PathBuf;

use crate::Freshness;
use crate::JournalCheckpoint;
use crate::JournalPayload;
use crate::ManifestStore;
use crate::Result;
use crate::StorageError;
use crate::TimeRange;
use crate::USAGE_DIRECTORY_NAME;
use crate::UsageCorrection;
use crate::UsageFilter;
use crate::UsageJournal;
use crate::UsageManifest;
use crate::UsageRecord;
use crate::UsageTotals;
use crate::journal::JournalReadLimits;
use crate::journal::JournalTailRead;
use crate::reader_query::SnapshotQuery;

/// Maximum journal work a foreground reader may perform. A larger backlog is
/// left to the publisher; the last consistent snapshot remains available.
const REPORT_TAIL_LIMITS: JournalReadLimits = JournalReadLimits {
    max_bytes: 8 * 1024 * 1024,
    max_entries: 4096,
};

/// An immutable report view pinned to one published generation and journal end.
/// Opening never decodes historical Parquet rows or acquires the publisher lock.
#[derive(Debug)]
pub struct UsageReader {
    pub(crate) root: PathBuf,
    pub(crate) manifest: std::sync::Arc<UsageManifest>,
    pub(crate) metadata_revision: u64,
    pub(crate) records: Vec<UsageRecord>,
    pub(crate) record_sequences: std::collections::BTreeMap<String, u64>,
    pub(crate) corrections: Vec<UsageCorrection>,
    pending_entries: u64,
    freshness: Freshness,
    journal_watermark: u64,
}

/// Optional row restriction applied inside DuckDB before paging.
#[derive(Clone, Copy, Debug, Default)]
pub enum RecordKind {
    /// All recorded observations.
    #[default]
    All,
    /// Observations associated with a tool.
    Tool,
    /// Observations with an activity classification.
    Activity,
    /// Provider observations, without counting lifecycle rows as tokens.
    Tokens,
    /// Thread creation and terminal facts.
    Threads,
    /// Turn creation and terminal facts.
    Turns,
    /// Append-only declarations and classification corrections.
    Classifications,
    /// Explicit gaps in observation coverage.
    Coverage,
    /// Process/thread/turn/activity lifecycle facts.
    Lifecycle,
    /// Explicit repository identity or attribution observations.
    Repositories,
    /// Tool approval observations when an approval producer is connected.
    Approvals,
    /// Process lifecycle observations without operating-system identifiers.
    Processes,
    /// Recorded agent lifecycle observations.
    Agents,
    /// Versioned taxonomy observations.
    Taxonomies,
}

/// Stable keyset position in a record query, preserving exact UTC milliseconds.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordCursor {
    /// Millisecond timestamp of the last returned observation.
    pub observed_at_ms: i64,
    /// Stable ID of the last returned observation.
    pub id: String,
}

impl RecordCursor {
    /// Construct a continuation from an exact millisecond timestamp.
    pub fn from_millis(observed_at_ms: i64, id: impl Into<String>) -> Self {
        Self {
            observed_at_ms,
            id: id.into(),
        }
    }
}

/// A bounded page. The cursor identifies the last returned row, never a
/// lookahead row; following it therefore cannot skip an observation.
#[derive(Clone, Debug)]
pub struct RecordPage {
    /// At most the requested number of records.
    pub records: Vec<UsageRecord>,
    /// Continuation when another matching record exists.
    pub next: Option<RecordCursor>,
}

/// Aggregate and observed interval for one repository identity.
#[derive(Clone, Debug)]
pub struct RepositoryUsage {
    /// Local repository identity, without a raw path or remote.
    pub id: String,
    /// Earliest matching observation in Unix seconds.
    pub first_observed_at: i64,
    /// Exact earliest matching observation in UTC milliseconds.
    pub first_observed_at_ms: i64,
    /// Latest matching observation in Unix seconds.
    pub last_observed_at: i64,
    /// Exact latest matching observation in UTC milliseconds.
    pub last_observed_at_ms: i64,
    /// Mergeable token and observation totals.
    pub totals: UsageTotals,
}

impl UsageReader {
    /// Open the new analytics store without touching retired usage databases.
    pub fn open(codex_home: impl Into<PathBuf>) -> Result<Self> {
        Self::open_at(codex_home.into().join(USAGE_DIRECTORY_NAME))
    }

    /// Pin published metadata and a bounded, complete journal tail.
    pub fn open_at(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let parsed = ManifestStore::new(root.join("manifest.json")).load_shared()?;
        let metadata_revision = parsed.revision;
        let manifest = parsed.value;
        if manifest.format_version != crate::MANIFEST_FORMAT_VERSION {
            return Err(StorageError::InvalidData(format!(
                "unsupported usage manifest version {}",
                manifest.format_version
            )));
        }
        let after = JournalCheckpoint {
            sequence: manifest.watermark,
            byte_offset: manifest.journal_byte_offset,
        };
        let tail =
            UsageJournal::new(root.join("events.jsonl")).read_tail(after, REPORT_TAIL_LIMITS)?;
        let mut records = Vec::new();
        let mut record_sequences = std::collections::BTreeMap::new();
        let mut corrections = Vec::new();
        let (pending_entries, freshness, journal_watermark) = match tail {
            JournalTailRead::Complete(batch) => {
                for entry in batch.entries {
                    match entry.payload {
                        JournalPayload::Record(record) => {
                            if !record_sequences.contains_key(&record.id) {
                                record_sequences.insert(record.id.clone(), entry.sequence);
                                records.push(record);
                            }
                        }
                        JournalPayload::Correction(correction) => corrections.push(correction),
                    }
                }
                let freshness = if manifest.pending_corrections.is_empty() {
                    Freshness::Fresh
                } else {
                    Freshness::Stale
                };
                (0, freshness, batch.target.sequence)
            }
            JournalTailRead::Deferred {
                pending_entries, ..
            } => (pending_entries, Freshness::Stale, manifest.watermark),
        };
        Ok(Self {
            root,
            manifest,
            metadata_revision,
            records,
            record_sequences,
            corrections,
            pending_entries,
            freshness,
            journal_watermark,
        })
    }

    /// Metadata of the immutable generation used by this reader.
    pub fn manifest(&self) -> &UsageManifest {
        &self.manifest
    }

    /// Entries beyond the complete view, awaiting independent publication.
    pub fn pending_entries(&self) -> u64 {
        self.pending_entries
    }

    /// Whether the view includes the complete captured journal prefix.
    pub fn freshness(&self) -> Freshness {
        self.freshness
    }

    /// Last journal fact represented by this consistent snapshot plus tail.
    pub fn journal_watermark(&self) -> u64 {
        self.journal_watermark
    }

    /// Resolve trusted descendants from this generation and its bounded tail.
    pub fn thread_scope(
        &self,
        root_thread_id: &str,
        scope: crate::ThreadLineageScope,
    ) -> Result<Vec<String>> {
        crate::read_descendant_threads(
            &self.root,
            &self.manifest,
            root_thread_id,
            scope,
            &self.records,
        )
    }

    /// Read mergeable totals without decoding historical observations in Rust.
    pub fn totals(&self, range: TimeRange, filter: &UsageFilter) -> Result<UsageTotals> {
        if range == TimeRange::all()
            && self.corrections.is_empty()
            && (self.manifest.correction_files.is_empty()
                || self.manifest.summaries_include_corrections)
            && filter.model.is_none()
        {
            let totals = if let Some(threads) = &filter.thread_ids {
                if filter.account_id.is_none()
                    && filter.repository_id.is_none()
                    && filter.thread_id.is_none()
                {
                    Some(
                        threads
                            .iter()
                            .filter_map(|thread| self.manifest.thread_totals.get(thread))
                            .fold(UsageTotals::default(), |mut total, delta| {
                                total.add_assign(delta);
                                total
                            }),
                    )
                } else {
                    None
                }
            } else {
                match (&filter.account_id, &filter.thread_id, &filter.repository_id) {
                    (None, None, None) => Some(self.manifest.snapshot_totals.clone()),
                    (Some(id), None, None) => Some(
                        self.manifest
                            .account_totals
                            .get(id)
                            .cloned()
                            .unwrap_or_default(),
                    ),
                    (None, Some(id), None) => Some(
                        self.manifest
                            .thread_totals
                            .get(id)
                            .cloned()
                            .unwrap_or_default(),
                    ),
                    (None, None, Some(id)) => Some(
                        self.manifest
                            .repository_totals
                            .get(id)
                            .cloned()
                            .unwrap_or_default(),
                    ),
                    _ => None,
                }
            };
            if let Some(mut totals) = totals {
                if !self.records.is_empty() {
                    let connection = duckdb::Connection::open_in_memory()
                        .map_err(crate::reader_query::query_error)?;
                    for record in crate::report_query::canonical_tail(self, &connection)? {
                        if filter.matches(record) {
                            totals.add_assign(&record.totals());
                        }
                    }
                }
                return Ok(totals);
            }
        }
        SnapshotQuery::new(self, range, filter)?.totals()
    }

    /// Apply filters, stable ordering and the page bound inside DuckDB.
    pub fn page(
        &self,
        range: TimeRange,
        filter: &UsageFilter,
        kind: RecordKind,
        after: Option<&RecordCursor>,
        limit: usize,
    ) -> Result<RecordPage> {
        if !(1..=50).contains(&limit) {
            return Err(StorageError::InvalidData(
                "page limit must be between 1 and 50".into(),
            ));
        }
        let mut records =
            SnapshotQuery::new(self, range, filter)?.records(kind, after, limit + 1)?;
        let has_more = records.len() > limit;
        records.truncate(limit);
        let next = has_more.then(|| records.last()).flatten().map(|record| {
            RecordCursor::from_millis(record.effective_observed_at_ms(), record.id.clone())
        });
        Ok(RecordPage { records, next })
    }

    /// Group repository totals in the query engine before limiting the result.
    /// Returns one lookahead row so callers can expose a lossless cursor.
    pub fn repositories(
        &self,
        range: TimeRange,
        filter: &UsageFilter,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RepositoryUsage>> {
        if !(1..=50).contains(&limit) {
            return Err(StorageError::InvalidData(
                "page limit must be between 1 and 50".into(),
            ));
        }
        SnapshotQuery::new(self, range, filter)?.repositories(after, limit + 1)
    }
}
