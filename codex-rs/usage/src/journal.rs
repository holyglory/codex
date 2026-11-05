use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Result;
use crate::error::StorageError;
use crate::model::JournalSequence;
use crate::model::UsageCorrection;
use crate::model::UsageRecord;

/// One append-only journal payload. Corrections are events, never in-place
/// updates, so a replay can reconstruct the complete audit trail.
// The record carries the complete dimensional fact and is intentionally kept
// inline so journal append/replay does not allocate a second heap object.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum JournalPayload {
    /// A new usage observation.
    Record(UsageRecord),
    /// A signed adjustment to an existing observation.
    Correction(UsageCorrection),
}

/// One journal line with its monotonic sequence number.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JournalEntry {
    /// Monotonic sequence assigned by the journal writer.
    pub sequence: JournalSequence,
    /// Append-only payload.
    pub payload: JournalPayload,
}

/// Location returned after an append.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalPosition {
    /// Sequence assigned to the appended entry.
    pub sequence: JournalSequence,
    /// Byte offset at which the JSONL line starts.
    pub byte_offset: u64,
}

/// Append-only JSONL journal for observations and corrections.
///
/// A `UsageJournal` serializes writers cloned from the same handle. Multiple
/// processes should use one journal owner or an outer file lock; no writer ever
/// rewrites an existing line.
#[derive(Clone, Debug)]
pub struct UsageJournal {
    path: PathBuf,
    append_lock: Arc<Mutex<()>>,
}

impl UsageJournal {
    /// Open a journal path without creating or mutating it.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            append_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Return the JSONL journal path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one usage observation.
    pub fn append_record(&self, record: UsageRecord) -> Result<JournalPosition> {
        self.append(JournalPayload::Record(record))
    }

    /// Append one signed correction.
    pub fn append_correction(&self, correction: UsageCorrection) -> Result<JournalPosition> {
        self.append(JournalPayload::Correction(correction))
    }

    /// Append one payload and flush it as a complete JSONL line.
    pub fn append(&self, payload: JournalPayload) -> Result<JournalPosition> {
        let _guard = self
            .append_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sequence = self
            .last_sequence_locked()?
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("journal sequence exhausted".into()))?;
        let entry = JournalEntry { sequence, payload };
        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');

        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let byte_offset = file.metadata()?.len();
        file.write_all(&line)?;
        file.flush()?;
        file.sync_data()?;
        Ok(JournalPosition {
            sequence,
            byte_offset,
        })
    }

    /// Replay all valid entries in sequence order.
    pub fn replay(&self) -> Result<Vec<JournalEntry>> {
        self.read_entries()
    }

    /// Replay entries after `sequence`, excluding the watermark itself.
    pub fn read_after(&self, sequence: JournalSequence) -> Result<Vec<JournalEntry>> {
        Ok(self
            .read_entries()?
            .into_iter()
            .filter(|entry| entry.sequence > sequence)
            .collect())
    }

    /// Return the highest sequence currently present, or zero for an empty journal.
    pub fn last_sequence(&self) -> Result<JournalSequence> {
        self.read_entries()?
            .last()
            .map_or(Ok(0), |entry| Ok(entry.sequence))
    }

    /// Replay only observation records.
    pub fn records(&self) -> Result<Vec<UsageRecord>> {
        Ok(self
            .read_entries()?
            .into_iter()
            .filter_map(|entry| match entry.payload {
                JournalPayload::Record(record) => Some(record),
                JournalPayload::Correction(_) => None,
            })
            .collect())
    }

    /// Replay only correction records.
    pub fn corrections(&self) -> Result<Vec<UsageCorrection>> {
        Ok(self
            .read_entries()?
            .into_iter()
            .filter_map(|entry| match entry.payload {
                JournalPayload::Record(_) => None,
                JournalPayload::Correction(correction) => Some(correction),
            })
            .collect())
    }

    fn last_sequence_locked(&self) -> Result<JournalSequence> {
        self.read_entries()?
            .last()
            .map_or(Ok(0), |entry| Ok(entry.sequence))
    }

    fn read_entries(&self) -> Result<Vec<JournalEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&self.path)?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut line_number = 0usize;
        let mut expected_sequence = 1u64;
        let mut entries = Vec::new();
        loop {
            line.clear();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            line_number += 1;
            if line.trim().is_empty() {
                continue;
            }
            let entry = serde_json::from_str::<JournalEntry>(line.trim_end()).map_err(|error| {
                StorageError::InvalidData(format!("journal line {line_number}: {error}"))
            })?;
            if entry.sequence != expected_sequence {
                return Err(StorageError::InvalidData(format!(
                    "journal line {line_number} has sequence {}, expected {expected_sequence}",
                    entry.sequence
                )));
            }
            expected_sequence = expected_sequence
                .checked_add(1)
                .ok_or_else(|| StorageError::InvalidData("journal sequence exhausted".into()))?;
            entries.push(entry);
        }
        Ok(entries)
    }
}
