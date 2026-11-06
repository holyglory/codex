//! Framed immutable record-id run I/O.
use std::fs::File;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use crate::Result;
use crate::StorageError;
use crate::id_index::ID_FENCE_STRIDE;
use crate::id_index::IdRunDescriptor;
use crate::id_index::MAX_ID_ENTRY_BYTES;
use crate::id_index::RecordIdEntry;
use crate::id_index::data_path;
use crate::id_index::id_hash;
use crate::storage_file;

const DATA_MAGIC: &[u8; 8] = b"CIDX1RUN";
const FENCE_MAGIC: &[u8; 8] = b"CIDX1FEN";
const FRAME_HEADER_BYTES: usize = 4;

pub(crate) struct RunWriter {
    data_path: PathBuf,
    fence_path: PathBuf,
    data: File,
    fence: File,
    level: u8,
    count: u64,
    fences: u64,
    min_hash: u64,
    max_hash: u64,
    min_sequence: u64,
    max_sequence: u64,
}

impl RunWriter {
    pub(crate) fn new(root: &Path, data_file: &str, fence_file: &str, level: u8) -> Result<Self> {
        let data_file_path = data_path(root, data_file)?;
        let fence_file_path = data_path(root, fence_file)?;
        let mut data = storage_file::create_new(&data_file_path)?;
        let mut fence = storage_file::create_new(&fence_file_path)?;
        data.write_all(DATA_MAGIC)?;
        fence.write_all(FENCE_MAGIC)?;
        Ok(Self {
            data_path: data_file_path,
            fence_path: fence_file_path,
            data,
            fence,
            level,
            count: 0,
            fences: 0,
            min_hash: u64::MAX,
            max_hash: 0,
            min_sequence: u64::MAX,
            max_sequence: 0,
        })
    }

    pub(crate) fn push(&mut self, entry: &RecordIdEntry) -> Result<()> {
        self.push_with_hash(entry, id_hash(&entry.id))
    }

    #[cfg(test)]
    pub(crate) fn push_with_test_hash(&mut self, entry: &RecordIdEntry, hash: u64) -> Result<()> {
        self.push_with_hash(entry, hash)
    }

    fn push_with_hash(&mut self, entry: &RecordIdEntry, hash: u64) -> Result<()> {
        let encoded = serde_json::to_vec(entry)?;
        if encoded.is_empty()
            || encoded.len() > MAX_ID_ENTRY_BYTES
            || encoded.len() > u32::MAX as usize
        {
            return Err(invalid("record-id index entry exceeds its size limit"));
        }
        if self.count.is_multiple_of(u64::from(ID_FENCE_STRIDE)) {
            self.fence.write_all(&hash.to_le_bytes())?;
            self.fence
                .write_all(&self.data.stream_position()?.to_le_bytes())?;
            self.fences += 1;
        }
        self.data.write_all(&(encoded.len() as u32).to_le_bytes())?;
        self.data.write_all(&encoded)?;
        self.count += 1;
        self.min_hash = self.min_hash.min(hash);
        self.max_hash = self.max_hash.max(hash);
        self.min_sequence = self.min_sequence.min(entry.first_sequence);
        self.max_sequence = self.max_sequence.max(entry.first_sequence);
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<IdRunDescriptor> {
        self.data.flush()?;
        self.fence.flush()?;
        self.data.sync_all()?;
        self.fence.sync_all()?;
        drop(self.data);
        drop(self.fence);
        storage_file::finish_generated(&self.data_path)?;
        storage_file::finish_generated(&self.fence_path)?;
        sync_parent(&self.data_path)?;
        let data_file = self
            .data_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("record-id run path has no file name"))?
            .to_string();
        let fence_file = self
            .fence_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("record-id fence path has no file name"))?
            .to_string();
        let data_bytes = std::fs::metadata(&self.data_path)?.len();
        Ok(IdRunDescriptor {
            data_file,
            fence_file,
            level: self.level,
            entry_count: self.count,
            fence_count: self.fences,
            min_hash: self.min_hash,
            max_hash: self.max_hash,
            min_sequence: self.min_sequence,
            max_sequence: self.max_sequence,
            data_bytes,
        })
    }
}

pub(crate) struct RunReader {
    file: File,
}

impl RunReader {
    pub(crate) fn open(root: &Path, run: &IdRunDescriptor) -> Result<Self> {
        let path = data_path(root, &run.data_file)?;
        storage_file::protect_existing(&path)?;
        let mut file = File::open(path)?;
        let mut magic = [0; 8];
        file.read_exact(&mut magic)?;
        if &magic != DATA_MAGIC {
            return Err(invalid("record-id run has an invalid header"));
        }
        Ok(Self { file })
    }

    pub(crate) fn next(&mut self) -> Result<Option<RecordIdEntry>> {
        next_frame(&mut self.file)
    }
}

pub(crate) fn next_frame(file: &mut File) -> Result<Option<RecordIdEntry>> {
    let mut length = [0; FRAME_HEADER_BYTES];
    let start = file.stream_position()?;
    match file.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
            let length = file.metadata()?.len();
            if start == length {
                return Ok(None);
            }
            return Err(invalid("record-id run has a truncated frame"));
        }
        Err(error) => return Err(StorageError::Io(error)),
    }
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_ID_ENTRY_BYTES {
        return Err(invalid("record-id run frame exceeds its size limit"));
    }
    let mut encoded = vec![0; length];
    file.read_exact(&mut encoded)?;
    Ok(Some(serde_json::from_slice(&encoded)?))
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidData(message.into())
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}
