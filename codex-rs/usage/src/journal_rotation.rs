//! Reclaim a published journal prefix without changing logical checkpoints.
//! A fixed header records the retired prefix. Writers lock a stable sidecar,
//! so atomic replacement cannot leave a writer appending to an unlinked file.
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;

use crate::JournalCheckpoint;
use crate::Result;
use crate::StorageError;
use crate::storage_file;

const HEADER_BYTES: u64 = 96;
const HEADER_PREFIX: &[u8] = b"{\"journalBase\":[";
const MAX_ROTATION_TAIL_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Default)]
pub(crate) struct JournalBase {
    pub checkpoint: JournalCheckpoint,
    pub header_bytes: u64,
}

impl JournalBase {
    pub(crate) fn read(file: &mut File) -> Result<Self> {
        file.seek(SeekFrom::Start(0))?;
        let mut prefix = [0; HEADER_PREFIX.len()];
        let length = file.read(&mut prefix)?;
        if length != prefix.len() || prefix != HEADER_PREFIX {
            return Ok(Self::default());
        }
        file.seek(SeekFrom::Start(0))?;
        let mut header = [0; HEADER_BYTES as usize];
        file.read_exact(&mut header)?;
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Header {
            journal_base: [u64; 2],
        }
        let Header {
            journal_base: [sequence, byte_offset],
        } = serde_json::from_slice(&header)?;
        if (sequence == 0) != (byte_offset == 0) || header[header.len() - 1] != b'\n' {
            return Err(invalid("invalid journal base checkpoint"));
        }
        Ok(Self {
            checkpoint: JournalCheckpoint {
                sequence,
                byte_offset,
            },
            header_bytes: HEADER_BYTES,
        })
    }

    pub(crate) fn physical_offset(self, logical: u64) -> Result<u64> {
        logical
            .checked_sub(self.checkpoint.byte_offset)
            .and_then(|offset| offset.checked_add(self.header_bytes))
            .ok_or_else(|| invalid("journal checkpoint was retired"))
    }

    pub(crate) fn logical_offset(self, physical: u64) -> Result<u64> {
        physical
            .checked_sub(self.header_bytes)
            .and_then(|offset| offset.checked_add(self.checkpoint.byte_offset))
            .ok_or_else(|| invalid("invalid journal physical offset"))
    }
}

/// Caller holds the publisher lock and excludes readers pinning older metadata.
/// Work under the writer lock is capped at one MiB; a larger tail defers rotation.
pub(crate) fn rotate_prefix(path: &Path, through: JournalCheckpoint) -> Result<bool> {
    let writer_lock = storage_file::open_read_write(&path.with_extension("lock"))?;
    writer_lock.lock()?;
    let mut file = storage_file::open_read_write(path)?;
    file.lock_shared()?;
    let base = JournalBase::read(&mut file)?;
    if through.sequence <= base.checkpoint.sequence {
        return Ok(false);
    }
    let start = base.physical_offset(through.byte_offset)?;
    let length = file.metadata()?.len();
    let Some(tail_bytes) = length.checked_sub(start) else {
        return Err(invalid("journal retirement past end"));
    };
    if tail_bytes > MAX_ROTATION_TAIL_BYTES {
        return Ok(false);
    }
    // Validate both the complete boundary and its sequence, not just a newline.
    let boundary = crate::journal::tail_checkpoint(&mut file, start)?;
    if boundary != through {
        return Err(invalid("journal retirement checkpoint mismatch"));
    }
    let temporary = path.with_extension(format!("rotate-{}", std::process::id()));
    // A previous interrupted attempt is private, unreferenced and replaceable.
    if temporary.exists() {
        std::fs::remove_file(&temporary)?;
    }
    let result = (|| {
        let mut output = storage_file::create_new(&temporary)?;
        let encoded = serde_json::to_vec(&serde_json::json!({
            "journalBase": [through.sequence, through.byte_offset]
        }))?;
        if encoded.len() >= HEADER_BYTES as usize {
            return Err(invalid("journal base exceeds header capacity"));
        }
        let mut header = [b' '; HEADER_BYTES as usize];
        header[..encoded.len()].copy_from_slice(&encoded);
        header[header.len() - 1] = b'\n';
        output.write_all(&header)?;
        file.seek(SeekFrom::Start(start))?;
        let copied = std::io::copy(&mut file.take(tail_bytes), &mut output)?;
        if copied != tail_bytes {
            return Err(invalid("journal changed during rotation"));
        }
        output.sync_all()?;
        drop(output);
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
        Ok(true)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidData(message.into())
}
