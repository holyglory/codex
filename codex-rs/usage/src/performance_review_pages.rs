//! Short-lived, bounded derived snapshots keep every page on one SQLite read view.
//! Restart or eviction expires the cursor rather than silently mixing measurements.
use super::PerformanceReviewPacket;
use super::PerformanceReviewQuery;
use crate::UsageStore;
use crate::UsageStoreError;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
// Preserve the established 12 KiB packet budget, including reporting disclosure.
const MAX_PAGE_BYTES: usize = 12 * 1024 - 512;
const MAX_SNAPSHOTS: usize = 32;
const LIFETIME: Duration = Duration::from_secs(300);

struct Snapshot {
    scope: [u8; 32],
    packet: Arc<PerformanceReviewPacket>,
    created: Instant,
    bytes: usize,
}

static SNAPSHOTS: OnceLock<Mutex<BTreeMap<String, Snapshot>>> = OnceLock::new();

pub(super) fn existing(
    store: &UsageStore,
    query: &PerformanceReviewQuery,
) -> Result<Option<PerformanceReviewPacket>, UsageStoreError> {
    if !(1..=50).contains(&query.outcome_limit.unwrap_or(8)) {
        return Err(UsageStoreError::InvalidFact);
    }
    let Some(cursor) = &query.outcome_cursor else {
        return Ok(None);
    };
    if cursor.len() > 64 {
        return Err(UsageStoreError::InvalidReviewCursor);
    }
    let (id, offset) = cursor
        .split_once(':')
        .ok_or(UsageStoreError::InvalidReviewCursor)?;
    let offset = offset
        .parse::<usize>()
        .map_err(|_| UsageStoreError::InvalidReviewCursor)?;
    let snapshots = SNAPSHOTS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let snapshot = snapshots
        .get(id)
        .ok_or(UsageStoreError::InvalidReviewCursor)?;
    if snapshot.created.elapsed() >= LIFETIME || snapshot.scope != scope(store, query) {
        return Err(UsageStoreError::InvalidReviewCursor);
    }
    page(
        &snapshot.packet,
        id,
        offset,
        query.outcome_limit.unwrap_or(8) as usize,
    )
    .map(Some)
}

pub(super) fn remember(
    store: &UsageStore,
    query: &PerformanceReviewQuery,
    packet: PerformanceReviewPacket,
) -> Result<PerformanceReviewPacket, UsageStoreError> {
    let bytes = serde_json::to_vec(&packet)
        .map_err(|_| UsageStoreError::InvalidFact)?
        .len();
    if bytes > MAX_SNAPSHOT_BYTES {
        return Err(UsageStoreError::TaskTreeTooLarge);
    }
    let id = uuid::Uuid::now_v7().to_string();
    let result = page(&packet, &id, 0, query.outcome_limit.unwrap_or(8) as usize)?;
    if result.outcomes.next_cursor.is_none() {
        return Ok(result);
    }
    let mut snapshots = SNAPSHOTS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    snapshots.retain(|_, entry| entry.created.elapsed() < LIFETIME);
    while snapshots.len() >= MAX_SNAPSHOTS
        || snapshots.values().map(|entry| entry.bytes).sum::<usize>() + bytes > MAX_CACHE_BYTES
    {
        let oldest = snapshots
            .iter()
            .min_by_key(|(_, entry)| entry.created)
            .map(|(id, _)| id.clone());
        if let Some(oldest) = oldest {
            snapshots.remove(&oldest);
        } else {
            break;
        }
    }
    snapshots.insert(
        id,
        Snapshot {
            scope: scope(store, query),
            packet: Arc::new(packet),
            created: Instant::now(),
            bytes,
        },
    );
    Ok(result)
}

fn scope(store: &UsageStore, query: &PerformanceReviewQuery) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(
        store
            .pool
            .connect_options()
            .get_filename()
            .as_os_str()
            .as_encoded_bytes(),
    );
    hash.update(
        format!(
            "|{:?}|{:?}|{:?}|{}",
            query.repository_id, query.thread_id, query.time_range, query.include_descendants
        )
        .as_bytes(),
    );
    hash.finalize().into()
}

fn page(
    packet: &PerformanceReviewPacket,
    id: &str,
    offset: usize,
    limit: usize,
) -> Result<PerformanceReviewPacket, UsageStoreError> {
    let rows = &packet.outcomes.rows;
    if offset > rows.len() || offset > 0 && offset == rows.len() {
        return Err(UsageStoreError::InvalidReviewCursor);
    }
    let mut result = packet.clone();
    let mut end = offset.saturating_add(limit).min(rows.len());
    loop {
        result.outcomes.rows = rows[offset..end].to_vec();
        result.outcomes.next_cursor = (end < rows.len()).then(|| format!("{id}:{end}"));
        // Outcome measurements take precedence over metadata-only historical samples.
        while !result.work_bindings.references.is_empty()
            && serde_json::to_vec(&result)
                .map_err(|_| UsageStoreError::InvalidFact)?
                .len()
                > MAX_PAGE_BYTES
        {
            result.work_bindings.references.pop();
            result.work_bindings.omitted_references += 1;
        }
        if serde_json::to_vec(&result)
            .map_err(|_| UsageStoreError::InvalidFact)?
            .len()
            <= MAX_PAGE_BYTES
        {
            return Ok(result);
        }
        if end <= offset + 1 {
            return Err(UsageStoreError::TaskTreeTooLarge);
        }
        end -= 1;
    }
}
