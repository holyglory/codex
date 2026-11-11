//! Bounded append pages and per-scope indexes for sparse work assignments.
//!
//! The active page is append-only and capped; rotation creates a new immutable
//! page without rewriting old scope history. Current state is one small JSON
//! object per thread, while scope indexes contain only page addresses.

use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
use std::fs::TryLockError;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use crate::Result;
use crate::StorageError;
use crate::WorkBindingTransition;
use crate::WorkBindingUpdate;
use crate::storage_file;
use crate::work_binding_index::ActivePage;
use crate::work_binding_index::LEGACY_FILE;
use crate::work_binding_index::MAX_PAGE_BYTES;
use crate::work_binding_index::MAX_PAGE_TRANSITIONS;
use crate::work_binding_index::ScopeIndex;
use crate::work_binding_index::ScopePage;
use crate::work_binding_index::active_path;
use crate::work_binding_index::current_path;
use crate::work_binding_index::ensure_directories;
use crate::work_binding_index::page_file;
use crate::work_binding_index::page_path;
use crate::work_binding_index::read_active;
use crate::work_binding_index::read_current;
use crate::work_binding_index::read_scope;
use crate::work_binding_index::scope_path;
use crate::work_binding_index::store_root;
use crate::work_binding_index::write_atomic;

const MAX_DECODE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
struct PageRead {
    rows: Vec<WorkBindingTransition>,
    complete_bytes: u64,
    total_bytes: u64,
}

/// A pinned lookup view. The active page's byte boundary is captured at open;
/// subsequent appends and rotations cannot change results for this instance.
#[derive(Debug)]
pub(crate) struct WorkBindingLookup {
    root: PathBuf,
    active_file: Option<String>,
    active_generation: u64,
    active_bytes: u64,
    active_rows: Arc<Vec<WorkBindingTransition>>,
    legacy: Option<Arc<Vec<WorkBindingTransition>>>,
}

impl WorkBindingLookup {
    pub(crate) fn open(root: &Path) -> Result<Self> {
        let Some(active) = read_active(root)? else {
            let legacy = legacy_rows(root)?;
            return Ok(Self {
                root: root.to_path_buf(),
                active_file: None,
                active_generation: 0,
                active_bytes: 0,
                active_rows: Arc::new(Vec::new()),
                legacy: legacy.map(Arc::new),
            });
        };
        let path = page_path(root, &active.file)?;
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() || metadata.len() > MAX_DECODE_BYTES {
            return Err(invalid("work binding active page exceeds its bounded size"));
        }
        let bytes = metadata.len();
        let page = read_page(root, &active.file, Some(bytes), /*strict*/ true)?;
        if page.total_bytes != bytes || page.complete_bytes != bytes && !page.rows.is_empty() {
            return Err(invalid("work binding active page changed while opening"));
        }
        Ok(Self {
            root: root.to_path_buf(),
            active_file: Some(active.file),
            active_generation: active.generation,
            active_bytes: bytes,
            active_rows: Arc::new(page.rows),
            legacy: None,
        })
    }

