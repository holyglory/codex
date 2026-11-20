//! Small per-scope metadata for the sparse work-binding pages.
//!
//! The index stores page addresses, not transition history. A capture updates
//! one scope's file and the current pointer for that scope. Lookup can then
//! choose one bounded page instead of scanning a global transition journal.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::Result;
use crate::StorageError;
use crate::WorkBindingTransition;
use crate::storage_file;

pub(crate) const STORE_DIRECTORY: &str = "work-binding-store";
pub(crate) const CURRENT_DIRECTORY: &str = "current";
pub(crate) const SCOPE_DIRECTORY: &str = "scopes";
pub(crate) const PAGE_DIRECTORY: &str = "pages";
pub(crate) const ACTIVE_FILE: &str = "active.json";
pub(crate) const LEGACY_FILE: &str = "work-bindings.jsonl";
pub(crate) const MAX_PAGE_TRANSITIONS: usize = 256;
pub(crate) const MAX_PAGE_BYTES: u64 = 1024 * 1024;
const INDEX_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActivePage {
    pub version: u32,
    pub file: String,
    pub first_sequence: u64,
    pub generation: u64,
    #[serde(default)]
    pub transition_count: usize,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub last_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScopePage {
    pub file: String,
    pub first_sequence: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScopeIndex {
    pub version: u32,
    pub thread_id: String,
    pub pages: Vec<ScopePage>,
    pub latest_revision: u64,
    pub latest_effective_sequence: u64,
}

impl ScopeIndex {
    pub(crate) fn empty(thread_id: &str) -> Self {
        Self {
            version: INDEX_VERSION,
            thread_id: thread_id.to_owned(),
            pages: Vec::new(),
            latest_revision: 0,
            latest_effective_sequence: 0,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != INDEX_VERSION
            || self.thread_id.is_empty()
            || self.pages.windows(2).any(|pages| {
                pages[0].first_sequence > pages[1].first_sequence
                    || pages[0].generation >= pages[1].generation
            })
            || self
                .pages
                .iter()
                .any(|page| page.first_sequence == 0 || !valid_file_name(&page.file))
        {
            return Err(invalid("invalid work binding scope index"));
        }
        Ok(())
    }

    pub(crate) fn add_page(&mut self, page: ScopePage) -> Result<bool> {
        self.validate()?;
        if self.pages.iter().any(|existing| existing.file == page.file) {
            return Ok(false);
        }
        if self
            .pages
            .last()
            .is_some_and(|existing| existing.first_sequence > page.first_sequence)
        {
            return Err(invalid("work binding pages are out of order"));
        }
        self.pages.push(page);
        Ok(true)
    }
}

pub(crate) fn store_root(root: &Path) -> PathBuf {
    root.join(STORE_DIRECTORY)
}

pub(crate) fn current_path(root: &Path, thread_id: &str) -> PathBuf {
    store_root(root)
        .join(CURRENT_DIRECTORY)
        .join(format!("{}.json", scope_file_key(thread_id)))
}

pub(crate) fn scope_path(root: &Path, thread_id: &str) -> PathBuf {
    store_root(root)
        .join(SCOPE_DIRECTORY)
        .join(format!("{}.json", scope_file_key(thread_id)))
}

pub(crate) fn active_path(root: &Path) -> PathBuf {
    store_root(root).join(ACTIVE_FILE)
}

pub(crate) fn page_path(root: &Path, file: &str) -> Result<PathBuf> {
    if !valid_file_name(file) {
        return Err(invalid("invalid work binding page path"));
    }
    let path = if file == LEGACY_FILE {
        root.join(file)
    } else {
        store_root(root).join(file)
    };
    Ok(path)
}

pub(crate) fn scope_file_key(thread_id: &str) -> String {
    thread_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn page_file(generation: u64) -> String {
    format!("{PAGE_DIRECTORY}/page-{generation:020}.jsonl")
}

pub(crate) fn page_generation(file: &str) -> Option<u64> {
    let name = file.strip_prefix("pages/page-")?.strip_suffix(".jsonl")?;
    (name.len() == 20 && name.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| name.parse().ok())
        .flatten()
}

pub(crate) fn ensure_directories(root: &Path) -> Result<()> {
    let store = store_root(root);
    storage_file::ensure_usage_directory(root)?;
    fs::create_dir_all(store.join(CURRENT_DIRECTORY))?;
    fs::create_dir_all(store.join(SCOPE_DIRECTORY))?;
    fs::create_dir_all(store.join(PAGE_DIRECTORY))?;
    Ok(())
}

pub(crate) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn read_active(root: &Path) -> Result<Option<ActivePage>> {
    let active: Option<ActivePage> = read_json(&active_path(root))?;
    if let Some(active) = &active
        && (active.version != INDEX_VERSION
            || active.first_sequence == 0
            || !valid_file_name(&active.file))
    {
        return Err(invalid("invalid work binding active page"));
    }
    Ok(active)
}

pub(crate) fn read_scope(root: &Path, thread_id: &str) -> Result<Option<ScopeIndex>> {
    let index: Option<ScopeIndex> = read_json(&scope_path(root, thread_id))?;
    if let Some(index) = &index {
        if index.thread_id != thread_id {
            return Err(invalid("work binding scope index identity mismatch"));
        }
        index.validate()?;
    }
    Ok(index)
}

pub(crate) fn read_current(root: &Path, thread_id: &str) -> Result<Option<WorkBindingTransition>> {
    let path = current_path(root, thread_id);
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 128 * 1024) {
        return Err(invalid(
            "work binding current state exceeds its bounded size",
        ));
    }
    let current: Option<WorkBindingTransition> = read_json(&path)?;
    if let Some(current) = &current {
        if current.update.thread_id != thread_id {
            return Err(invalid("work binding current identity mismatch"));
        }
        crate::work_binding::validate_update(&current.update)?;
        if current.effective_capture_sequence == 0 {
            return Err(invalid("work binding current sequence is zero"));
        }
    }
    Ok(current)
}

pub(crate) fn write_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| invalid("work binding metadata has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("tmp-{}-{}", std::process::id(), unix_now_nanos()));
    let mut file = storage_file::create_new(&temporary)?;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    std::io::Write::write_all(&mut file, &bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) fn valid_file_name(file: &str) -> bool {
    if file == LEGACY_FILE {
        return true;
    }
    let Some(name) = file.strip_prefix("pages/page-") else {
        return false;
    };
    name.strip_suffix(".jsonl").is_some_and(|digits| {
        digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn unix_now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidData(message.into())
}
