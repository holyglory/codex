use std::fs::File;
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
use crate::journal_rotation::JournalBase;
use crate::model::JournalSequence;
use crate::model::UsageCorrection;
use crate::model::UsageRecord;
use crate::storage_file;

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

/// Positive bounds for one journal read, excluding checkpoint boundary checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalReadLimits {
    /// Maximum bytes read from the requested range, including newlines.
    pub max_bytes: u64,
    /// Maximum complete entries returned.
    pub max_entries: usize,
}

/// Whether a bounded read reached its captured target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalReadStatus {
    /// Every entry through the target was consumed.
    Complete,
    /// Continue at the returned checkpoint with the same target.
    More,
    /// The next entry cannot fit; no entry was consumed or checkpoint advanced.
    EntryExceedsByteLimit {
        /// Lower bound on the byte limit needed, not the entry's full size.
        minimum_bytes: u64,
    },
}

/// Complete entries from one bounded portion of a captured journal prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalReadBatch {
    /// Entries consumed, in strictly consecutive sequence order.
    pub entries: Vec<JournalEntry>,
    /// End of the last consumed entry; use this to continue without loss.
    pub next: JournalCheckpoint,
    /// Fixed end of the requested prefix, excluding later appends.
    pub target: JournalCheckpoint,
    /// Completion or the limit preventing further progress.
    pub status: JournalReadStatus,
}

/// A complete report overlay, or an explicit backlog left for the publisher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JournalTailRead {
    /// The entire captured tail fits; this batch may overlay the saved snapshot.
    Complete(JournalReadBatch),
    /// No partial overlay is returned when the complete tail exceeds its bounds.
    Deferred {
        /// Captured end of the complete tail, including all deferred entries.
        target: JournalCheckpoint,
        /// Complete entries after the caller's watermark.
        pending_entries: u64,
        /// Complete bytes after the caller's watermark.
        pending_bytes: u64,
    },
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
        let writer_lock = storage_file::open_read_write(&self.path.with_extension("lock"))?;
        writer_lock.lock()?;
        let mut file = storage_file::open_append(&self.path)?;
        file.lock()?;
        let length = file.metadata()?.len();
        let base = JournalBase::read(&mut file)?;
        let mut checkpoint = tail_checkpoint(&mut file, length)?;
        #[cfg(unix)]
        let needs_directory_sync = checkpoint.byte_offset == 0;
        checkpoint
            .sequence
            .checked_add(payloads.len() as u64)
            .ok_or_else(|| StorageError::InvalidData("journal sequence exhausted".into()))?;
        let complete_length = base.physical_offset(checkpoint.byte_offset)?;
        if complete_length != length {
            // Windows append handles cannot truncate. Release this handle's lock
            // before opening the recovery handle; the stable writer lock still
            // excludes concurrent appends and rotation throughout both reopens.
            drop(file);
            let recovery = storage_file::open_read_write(&self.path)?;
            recovery.lock()?;
            recovery.set_len(complete_length)?;
            drop(recovery);
            file = storage_file::open_append(&self.path)?;
            file.lock()?;
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
        let batch = self.read_batch(
            checkpoint,
            self.checkpoint()?,
            JournalReadLimits {
                max_bytes: u64::MAX,
                max_entries: usize::MAX,
            },
        )?;
        Ok((batch.entries, batch.next))
    }

    /// Read bounded complete entries through a previously captured target.
    /// Later appends are excluded, and no historical prefix is replayed.
    /// An oversized first entry leaves the checkpoint unchanged so callers can
    /// explicitly increase the limit without skipping canonical data.
    pub fn read_batch(
        &self,
        after: JournalCheckpoint,
        target: JournalCheckpoint,
        limits: JournalReadLimits,
    ) -> Result<JournalReadBatch> {
        validate_read(after, target, limits)?;
        let mut batch = JournalReadBatch {
            entries: Vec::new(),
            next: after,
            target,
            status: JournalReadStatus::Complete,
        };
        if target == JournalCheckpoint::default() {
            return Ok(batch);
        }
        let mut file = File::open(&self.path)?;
        storage_file::protect_existing(&self.path)?;
        file.lock_shared()?;
        let base = JournalBase::read(&mut file)?;
        if after.sequence < base.checkpoint.sequence {
            return Err(StorageError::InvalidData(
                "journal checkpoint was retired".into(),
            ));
        }
        if base.physical_offset(target.byte_offset)? > file.metadata()?.len() {
            return Err(StorageError::InvalidData(
                "journal checkpoint past end".into(),
            ));
        }
        for logical in [after.byte_offset, target.byte_offset] {
            let offset = base.physical_offset(logical)?;
            if offset == 0 {
                continue;
            }
            file.seek(SeekFrom::Start(offset - 1))?;
            let mut boundary = [0];
            file.read_exact(&mut boundary)?;
            if boundary != [b'\n'] {
                return Err(StorageError::InvalidData(
                    "journal checkpoint inside entry".into(),
                ));
            }
        }
        if after == target {
            return Ok(batch);
        }
        file.seek(SeekFrom::Start(base.physical_offset(after.byte_offset)?))?;
        file.unlock()?;
        let byte_limit = limits.max_bytes.min(target.byte_offset - after.byte_offset);
        let mut reader = BufReader::with_capacity(
            /*capacity*/ 64 * 1024,
            SnapshotReader(file).take(byte_limit),
        );
        let mut line = Vec::new();
        while batch.entries.len() < limits.max_entries
            && batch.next.byte_offset < target.byte_offset
        {
            line.clear();
            let bytes = reader.read_until(b'\n', &mut line)?;
            if bytes == 0 {
                if batch.next.byte_offset - after.byte_offset < byte_limit {
                    return Err(StorageError::InvalidData(
                        "journal snapshot changed during replay".into(),
                    ));
                }
                break;
            }
            if !line.ends_with(b"\n") {
                if byte_limit == target.byte_offset - after.byte_offset {
                    return Err(StorageError::InvalidData(
                        "incomplete captured journal entry".into(),
                    ));
                }
                batch.status = if batch.entries.is_empty() {
                    JournalReadStatus::EntryExceedsByteLimit {
                        minimum_bytes: limits.max_bytes.saturating_add(1),
                    }
                } else {
                    JournalReadStatus::More
                };
                return Ok(batch);
            }
            let entry: JournalEntry = serde_json::from_slice(&line).map_err(|error| {
                StorageError::InvalidData(format!(
                    "journal byte {}: {error}",
                    batch.next.byte_offset
                ))
            })?;
            if Some(entry.sequence) != batch.next.sequence.checked_add(1) {
                return Err(StorageError::InvalidData(format!(
                    "journal byte {} has nonconsecutive sequence {} after {}",
                    batch.next.byte_offset, entry.sequence, batch.next.sequence
                )));
            }
            batch.next.sequence = entry.sequence;
            batch.next.byte_offset += bytes as u64;
            batch.entries.push(entry);
        }
        if batch.next.byte_offset == target.byte_offset {
            if batch.next != target {
                return Err(StorageError::InvalidData(
                    "journal target sequence mismatch".into(),
                ));
            }
        } else {
            batch.status = JournalReadStatus::More;
        }
        Ok(batch)
    }

    /// Capture a sealed complete prefix and overlay it only when it all fits.
    /// Deferred reads leave every journal entry available for publication and
    /// let reports serve their saved snapshot with explicit backlog metadata.
    pub fn read_tail(
        &self,
        after: JournalCheckpoint,
        limits: JournalReadLimits,
    ) -> Result<JournalTailRead> {
        let target = self.checkpoint()?;
        validate_read(after, target, limits)?;
        let pending_bytes = target.byte_offset - after.byte_offset;
        let pending_entries = target.sequence - after.sequence;
        if pending_bytes > limits.max_bytes || pending_entries > limits.max_entries as u64 {
            return Ok(JournalTailRead::Deferred {
                target,
                pending_entries,
                pending_bytes,
            });
        }
        let batch = self.read_batch(after, target, limits)?;
        if batch.status != JournalReadStatus::Complete {
            return Err(StorageError::InvalidData(
                "journal target sequence mismatch".into(),
            ));
        }
        Ok(JournalTailRead::Complete(batch))
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
        storage_file::protect_existing(&self.path)?;
        file.lock_shared()?;
        let length = file.metadata()?.len();
        let checkpoint = tail_checkpoint(&mut file, length)?;
        Ok(Some((file, checkpoint)))
    }

    fn read_entries(&self) -> Result<Vec<JournalEntry>> {
        let Some((mut file, target)) = self.snapshot()? else {
            return Ok(Vec::new());
        };
        let base = JournalBase::read(&mut file)?;
        drop(file);
        Ok(self
            .read_batch(
                base.checkpoint,
                target,
                JournalReadLimits {
                    max_bytes: u64::MAX,
                    max_entries: usize::MAX,
                },
            )?
            .entries)
    }
}