    pub(crate) fn lookup(
        &self,
        thread_id: &str,
        capture_sequence: u64,
    ) -> Result<Option<WorkBindingTransition>> {
        if let Some(rows) = &self.legacy {
            return Ok(rows
                .iter()
                .filter(|row| {
                    row.update.thread_id == thread_id
                        && row.effective_capture_sequence <= capture_sequence
                })
                .max_by_key(|row| row.effective_capture_sequence)
                .cloned());
        }
        let Some(index) = read_scope(&self.root, thread_id)? else {
            return Ok(None);
        };
        for page in index.pages.iter().rev() {
            if page.generation > self.active_generation {
                continue;
            }
            if page.first_sequence > capture_sequence {
                continue;
            }
            let limit = self
                .active_file
                .as_deref()
                .filter(|active| *active == page.file)
                .map(|_| self.active_bytes);
            let rows = if self
                .active_file
                .as_deref()
                .is_some_and(|active| active == page.file)
            {
                self.active_rows.as_ref().clone()
            } else {
                read_page(&self.root, &page.file, limit, /*strict*/ true)?.rows
            };
            if let Some(row) = rows
                .into_iter()
                .filter(|row| {
                    row.update.thread_id == thread_id
                        && row.effective_capture_sequence <= capture_sequence
                })
                .max_by_key(|row| row.effective_capture_sequence)
            {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    pub(crate) fn read_all(&self) -> Result<Vec<WorkBindingTransition>> {
        if let Some(rows) = &self.legacy {
            return Ok((**rows).clone());
        }
        let mut files = Vec::new();
        let legacy = self.root.join(LEGACY_FILE);
        if legacy.is_file() {
            files.push(LEGACY_FILE.to_owned());
        }
        let pages = store_root(&self.root).join(crate::work_binding_index::PAGE_DIRECTORY);
        if pages.is_dir() {
            for entry in fs::read_dir(pages)? {
                let entry = entry?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if name.starts_with("page-") && name.ends_with(".jsonl") {
                    let file = format!("pages/{name}");
                    if crate::work_binding_index::page_generation(&file)
                        .is_some_and(|generation| generation <= self.active_generation)
                    {
                        files.push(file);
                    }
                }
            }
        }
        files.sort();
        let mut rows = Vec::new();
        for file in files {
            let limit = self
                .active_file
                .as_deref()
                .filter(|active| *active == file)
                .map(|_| self.active_bytes);
            if self
                .active_file
                .as_deref()
                .is_some_and(|active| active == file)
            {
                rows.extend(self.active_rows.iter().cloned());
            } else {
                rows.extend(read_page(&self.root, &file, limit, /*strict*/ true)?.rows);
            }
        }
        rows.sort_by_key(|row| row.effective_capture_sequence);
        let mut unique = BTreeMap::new();
        for row in rows {
            let key = (row.update.thread_id.clone(), row.effective_capture_sequence);
            unique.insert(key, row);
        }
        Ok(unique.into_values().collect())
    }
}

/// Remove binding history only after the caller has committed a canonical and
/// retained assignment proof at `coverage_sequence`.
///
/// The proof is deliberately an explicit argument owned by the retention
/// coordinator; this function never infers it from the usage watermark. A
/// busy publisher or pinned reader returns `Ok(false)` so background work can
/// retry without waiting or disturbing the reader's immutable view.
pub(crate) fn prune_before(root: &Path, coverage_sequence: u64) -> Result<bool> {
    if coverage_sequence == 0 {
        return Err(invalid("work binding coverage sequence is zero"));
    }
    let read_lock = storage_file::open_read_write(&root.join("events.lock"))?;
    read_lock.lock_shared()?;
    let Some(active) = read_active(root)? else {
        return Ok(false);
    };
    let active_signature = (
        active.file.clone(),
        fs::metadata(page_path(root, &active.file)?)?.len(),
    );
    let scopes = scope_indexes(root)?;
    let mut candidate = None;
    let mut candidate_first = 0_u64;
    for file in page_files(root)? {
        if file == active.file {
            continue;
        }
        let page = read_page(root, &file, None, /*strict*/ true)?;
        if page.rows.is_empty()
            || page
                .rows
                .iter()
                .any(|row| row.effective_capture_sequence >= coverage_sequence)
        {
            continue;
        }
        let first = page.rows[0].effective_capture_sequence;
        let threads = page
            .rows
            .iter()
            .map(|row| row.update.thread_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let safe = threads.iter().all(|thread| {
            scopes
                .iter()
                .find(|scope| scope.thread_id == *thread)
                .is_some_and(|scope| {
                    scope
                        .pages
                        .iter()
                        .any(|page| page.file == file && page.first_sequence == first)
                        && scope.pages.iter().any(|page| page.first_sequence > first)
                })
        });
        if safe {
            candidate = Some(file);
            candidate_first = first;
            break;
        }
    }
    drop(read_lock);
    let Some(candidate) = candidate else {
        return Ok(false);
    };
    let publisher = storage_file::open_read_write(&root.join(".snapshot.lock"))?;
    match publisher.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    let events = storage_file::open_read_write(&root.join("events.lock"))?;
    match events.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    let Some(current_active) = read_active(root)? else {
        return Ok(false);
    };
    let current_signature = (
        current_active.file.clone(),
        fs::metadata(page_path(root, &current_active.file)?)?.len(),
    );
    if current_signature != active_signature
        || fs::metadata(page_path(root, &candidate)?)?.len() == 0
    {
        return Ok(false);
    }
    let readers = storage_file::open_read_write(&root.join(".readers.lock"))?;
    match readers.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    for mut scope in scopes {
        let mut changed = false;
        scope.pages.retain(|page| {
            let remove = page.file == candidate && page.first_sequence == candidate_first;
            changed |= remove;
            !remove
        });
        if changed {
            scope.validate()?;
            write_atomic(&scope_path(root, &scope.thread_id), &scope)?;
        }
    }
    fs::remove_file(page_path(root, &candidate)?)?;
    Ok(true)
}

fn scope_indexes(root: &Path) -> Result<Vec<ScopeIndex>> {
    let directory = store_root(root).join(crate::work_binding_index::SCOPE_DIRECTORY);
    let mut scopes = Vec::new();
    if !directory.is_dir() {
        return Ok(scopes);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(scope) = crate::work_binding_index::read_json(&path)? else {
            continue;
        };
        scope.validate()?;
        scopes.push(scope);
    }
    Ok(scopes)
}

/// Append one transition while the caller owns `events.lock`.
pub(crate) fn append(
    root: &Path,
    update: WorkBindingUpdate,
    effective_capture_sequence: u64,
    captured_at_ms: i64,
) -> Result<WorkBindingTransition> {
    ensure_directories(root)?;
    let active = initialize(root)?;
    let active_path_now = page_path(root, &active.file)?;
    let actual_bytes = fs::metadata(&active_path_now)?.len();
    if actual_bytes != active.bytes {
        recover_active(root, &active)?;
    }
    if let Some(previous) = read_current(root, &update.thread_id)? {
        if previous.update == update {
            return Ok(previous);
        }
        if update.transition_sequence <= previous.update.transition_sequence {
            return Err(invalid("stale or conflicting work binding transition"));
        }
    }
    let transition = WorkBindingTransition {
        update,
        effective_capture_sequence,
        captured_at_ms,
    };
    crate::work_binding::validate_transition(&transition)?;
    let mut active =
        read_active(root)?.ok_or_else(|| invalid("work binding active page missing"))?;
    let page = read_page(root, &active.file, None, /*strict*/ true)?;
    let encoded = line(&transition)?;
    if page.rows.len() >= MAX_PAGE_TRANSITIONS
        || page.total_bytes.saturating_add(encoded.len() as u64) > MAX_PAGE_BYTES
    {
        active.generation = active.generation.saturating_add(1);
        active.first_sequence = effective_capture_sequence;
        active.file = page_file(active.generation);
        active.transition_count = 0;
        active.bytes = 0;
        active.last_sequence = 0;
        let path = page_path(root, &active.file)?;
        storage_file::create_new(&path)?.sync_all()?;
        write_atomic(&active_path(root), &active)?;
    }
    let path = page_path(root, &active.file)?;
    let mut output = storage_file::open_append(&path)?;
    output.write_all(&encoded)?;
    output.sync_all()?;
    write_current(root, &transition)?;
    update_scope(root, &transition, &active)?;
    active.transition_count = active.transition_count.saturating_add(1);
    active.bytes = active.bytes.saturating_add(encoded.len() as u64);
    active.last_sequence = effective_capture_sequence;
    write_atomic(&active_path(root), &active)?;
    Ok(transition)
}

pub(crate) fn read_current(root: &Path, thread_id: &str) -> Result<Option<WorkBindingTransition>> {
    if read_active(root)?.is_some() {
        // Status/CAS must not turn a damaged scope index into an implicit clear.
        let scope = read_scope(root, thread_id)?;
        if let Some(current) = crate::work_binding_index::read_current(root, thread_id)? {
            if scope.is_none() {
                return Err(invalid("work binding current state has no scope index"));
            }
            return Ok(Some(current));
        }
        return WorkBindingLookup::open(root)?.lookup(thread_id, u64::MAX);
    }
    Ok(legacy_rows(root)?.and_then(|rows| {
        rows.into_iter()
            .filter(|row| row.update.thread_id == thread_id)
            .max_by_key(|row| row.effective_capture_sequence)
    }))
}

fn initialize(root: &Path) -> Result<ActivePage> {
    if let Some(active) = read_active(root)? {
        let _ = page_path(root, &active.file)?;
        return Ok(active);
    }
    let legacy = legacy_rows(root)?;
    if let Some(rows) = legacy {
        let old_page = ScopePage {
            file: LEGACY_FILE.to_owned(),
            first_sequence: rows.first().map_or(1, |row| row.effective_capture_sequence),
            generation: 0,
        };
        for row in &rows {
            write_current(root, row)?;
            let mut scope = read_scope(root, &row.update.thread_id)?
                .unwrap_or_else(|| ScopeIndex::empty(&row.update.thread_id));
            scope.add_page(old_page.clone())?;
            scope.latest_revision = scope.latest_revision.max(row.update.transition_sequence);
            scope.latest_effective_sequence = scope
                .latest_effective_sequence
                .max(row.effective_capture_sequence);
            write_atomic(&scope_path(root, &row.update.thread_id), &scope)?;
        }
        let next_generation = 1;
        let active = ActivePage {
            version: 1,
            file: page_file(next_generation),
            first_sequence: rows
                .last()
                .map_or(1, |row| row.effective_capture_sequence.saturating_add(1)),
            generation: next_generation,
            transition_count: 0,
            bytes: 0,
            last_sequence: 0,
        };
        let path = page_path(root, &active.file)?;
        storage_file::create_new(&path)?.sync_all()?;
        write_atomic(&active_path(root), &active)?;
        return Ok(active);
    }
    let active = ActivePage {
        version: 1,
        file: LEGACY_FILE.to_owned(),
        first_sequence: 1,
        generation: 0,
        transition_count: 0,
        bytes: 0,
        last_sequence: 0,
    };
    let path = page_path(root, &active.file)?;
    if !path.exists() {
        storage_file::open_append(&path)?.sync_all()?;
    }
    write_atomic(&active_path(root), &active)?;
    Ok(active)
}

fn recover_active(root: &Path, active: &ActivePage) -> Result<()> {
    let path = page_path(root, &active.file)?;
    let page = read_page(root, &active.file, None, /*strict*/ false)?;
    if page.complete_bytes != page.total_bytes {
        let file = storage_file::open_read_write(&path)?;
        file.set_len(page.complete_bytes)?;
        file.sync_all()?;
    }
    for row in &page.rows {
        write_current_if_newer(root, row)?;
        let mut scope = read_scope(root, &row.update.thread_id)?
            .unwrap_or_else(|| ScopeIndex::empty(&row.update.thread_id));
        scope.add_page(ScopePage {
            file: active.file.clone(),
            first_sequence: page
                .rows
                .first()
                .map_or(active.first_sequence, |row| row.effective_capture_sequence),
            generation: active.generation,
        })?;
        scope.latest_revision = scope.latest_revision.max(row.update.transition_sequence);
        scope.latest_effective_sequence = scope
            .latest_effective_sequence
            .max(row.effective_capture_sequence);
        write_atomic(&scope_path(root, &row.update.thread_id), &scope)?;
    }
    let mut corrected = active.clone();
    if let Some(first) = page.rows.first() {
        corrected.first_sequence = first.effective_capture_sequence;
    }
    corrected.transition_count = page.rows.len();
    corrected.bytes = page.complete_bytes;
    corrected.last_sequence = page
        .rows
        .last()
        .map_or(0, |row| row.effective_capture_sequence);
    if corrected != *active {
        write_atomic(&active_path(root), &corrected)?;
    }
    Ok(())
}

fn update_scope(
    root: &Path,
    transition: &WorkBindingTransition,
    active: &ActivePage,
) -> Result<()> {
    let mut scope = read_scope(root, &transition.update.thread_id)?
        .unwrap_or_else(|| ScopeIndex::empty(&transition.update.thread_id));
    scope.add_page(ScopePage {
        file: active.file.clone(),
        first_sequence: active.first_sequence,
        generation: active.generation,
    })?;
    scope.latest_revision = transition.update.transition_sequence;
    scope.latest_effective_sequence = transition.effective_capture_sequence;
    write_atomic(&scope_path(root, &transition.update.thread_id), &scope)
}

fn write_current(root: &Path, transition: &WorkBindingTransition) -> Result<()> {
    write_atomic(
        &current_path(root, &transition.update.thread_id),
        transition,
    )
}

fn write_current_if_newer(root: &Path, transition: &WorkBindingTransition) -> Result<()> {
    if read_current(root, &transition.update.thread_id)?.is_some_and(|current| {
        current.effective_capture_sequence >= transition.effective_capture_sequence
    }) {
        return Ok(());
    }
    write_current(root, transition)
}

fn legacy_rows(root: &Path) -> Result<Option<Vec<WorkBindingTransition>>> {
    let path = root.join(LEGACY_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    let (rows, _) = crate::work_binding::read_prefix(&mut file.try_clone()?, length)?;
    Ok(Some(rows))
}

fn read_page(root: &Path, file: &str, limit: Option<u64>, strict: bool) -> Result<PageRead> {
    let path = page_path(root, file)?;
    let mut input = File::open(&path)?;
    let metadata = input.metadata()?;
    let total_bytes = metadata.len();
    let bytes = limit.unwrap_or(total_bytes);
    if bytes > total_bytes || bytes > MAX_DECODE_BYTES {
        return Err(invalid("work binding page exceeds its bounded read"));
    }
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(bytes as usize)
        .map_err(|_| invalid("work binding page allocation exceeds its bounded read"))?;
    encoded.resize(bytes as usize, 0);
    input.read_exact(&mut encoded)?;
    let complete = encoded
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1) as u64;
    if strict && complete != bytes {
        return Err(invalid("work binding page has an incomplete final record"));
    }
    let mut rows = Vec::new();
    let mut previous = 0;
    for line in encoded[..complete as usize]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let row: WorkBindingTransition = serde_json::from_slice(line)?;
        crate::work_binding::validate_transition(&row)?;
        if row.effective_capture_sequence <= previous {
            return Err(invalid("work binding page sequence is not increasing"));
        }
        previous = row.effective_capture_sequence;
        rows.push(row);
    }
    if rows.len() > MAX_PAGE_TRANSITIONS {
        return Err(invalid("work binding page has too many transitions"));
    }
    Ok(PageRead {
        rows,
        complete_bytes: complete,
        total_bytes,
    })
}

fn line(transition: &WorkBindingTransition) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(transition)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_PAGE_BYTES as usize {
        return Err(invalid("work binding transition exceeds page size"));
    }
    Ok(bytes)
}

fn write_page(root: &Path, file: &str, rows: &[WorkBindingTransition]) -> Result<()> {
    let path = page_path(root, file)?;
    let mut output = storage_file::create_new(&path)?;
    for row in rows {
        output.write_all(&line(row)?)?;
    }
    output.sync_all()?;
    Ok(())
}

fn page_files(root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let legacy = root.join(LEGACY_FILE);
    if legacy.is_file() {
        files.push(LEGACY_FILE.to_owned());
    }
    let pages = store_root(root).join(crate::work_binding_index::PAGE_DIRECTORY);
    if pages.is_dir() {
        for entry in fs::read_dir(pages)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let file = format!("pages/{name}");
            if crate::work_binding_index::valid_file_name(&file) {
                files.push(file);
            }
        }
    }
    Ok(files)
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidData(message.into())
}

#[cfg(test)]
#[path = "work_binding_store_tests.rs"]
mod tests;
