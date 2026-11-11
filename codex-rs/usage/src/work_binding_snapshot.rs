//! Cheap reader pins and bounded cached decoding, independent of capture locks.
use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::SystemTime;

use crate::Result;
use crate::StorageError;
use crate::WorkBindingTransition;

const CACHE_BYTES: u64 = 64 * 1024 * 1024;
const CACHE_VIEWS: usize = 4;

#[derive(Debug)]
enum Source {
    Missing,
    File {
        file: File,
        length: u64,
        modified: Option<SystemTime>,
    },
    Unavailable,
}

#[derive(Clone)]
struct Cached {
    path: PathBuf,
    length: u64,
    modified: Option<SystemTime>,
    transitions: Arc<Vec<WorkBindingTransition>>,
}

/// Opening a normal usage reader only pins a file handle and its length.
/// Parsing is deferred until attribution is requested. Appends cannot mutate
/// that prefix, and a later rotation cannot change this open file's identity.
#[derive(Debug)]
pub(crate) struct WorkBindingSnapshot {
    path: PathBuf,
    source: Source,
    loaded: OnceLock<Option<Arc<Vec<WorkBindingTransition>>>>,
}

impl WorkBindingSnapshot {
    pub(crate) fn pin(root: &Path) -> Self {
        let path = root.join("work-bindings.jsonl");
        let source = match File::open(&path) {
            Ok(file) => match file.metadata() {
                Ok(metadata)
                    if metadata.is_file()
                        && crate::storage_file::protect_existing(&path).is_ok() =>
                {
                    Source::File {
                        file,
                        length: metadata.len(),
                        modified: metadata.modified().ok(),
                    }
                }
                _ => Source::Unavailable,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Source::Missing,
            Err(_) => Source::Unavailable,
        };
        Self {
            path,
            source,
            loaded: OnceLock::new(),
        }
    }

    pub(crate) fn transitions(&self) -> &[WorkBindingTransition] {
        self.loaded().map_or(&[], |rows| rows.as_slice())
    }

    pub(crate) fn coverage(&self) -> &'static str {
        match self.loaded() {
            Some(rows) if rows.is_empty() => "unobserved",
            Some(_) => "recorded",
            None => "unavailable",
        }
    }

    fn loaded(&self) -> Option<&Arc<Vec<WorkBindingTransition>>> {
        self.loaded.get_or_init(|| self.load().ok()).as_ref()
    }

    fn load(&self) -> Result<Arc<Vec<WorkBindingTransition>>> {
        static CACHE: OnceLock<Mutex<VecDeque<Cached>>> = OnceLock::new();
        let (file, length, modified) = match &self.source {
            Source::Missing => return Ok(Arc::new(Vec::new())),
            Source::Unavailable => {
                return Err(StorageError::InvalidData(
                    "work binding metadata unavailable".into(),
                ));
            }
            Source::File {
                file,
                length,
                modified,
            } => (file, *length, *modified),
        };
        let cache = CACHE.get_or_init(Mutex::default);
        if let Ok(cache) = cache.lock()
            && let Some(entry) = cache.iter().find(|entry| {
                entry.path == self.path && entry.length == length && entry.modified == modified
            })
        {
            return Ok(Arc::clone(&entry.transitions));
        }
        // No events.lock or publisher lock is held while reading/decoding.
        let (transitions, _) = crate::work_binding::read_prefix(&mut file.try_clone()?, length)?;
        let transitions = Arc::new(transitions);
        if length <= CACHE_BYTES
            && let Ok(mut cache) = cache.lock()
        {
            while cache.len() >= CACHE_VIEWS
                || cache
                    .iter()
                    .map(|entry| entry.length)
                    .sum::<u64>()
                    .saturating_add(length)
                    > CACHE_BYTES
            {
                cache.pop_front();
            }
            cache.push_back(Cached {
                path: self.path.clone(),
                length,
                modified,
                transitions: Arc::clone(&transitions),
            });
        }
        Ok(transitions)
    }
}
