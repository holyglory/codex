use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

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

/// A replay watermark at the end of a complete journal entry.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct JournalCheckpoint {
    /// Sequence of the last consumed entry, or zero for an empty journal.
    pub sequence: JournalSequence,
    /// Byte offset immediately after that entry's terminating newline.
    pub byte_offset: u64,
}

/// Append-only JSONL journal for observations and corrections.
///
/// OS file locks serialize independent writers, including other processes.
/// Complete lines are immutable; an interrupted final line is discarded before
/// the next append. Successful appends have been synchronized to storage.
#[derive(Clone, Debug)]
pub struct UsageJournal {
    path: PathBuf,
}

impl UsageJournal {
    /// Open a journal path without creating or mutating it.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
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

    /// Append one payload and synchronize its complete JSONL line to storage.
    pub fn append(&self, payload: JournalPayload) -> Result<JournalPosition> {
        Ok(self.append_batch(vec![payload])?[0])
    }

    /// Append an ordered batch, synchronizing every returned position together.
    pub fn append_batch(&self, payloads: Vec<JournalPayload>) -> Result<Vec<JournalPosition>> {
        if payloads.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.path)?;
        file.lock()?;
        let length = file.metadata()?.len();
        let mut checkpoint = tail_checkpoint(&mut file, length)?;
        #[cfg(unix)]
        let needs_directory_sync = checkpoint.byte_offset == 0;
        checkpoint
            .sequence
            .checked_add(payloads.len() as u64)
            .ok_or_else(|| StorageError::InvalidData("journal sequence exhausted".into()))?;
        if checkpoint.byte_offset != length {
            file.set_len(checkpoint.byte_offset)?;
        }
        let mut positions = Vec::with_capacity(payloads.len());
        for payload in payloads {
            let sequence = checkpoint.sequence + 1;
            let mut line = serde_json::to_vec(&JournalEntry { sequence, payload })?;
            line.push(b'\n');
            file.write_all(&line)?;
            positions.push(JournalPosition {
                sequence,
                byte_offset: checkpoint.byte_offset,
            });
            checkpoint = JournalCheckpoint {
                sequence,
                byte_offset: checkpoint.byte_offset + line.len() as u64,
            };
        }
        file.sync_data()?;
        #[cfg(unix)]
        if needs_directory_sync {
            let parent = self
                .path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            File::open(parent)?.sync_all()?;
        }
        Ok(positions)
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
        Ok(self.checkpoint()?.sequence)
    }

    /// Inspect only the final entry and any interrupted suffix.
    pub fn checkpoint(&self) -> Result<JournalCheckpoint> {
        Ok(self
            .snapshot()?
            .map_or_else(JournalCheckpoint::default, |(_, checkpoint)| checkpoint))
    }

    /// Read complete entries after a saved watermark from one fixed snapshot.
    ///
    /// Shared locks cover tail inspection and each bounded buffered read.
    /// Parsing runs without the lock, allowing writers to continue during replay.
    pub fn read_from(
        &self,
        checkpoint: JournalCheckpoint,
    ) -> Result<(Vec<JournalEntry>, JournalCheckpoint)> {
        let (mut file, snapshot) = match self.snapshot()? {
            Some(snapshot) => snapshot,
            None if checkpoint == JournalCheckpoint::default() => {
                return Ok((Vec::new(), checkpoint));
            }
            None => {
                return Err(StorageError::InvalidData(
                    "journal checkpoint past end".into(),
                ));
            }
        };
        if checkpoint.byte_offset > snapshot.byte_offset
            || checkpoint.sequence > snapshot.sequence
            || (checkpoint.byte_offset == 0 && checkpoint.sequence != 0)
            || (checkpoint.byte_offset == snapshot.byte_offset && checkpoint != snapshot)
        {
            return Err(StorageError::InvalidData(
                "invalid journal checkpoint".into(),
            ));
        }
        if checkpoint.byte_offset > 0 {
            file.seek(SeekFrom::Start(checkpoint.byte_offset - 1))?;
            let mut boundary = [0];
            file.read_exact(&mut boundary)?;
            if boundary != [b'\n'] {
                return Err(StorageError::InvalidData(
                    "journal checkpoint inside entry".into(),
                ));
            }
        }
        file.seek(SeekFrom::Start(checkpoint.byte_offset))?;
        file.unlock()?;
        let mut reader = BufReader::with_capacity(
            /*capacity*/ 64 * 1024,
            SnapshotReader(file).take(snapshot.byte_offset - checkpoint.byte_offset),
        );
        let mut checkpoint = checkpoint;
        let mut line = Vec::new();
        let mut entries = Vec::new();
        loop {
            line.clear();
            let bytes = reader.read_until(b'\n', &mut line)?;
            if bytes == 0 {
                break;
            }
            let entry: JournalEntry = serde_json::from_slice(&line).map_err(|error| {
                StorageError::InvalidData(format!(
                    "journal byte {}: {error}",
                    checkpoint.byte_offset
                ))
            })?;
            if Some(entry.sequence) != checkpoint.sequence.checked_add(1) {
                return Err(StorageError::InvalidData(format!(
                    "journal byte {} has nonconsecutive sequence {} after {}",
                    checkpoint.byte_offset, entry.sequence, checkpoint.sequence
                )));
            }
            checkpoint.sequence = entry.sequence;
            checkpoint.byte_offset += bytes as u64;
            entries.push(entry);
        }
        if checkpoint != snapshot {
            return Err(StorageError::InvalidData(
                "journal snapshot changed during replay".into(),
            ));
        }
        Ok((entries, checkpoint))
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

    fn snapshot(&self) -> Result<Option<(File, JournalCheckpoint)>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        file.lock_shared()?;
        let length = file.metadata()?.len();
        let checkpoint = tail_checkpoint(&mut file, length)?;
        Ok(Some((file, checkpoint)))
    }

    fn read_entries(&self) -> Result<Vec<JournalEntry>> {
        Ok(self.read_from(JournalCheckpoint::default())?.0)
    }
}

struct SnapshotReader(File);

impl Read for SnapshotReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        // Windows file locks also restrict reads from handles without a lock.
        self.0.lock_shared()?;
        let result = self.0.read(buffer);
        self.0.unlock()?;
        result
    }
}

fn tail_checkpoint(file: &mut File, length: u64) -> Result<JournalCheckpoint> {
    let Some(end) = previous_newline(file, length)? else {
        return Ok(JournalCheckpoint::default());
    };
    let start = previous_newline(file, end)?.map_or(0, |offset| offset + 1);
    file.seek(SeekFrom::Start(start))?;
    let mut line = Vec::new();
    file.take(end - start).read_to_end(&mut line)?;
    let entry: JournalEntry = serde_json::from_slice(&line)
        .map_err(|error| StorageError::InvalidData(format!("journal byte {start}: {error}")))?;
    if entry.sequence == 0 || (start == 0 && entry.sequence != 1) {
        return Err(StorageError::InvalidData(
            "invalid journal tail sequence".into(),
        ));
    }
    Ok(JournalCheckpoint {
        sequence: entry.sequence,
        byte_offset: end + 1,
    })
}

fn previous_newline(file: &mut File, mut end: u64) -> Result<Option<u64>> {
    let mut buffer = [0; 8192];
    while end > 0 {
        let start = end.saturating_sub(buffer.len() as u64);
        let length = (end - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..length])?;
        if let Some(index) = buffer[..length].iter().rposition(|byte| *byte == b'\n') {
            return Ok(Some(start + index as u64));
        }
        end = start;
    }
    Ok(None)
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