fn validate_read(
    after: JournalCheckpoint,
    target: JournalCheckpoint,
    limits: JournalReadLimits,
) -> Result<()> {
    if limits.max_bytes == 0
        || limits.max_entries == 0
        || (after.sequence == 0) != (after.byte_offset == 0)
        || (target.sequence == 0) != (target.byte_offset == 0)
        || after.sequence > target.sequence
        || after.byte_offset > target.byte_offset
        || (after.sequence == target.sequence) != (after.byte_offset == target.byte_offset)
    {
        return Err(StorageError::InvalidData(
            "invalid journal read limits or checkpoint".into(),
        ));
    }
    Ok(())
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

pub(crate) fn tail_checkpoint(file: &mut File, length: u64) -> Result<JournalCheckpoint> {
    let base = JournalBase::read(file)?;
    if length < base.header_bytes {
        return Err(StorageError::InvalidData(
            "incomplete journal header".into(),
        ));
    }
    let Some(end) = previous_newline(file, length)? else {
        return Ok(JournalCheckpoint::default());
    };
    if end < base.header_bytes {
        return Ok(base.checkpoint);
    }
    let start = previous_newline(file, end)?.map_or(0, |offset| offset + 1);
    file.seek(SeekFrom::Start(start))?;
    let mut line = Vec::new();
    file.take(end - start).read_to_end(&mut line)?;
    let entry: JournalEntry = serde_json::from_slice(&line)
        .map_err(|error| StorageError::InvalidData(format!("journal byte {start}: {error}")))?;
    if entry.sequence <= base.checkpoint.sequence
        || (start == base.header_bytes
            && Some(entry.sequence) != base.checkpoint.sequence.checked_add(1))
    {
        return Err(StorageError::InvalidData(
            "invalid journal tail sequence".into(),
        ));
    }
    Ok(JournalCheckpoint {
        sequence: entry.sequence,
        byte_offset: base.logical_offset(end + 1)?,
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
