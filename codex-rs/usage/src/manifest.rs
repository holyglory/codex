use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Result;
use crate::error::StorageError;
use crate::model::Freshness;
use crate::model::JournalSequence;
use crate::model::SnapshotGeneration;
use crate::model::UsageTotals;
use crate::review_counts::ReviewCounts;
use crate::storage_file;

/// Current on-disk manifest schema version.
pub const MANIFEST_FORMAT_VERSION: u32 = 3;

/// Metadata describing the materialized usage snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageManifest {
    /// Schema version for this manifest JSON object.
    pub format_version: u32,
    /// Monotonic generation of the materialized snapshot.
    pub snapshot_generation: SnapshotGeneration,
    /// Highest journal sequence represented by the snapshot.
    pub watermark: JournalSequence,
    /// End offset of the complete journal entry at the snapshot watermark.
    /// Recovery seeks here and reads only the mutable tail.
    pub journal_byte_offset: u64,
    /// Wall-clock time at which the snapshot watermark was published.
    pub watermark_at_ms: i64,
    /// Producer freshness state for the snapshot.
    pub freshness: Freshness,
    /// Correction ids observed in the journal but not yet folded into a snapshot.
    #[serde(default)]
    pub pending_corrections: Vec<String>,
    /// Correction ids already folded into all precomputed summary dimensions.
    #[serde(default)]
    pub applied_correction_ids: BTreeSet<String>,
    /// Whether precomputed summaries include every resolvable correction.
    /// False conservatively requires readers to query correction sidecars.
    #[serde(default)]
    pub summaries_include_corrections: bool,
    /// Immutable segment file names referenced by this snapshot generation.
    ///
    /// Names are relative to the dedicated usage segment directory. Keeping
    /// the list in the manifest lets recovery ignore orphaned files left by a
    /// crash between segment publication and manifest replacement.
    #[serde(default)]
    pub segment_files: Vec<String>,
    /// Complete immutable dimension/hour membership index, relative to segments.
    /// None preserves safe query fallback for manifests without an index.
    #[serde(default)]
    pub membership_file: Option<String>,
    /// Immutable membership fragments used after cumulative indexes outgrow the
    /// bounded publication rewrite threshold.
    #[serde(default)]
    pub membership_files: Vec<String>,
    /// Immutable explicit child/parent relationships, relative to segments.
    /// Readers open this index only when descendant traversal is requested.
    #[serde(default)]
    pub lineage_file: Option<String>,
    /// Immutable exact record-id runs used for bounded publisher deduplication.
    #[serde(default)]
    pub id_index_file: Option<String>,
    /// Immutable bounded operation state and indexed observation projections.
    /// None means review coverage has not been published for this generation.
    #[serde(default)]
    pub review_projection_file: Option<String>,
    /// Mergeable global totals for the immutable snapshot.
    #[serde(default)]
    pub snapshot_totals: UsageTotals,
    /// Immutable correction segment files referenced by this snapshot.
    #[serde(default)]
    pub correction_files: Vec<String>,
    /// Mergeable immutable totals grouped by account identity.
    #[serde(default)]
    pub account_totals: BTreeMap<String, UsageTotals>,
    /// Mergeable immutable totals grouped by repository identity.
    #[serde(default)]
    pub repository_totals: BTreeMap<String, UsageTotals>,
    /// Mergeable immutable totals grouped by thread identity.
    #[serde(default)]
    pub thread_totals: BTreeMap<String, UsageTotals>,
    /// Latest observed activity per chat, used for startup warming and retention.
    #[serde(default)]
    pub thread_last_activity_at_ms: BTreeMap<String, i64>,
    /// Compact operation counts for all-time single-dimension summaries.
    #[serde(default)]
    pub review_counts: BTreeMap<String, ReviewCounts>,
    /// Journal frontier represented by the cached operation counts. Missing
    /// means readers must use the immutable count sidecar instead.
    #[serde(default)]
    pub review_counts_watermark: Option<JournalSequence>,
    /// Unix timestamp when this manifest was last changed.
    pub updated_at: i64,
}

