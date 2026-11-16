//! Reuse parsed immutable metadata across the lifetime of the analytics worker.
//! File identity is checked on every read, including replacements that reset a
//! store's generation. No file descriptors or query connections stay cached.
use std::collections::VecDeque;
use std::fs::Metadata;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use serde::de::DeserializeOwned;

use crate::Result;
use crate::storage_file;

const MAX_ENTRIES: usize = 4;
const MAX_ENCODED_BYTES: usize = 32 * 1024 * 1024;
static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(crate) struct Parsed<T> {
    pub value: Arc<T>,
    pub revision: u64,
}

impl<T> Clone for Parsed<T> {
    fn clone(&self) -> Self {
        Self {
            value: Arc::clone(&self.value),
            revision: self.revision,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Identity {
    pub(crate) bytes: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64),
}

impl Identity {
    pub(crate) fn from_metadata(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            bytes: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            // Permission enforcement updates ctime on every read; it does not
            // change immutable contents. Inode + mtime identify replacements.
            inode: (metadata.dev(), metadata.ino()),
        }
    }
}

struct Entry<T> {
    path: PathBuf,
    identity: Identity,
    encoded_bytes: usize,
    parsed: Parsed<T>,
}

pub(crate) struct JsonCache<T> {
    entries: Mutex<VecDeque<Entry<T>>>,
    max_entries: usize,
    max_encoded_bytes: usize,
}

impl<T: DeserializeOwned> JsonCache<T> {
    pub const fn new() -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            max_entries: MAX_ENTRIES,
            max_encoded_bytes: MAX_ENCODED_BYTES,
        }
    }

    /// Used only for the canonical manifest and review descriptor: one entry
    /// each, at most 128 MiB encoded across those two caches. Caller-owned pins
    /// have their separate lifetime and admission bounds.
    pub const fn for_snapshot_metadata() -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            max_entries: 1,
            max_encoded_bytes: 64 * 1024 * 1024,
        }
    }

    pub fn load(&self, path: &Path) -> Result<Parsed<T>> {
        storage_file::protect_existing(path)?;
        let identity = Identity::from_metadata(&std::fs::metadata(path)?);
        if let Ok(mut entries) = self.entries.lock()
            && let Some(index) = entries
                .iter()
                .position(|entry| entry.path == path && entry.identity == identity)
            && let Some(entry) = entries.remove(index)
        {
            let parsed = entry.parsed.clone();
            entries.push_back(entry);
            return Ok(parsed);
        }
        // Decode outside the mutex so one cold file never serializes unrelated
        // report readers. Capture and publication never acquire this mutex.
        let file = std::fs::File::open(path)?;
        let identity = Identity::from_metadata(&file.metadata()?);
        let mut contents = Vec::new();
        std::io::Read::read_to_end(&mut &file, &mut contents)?;
        let value = serde_json::from_slice(&contents)?;
        let parsed = Parsed {
            value: Arc::new(value),
            revision: NEXT_REVISION.fetch_add(1, Ordering::Relaxed),
        };
        if identity != Identity::from_metadata(&file.metadata()?) {
            return Err(crate::StorageError::InvalidData(
                "usage metadata changed while decoding".into(),
            ));
        }
        if identity.bytes <= self.max_encoded_bytes as u64
            && let Ok(mut entries) = self.entries.lock()
        {
            entries.retain(|entry| entry.path != path);
            entries.push_back(Entry {
                path: path.to_path_buf(),
                encoded_bytes: identity.bytes as usize,
                identity,
                parsed: parsed.clone(),
            });
            while entries.len() > self.max_entries
                || entries
                    .iter()
                    .map(|entry| entry.encoded_bytes)
                    .sum::<usize>()
                    > self.max_encoded_bytes
            {
                entries.pop_front();
            }
        }
        Ok(parsed)
    }
}

#[cfg(test)]
#[path = "metadata_cache_tests.rs"]
mod tests;
