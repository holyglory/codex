//! Bounded continuation leases release their pins at a fixed expiry.
use super::*;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

const PREFIX: &str = "usage-v1:";
const MAX_SNAPSHOTS: usize = 8;
const LIFETIME: Duration = Duration::from_secs(10 * 60);

struct Pinned {
    id: String,
    created: Instant,
    context: (PathBuf, String, PathBuf),
    query: Value,
    reader: Arc<UsageReader>,
    review: Option<Arc<Value>>,
}

#[derive(Default)]
struct Entries {
    snapshots: VecDeque<Pinned>,
    closed: bool,
}

struct CacheState {
    entries: Mutex<Entries>,
    changed: Condvar,
    lifetime: Duration,
}

struct SnapshotCache {
    state: Arc<CacheState>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl SnapshotCache {
    fn new(lifetime: Duration) -> Self {
        Self {
            state: Arc::new(CacheState {
                entries: Mutex::new(Entries::default()),
                changed: Condvar::new(),
                lifetime,
            }),
            worker: Mutex::new(None),
        }
    }

    fn execute_request(
        &self,
        context: &UsageStatsContext,
        mut args: UsageStatsArgs,
    ) -> Result<Value, FunctionCallError> {
        let is_review = matches!(args.action, UsageStatsAction::PerformanceReview);
        let outcome_limit = usize::try_from(args.outcome_limit.unwrap_or(8))
            .map_err(|_| tool_error("outcome_limit must be between 1 and 50"))?;
        if is_review && !(1..=50).contains(&outcome_limit) {
            return Err(tool_error("outcome_limit must be between 1 and 50"));
        }
        if is_review && (args.cursor_id.is_some() || args.cursor_sort_value.is_some()) {
            return Err(tool_error(
                "performance_review uses outcome_cursor for continuation",
            ));
        }
        if args.outcome_cursor.is_some()
            && (!is_review || args.cursor_id.is_some() || args.cursor_sort_value.is_some())
        {
            return Err(tool_error(
                "outcome_cursor requires performance_review without an operation cursor",
            ));
        }
        let outcome = args
            .outcome_cursor
            .as_deref()
            .map(outcomes::cursor)
            .transpose()?;
        let outcome_offset = outcome
            .as_ref()
            .map_or(/*default*/ 0, |(_, offset)| *offset);
        let mut signature = serde_json::to_value(&args).map_err(|_| storage_error())?;
        let signature_fields = signature.as_object_mut().ok_or_else(storage_error)?;
        for field in [
            "cursor_id",
            "cursor_sort_value",
            "outcome_cursor",
            "limit",
            "outcome_limit",
        ] {
            signature_fields.remove(field);
        }
        let owner = (
            context.codex_home.clone(),
            context.thread_id.clone(),
            context.cwd.clone(),
        );
        let continuation = args
            .cursor_id
            .as_deref()
            .filter(|id| id.starts_with(PREFIX))
            .map(|id| {
                let (snapshot, row) = id[PREFIX.len()..]
                    .split_once(':')
                    .ok_or_else(|| tool_error("usage cursor is invalid"))?;
                Ok::<_, FunctionCallError>((snapshot.to_string(), row.to_string()))
            })
            .transpose()?;
        let continuation = outcome
            .map(|(id, _)| (id, None))
            .or_else(|| continuation.map(|(id, row)| (id, Some(row))));
        let (reader, retained_review, snapshot) = if let Some((id, row)) = continuation {
            let retained = self
                .state
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = retained
                .snapshots
                .iter()
                .find(|entry| entry.id == id && entry.created.elapsed() < self.state.lifetime)
                .ok_or_else(|| tool_error("usage cursor expired; request a new first page"))?;
            if entry.context != owner || entry.query != signature {
                return Err(tool_error("usage cursor does not match this query"));
            }
            if args.outcome_cursor.is_some() && entry.review.is_none() {
                return Err(tool_error("usage cursor does not match this query"));
            }
            args.cursor_id = row;
            (
                Arc::clone(&entry.reader),
                entry.review.clone(),
                Some((id, entry.created)),
            )
        } else {
            (
                Arc::new(UsageReader::open(&context.codex_home).map_err(|_| storage_error())?),
                None,
                None,
            )
        };
        let mut value = if let Some(report) = retained_review.as_deref() {
            report.clone()
        } else {
            execute(&reader, context, args)?
        };
        let (id, created) =
            snapshot.unwrap_or_else(|| (uuid::Uuid::new_v4().simple().to_string(), Instant::now()));
        let review = if is_review {
            let page = outcomes::page(&value, &id, outcome_offset, outcome_limit)?;
            let report = retained_review.unwrap_or_else(|| Arc::new(value));
            value = page;
            Some(report)
        } else {
            None
        };
        let next_row = value
            .get("nextCursor")
            .and_then(|cursor| cursor.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if next_row.is_some()
            || value
                .pointer("/outcomes/nextCursor")
                .is_some_and(Value::is_string)
        {
            // An in-flight query retains its own Arc after cache expiry. Its
            // result remains consistent, but cannot renew the original lease.
            if created.elapsed() < self.state.lifetime {
                {
                    let mut worker = self
                        .worker
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if worker.is_none() {
                        let state = Arc::clone(&self.state);
                        *worker = Some(
                            std::thread::Builder::new()
                                .name("usage-cursor-expiry".into())
                                .spawn(move || state.expire())
                                .map_err(|_| storage_error())?,
                        );
                    }
                }
                let mut retained = self
                    .state
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                retained
                    .snapshots
                    .retain(|entry| entry.created.elapsed() < self.state.lifetime);
                if !retained.snapshots.iter().any(|entry| entry.id == id)
                    && created.elapsed() < self.state.lifetime
                {
                    while retained.snapshots.len() >= MAX_SNAPSHOTS {
                        retained.snapshots.pop_front();
                    }
                    retained.snapshots.push_back(Pinned {
                        id: id.clone(),
                        created,
                        context: owner,
                        query: signature,
                        reader,
                        review,
                    });
                }
                self.state.changed.notify_all();
            }
            if let Some(row) = next_row
                && let Some(cursor) = value.get_mut("nextCursor").and_then(Value::as_object_mut)
            {
                let remaining = self.state.lifetime.saturating_sub(created.elapsed());
                cursor.insert("id".to_string(), json!(format!("{PREFIX}{id}:{row}")));
                cursor.insert("expiresAfterMs".to_string(), json!(remaining.as_millis()));
            }
        }
        Ok(value)
    }
}

impl CacheState {
    fn expire(self: Arc<Self>) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !entries.closed {
            let before = entries.snapshots.len();
            entries
                .snapshots
                .retain(|entry| entry.created.elapsed() < self.lifetime);
            if entries.snapshots.len() != before {
                self.changed.notify_all();
            }
            let remaining = entries
                .snapshots
                .iter()
                .map(|entry| self.lifetime.saturating_sub(entry.created.elapsed()))
                .min();
            entries = if let Some(remaining) = remaining {
                self.changed
                    .wait_timeout(entries, remaining)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0
            } else {
                self.changed
                    .wait(entries)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            };
        }
    }
}

impl Drop for SnapshotCache {
    fn drop(&mut self) {
        {
            let mut entries = self
                .state
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries.closed = true;
            entries.snapshots.clear();
            self.state.changed.notify_all();
        }
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = worker.join();
        }
    }
}

pub(super) fn execute_request(
    context: &UsageStatsContext,
    args: UsageStatsArgs,
) -> Result<Value, FunctionCallError> {
    static SNAPSHOTS: OnceLock<SnapshotCache> = OnceLock::new();
    SNAPSHOTS
        .get_or_init(|| SnapshotCache::new(LIFETIME))
        .execute_request(context, args)
}

#[cfg(test)]
#[path = "snapshots_tests.rs"]
mod tests;
