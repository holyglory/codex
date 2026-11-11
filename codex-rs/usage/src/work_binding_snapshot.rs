//! Cheap reader pins and bounded cached decoding, independent of capture locks.
#[cfg(unix)]
use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;
use std::sync::OnceLock;

use crate::Result;
use crate::StorageError;
use crate::WorkBindingTransition;
use crate::metadata_cache::Identity;

#[cfg(unix)]
const CACHE_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(unix)]
const CACHE_VIEWS: usize = 4;

#[derive(Debug)]
enum Source {
    Missing,
    File { file: File, identity: Identity },
    Unavailable,
}

#[cfg(unix)]
struct Cached {
    path: PathBuf,
    identity: Identity,
    estimated_bytes: usize,
    transitions: Arc<Vec<WorkBindingTransition>>,
}

/// Opening a normal usage reader only pins a file handle and its length.
/// Parsing is deferred until attribution is requested. Appends cannot mutate
/// that prefix, and a later rotation cannot change this open file's identity.
#[derive(Debug)]
pub(crate) struct WorkBindingSnapshot {
    #[cfg(unix)]
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
                        identity: Identity::from_metadata(&metadata),
                    }
                }
                _ => Source::Unavailable,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Source::Missing,
            Err(_) => Source::Unavailable,
        };
        Self {
            #[cfg(unix)]
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
        #[cfg(unix)]
        static CACHE: OnceLock<Mutex<VecDeque<Cached>>> = OnceLock::new();
        let (file, identity) = match &self.source {
            Source::Missing => return Ok(Arc::new(Vec::new())),
            Source::Unavailable => {
                return Err(StorageError::InvalidData(
                    "work binding metadata unavailable".into(),
                ));
            }
            Source::File { file, identity } => (file, identity),
        };
        #[cfg(unix)]
        let cache = CACHE.get_or_init(Mutex::default);
        #[cfg(unix)]
        if let Ok(cache) = cache.lock()
            && let Some(entry) = cache
                .iter()
                .find(|entry| entry.path == self.path && entry.identity == *identity)
        {
            return Ok(Arc::clone(&entry.transitions));
        }
        // No events.lock or publisher lock is held while reading/decoding.
        let (transitions, _) =
            crate::work_binding::read_prefix(&mut file.try_clone()?, identity.bytes)?;
        let transitions = Arc::new(transitions);
        // Non-Unix std metadata has no stable file identifier. Its reader-local
        // OnceLock remains safe; cross-reader caching waits for indexed epochs.
        #[cfg(unix)]
        let estimated_bytes =
            retained_bytes(&transitions).saturating_add(self.path.as_os_str().len());
        #[cfg(unix)]
        if estimated_bytes <= CACHE_BYTES as usize
            && let Ok(mut cache) = cache.lock()
        {
            while cache.len() >= CACHE_VIEWS
                || cache
                    .iter()
                    .map(|entry| entry.estimated_bytes)
                    .sum::<usize>()
                    .saturating_add(estimated_bytes)
                    > CACHE_BYTES as usize
            {
                cache.pop_front();
            }
            cache.push_back(Cached {
                path: self.path.clone(),
                identity: identity.clone(),
                estimated_bytes,
                transitions: Arc::clone(&transitions),
            });
        }
        Ok(transitions)
    }
}

/// Capacity-based payload estimate, including a 32-byte allowance for each
/// string allocation. The cache owns at most 64 MiB by this estimate and four
/// decoded views; active reader pins and transient decode buffers are separate.
#[cfg(unix)]
fn retained_bytes(rows: &Vec<WorkBindingTransition>) -> usize {
    rows.capacity()
        .saturating_mul(std::mem::size_of::<WorkBindingTransition>())
        .saturating_add(
            rows.iter()
                .map(|row| {
                    let update = &row.update;
                    [
                        Some(&update.thread_id),
                        update.native_project_id.as_ref(),
                        update.workstream_id.as_ref(),
                        update.outcome_id.as_ref(),
                        Some(&update.source),
                        Some(&update.authority),
                    ]
                    .into_iter()
                    .flatten()
                    .map(|value| value.capacity().saturating_add(32))
                    .sum::<usize>()
                })
                .sum::<usize>(),
        )
}
