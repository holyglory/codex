//! Bounded process-local cache for repeated summary requests.
//!
//! Entries are immutable snapshots keyed by the manifest generation and the
//! journal frontier. A publisher advance therefore creates a new key without
//! needing to synchronously walk or invalidate older entries; the bounded LRU
//! keeps week-long Codex processes from accumulating unbounded report data.
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde_json::Value;

use crate::TimeRange;
use crate::UsageFilter;

const MAX_ENTRIES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Key {
    root: PathBuf,
    snapshot_generation: u64,
    watermark: u64,
    journal_watermark: u64,
    pending_entries: u64,
    pending_corrections: usize,
    start_ms: i64,
    end_ms: i64,
    account_id: Option<String>,
    thread_id: Option<String>,
    thread_ids: Option<BTreeSet<String>>,
    repository_id: Option<String>,
    model: Option<String>,
}

#[derive(Default)]
struct Cache {
    entries: Vec<(Key, Value)>,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

#[allow(clippy::too_many_arguments)]
pub(crate) fn get(
    root: PathBuf,
    snapshot_generation: u64,
    watermark: u64,
    journal_watermark: u64,
    pending_entries: u64,
    pending_corrections: usize,
    range: TimeRange,
    filter: &UsageFilter,
) -> Option<Value> {
    let key = Key::new(
        root,
        snapshot_generation,
        watermark,
        journal_watermark,
        pending_entries,
        pending_corrections,
        range,
        filter,
    );
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(Cache::default()))
        .lock()
        .ok()?;
    let index = cache.entries.iter().position(|(entry, _)| entry == &key)?;
    let entry = cache.entries.remove(index);
    let value = entry.1.clone();
    cache.entries.push(entry);
    Some(value)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn insert(
    root: PathBuf,
    snapshot_generation: u64,
    watermark: u64,
    journal_watermark: u64,
    pending_entries: u64,
    pending_corrections: usize,
    range: TimeRange,
    filter: &UsageFilter,
    value: Value,
) {
    let key = Key::new(
        root,
        snapshot_generation,
        watermark,
        journal_watermark,
        pending_entries,
        pending_corrections,
        range,
        filter,
    );
    let Ok(mut cache) = CACHE.get_or_init(|| Mutex::new(Cache::default())).lock() else {
        return;
    };
    if let Some(index) = cache.entries.iter().position(|(entry, _)| entry == &key) {
        cache.entries.remove(index);
    }
    cache.entries.push((key, value));
    if cache.entries.len() > MAX_ENTRIES {
        cache.entries.remove(0);
    }
}

impl Key {
    #[allow(clippy::too_many_arguments)]
    fn new(
        root: PathBuf,
        snapshot_generation: u64,
        watermark: u64,
        journal_watermark: u64,
        pending_entries: u64,
        pending_corrections: usize,
        range: TimeRange,
        filter: &UsageFilter,
    ) -> Self {
        Self {
            root,
            snapshot_generation,
            watermark,
            journal_watermark,
            pending_entries,
            pending_corrections,
            start_ms: range.start_ms(),
            end_ms: range.end_ms(),
            account_id: filter.account_id.clone(),
            thread_id: filter.thread_id.clone(),
            thread_ids: filter.thread_ids.clone(),
            repository_id: filter.repository_id.clone(),
            model: filter.model.clone(),
        }
    }
}