impl Default for UsageManifest {
    fn default() -> Self {
        Self {
            format_version: MANIFEST_FORMAT_VERSION,
            snapshot_generation: 0,
            watermark: 0,
            journal_byte_offset: 0,
            watermark_at_ms: 0,
            freshness: Freshness::Unknown,
            pending_corrections: Vec::new(),
            applied_correction_ids: BTreeSet::new(),
            summaries_include_corrections: false,
            segment_files: Vec::new(),
            membership_file: None,
            membership_files: Vec::new(),
            lineage_file: None,
            id_index_file: None,
            review_projection_file: None,
            snapshot_totals: UsageTotals::default(),
            correction_files: Vec::new(),
            account_totals: BTreeMap::new(),
            repository_totals: BTreeMap::new(),
            thread_totals: BTreeMap::new(),
            thread_last_activity_at_ms: BTreeMap::new(),
            review_counts: BTreeMap::new(),
            review_counts_watermark: None,
            updated_at: 0,
        }
    }
}

impl UsageManifest {
    pub(crate) fn observe_thread_activity(&mut self, record: &crate::UsageRecord) {
        if let Some(id) = &record.thread_id {
            let at = record.effective_observed_at_ms();
            self.thread_last_activity_at_ms
                .entry(id.clone())
                .and_modify(|previous| *previous = (*previous).max(at))
                .or_insert(at);
        }
    }
    /// Create an empty manifest with an explicit freshness state.
    pub fn new(freshness: Freshness) -> Self {
        Self {
            freshness,
            ..Self::default()
        }
    }

    /// Advance the snapshot generation and set its journal watermark.
    pub fn mark_snapshot(&mut self, watermark: JournalSequence, freshness: Freshness) {
        self.snapshot_generation = self.snapshot_generation.saturating_add(1);
        self.watermark = watermark;
        self.watermark_at_ms = unix_now().saturating_mul(1_000);
        self.freshness = freshness;
        self.updated_at = unix_now();
    }

    /// Record a correction that still needs to be folded into a snapshot.
    pub fn add_pending_correction(&mut self, correction_id: impl Into<String>) {
        let correction_id = correction_id.into();
        if !self
            .pending_corrections
            .iter()
            .any(|id| id == &correction_id)
        {
            self.pending_corrections.push(correction_id);
            self.updated_at = unix_now();
        }
    }

    /// Mark one pending correction as folded into a snapshot.
    pub fn acknowledge_correction(&mut self, correction_id: &str) -> bool {
        let before = self.pending_corrections.len();
        self.pending_corrections.retain(|id| id != correction_id);
        let changed = before != self.pending_corrections.len();
        if changed {
            self.updated_at = unix_now();
        }
        changed
    }
}

/// JSON manifest reader/writer. The writer replaces only the manifest metadata;
/// journal and segment files remain append-only/immutable data sources.
#[derive(Clone, Debug)]
pub struct ManifestStore {
    path: PathBuf,
}

impl ManifestStore {
    /// Share parsed metadata with the long-lived reader/publisher process.
    pub(crate) fn load_shared(&self) -> Result<crate::metadata_cache::Parsed<UsageManifest>> {
        static CACHE: crate::metadata_cache::JsonCache<UsageManifest> =
            crate::metadata_cache::JsonCache::new();
        match CACHE.load(&self.path) {
            Err(StorageError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(crate::metadata_cache::Parsed {
                    value: std::sync::Arc::default(),
                    revision: 0,
                })
            }
            result => result,
        }
    }
    /// Open a manifest path without creating or mutating it.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Return the configured manifest path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the manifest, returning defaults when no manifest has been written.
    pub fn load(&self) -> Result<UsageManifest> {
        if !self.path.exists() {
            return Ok(UsageManifest::default());
        }
        storage_file::protect_existing(&self.path)?;
        let contents = fs::read_to_string(&self.path)?;
        let manifest = serde_json::from_str(&contents)?;
        Ok(manifest)
    }

    /// Atomically write one manifest generation.
    pub fn save(&self, manifest: &UsageManifest) -> Result<()> {
        if self.path.exists() {
            storage_file::protect_existing(&self.path)?;
        }
        let mut encoded = serde_json::to_vec(manifest)?;
        encoded.push(b'\n');
        let temporary =
            self.path
                .with_extension(format!("tmp-{}-{}", std::process::id(), unix_now_nanos()));
        let mut file = storage_file::create_new(&temporary)?;
        file.write_all(&encoded)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        if let Err(error) = fs::rename(&temporary, &self.path) {
            let _ = fs::remove_file(&temporary);
            return Err(StorageError::Io(error));
        }
        #[cfg(unix)]
        fs::File::open(
            self.path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )?
        .sync_all()?;
        Ok(())
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
}

fn unix_now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}
