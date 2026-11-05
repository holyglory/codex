use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
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

/// Current on-disk manifest schema version.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

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
    /// Wall-clock time at which the snapshot watermark was published.
    pub watermark_at_ms: i64,
    /// Producer freshness state for the snapshot.
    pub freshness: Freshness,
    /// Correction ids observed in the journal but not yet folded into a snapshot.
    #[serde(default)]
    pub pending_corrections: Vec<String>,
    /// Immutable segment file names referenced by this snapshot generation.
    ///
    /// Names are relative to the dedicated usage segment directory. Keeping
    /// the list in the manifest lets recovery ignore orphaned files left by a
    /// crash between segment publication and manifest replacement.
    #[serde(default)]
    pub segment_files: Vec<String>,
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
    /// Unix timestamp when this manifest was last changed.
    pub updated_at: i64,
}

impl Default for UsageManifest {
    fn default() -> Self {
        Self {
            format_version: MANIFEST_FORMAT_VERSION,
            snapshot_generation: 0,
            watermark: 0,
            watermark_at_ms: 0,
            freshness: Freshness::Unknown,
            pending_corrections: Vec::new(),
            segment_files: Vec::new(),
            snapshot_totals: UsageTotals::default(),
            correction_files: Vec::new(),
            account_totals: BTreeMap::new(),
            repository_totals: BTreeMap::new(),
            thread_totals: BTreeMap::new(),
            updated_at: 0,
        }
    }
}

impl UsageManifest {
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
        let contents = fs::read_to_string(&self.path)?;
        let manifest = serde_json::from_str(&contents)?;
        Ok(manifest)
    }

    /// Atomically write one manifest generation.
    pub fn save(&self, manifest: &UsageManifest) -> Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        let mut encoded = serde_json::to_vec_pretty(manifest)?;
        encoded.push(b'\n');
        let temporary =
            self.path
                .with_extension(format!("tmp-{}-{}", std::process::id(), unix_now_nanos()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&encoded)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        if let Err(error) = fs::rename(&temporary, &self.path) {
            let _ = fs::remove_file(&temporary);
            return Err(StorageError::Io(error));
        }
        #[cfg(unix)]
        if let Some(parent) = self.path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
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
