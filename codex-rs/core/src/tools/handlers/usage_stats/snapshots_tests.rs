use super::*;
use codex_usage::JournalPayload;
use codex_usage::JournalReadLimits;
use codex_usage::UsageJournal;
use codex_usage::UsagePublisher;
use codex_usage::UsageRecord;
use pretty_assertions::assert_eq;
use std::fs::File;
use std::fs::TryLockError;
use tempfile::tempdir;

#[test]
fn abandoned_cursor_expiry_releases_real_pin_without_another_request() {
    let directory = tempdir().expect("directory");
    let root = directory.path().join(codex_usage::USAGE_DIRECTORY_NAME);
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let first_record = UsageRecord::new(
        "one", /*observed_at*/ 10, /*input_tokens*/ 1, /*output_tokens*/ 0,
    );
    let second_record = UsageRecord::new(
        "two", /*observed_at*/ 11, /*input_tokens*/ 1, /*output_tokens*/ 0,
    );
    journal
        .append_batch(vec![
            JournalPayload::Record(first_record.clone()),
            JournalPayload::Record(second_record.clone()),
        ])
        .expect("capture records");
    let mut publisher = UsagePublisher::open_at(&root).expect("publisher");
    publisher
        .publish_batch(JournalReadLimits {
            max_bytes: 1024 * 1024,
            max_entries: 100,
        })
        .expect("publish original view");
    let cache = SnapshotCache::new(Duration::from_secs(/*secs*/ 1));
    let context = UsageStatsContext {
        codex_home: directory.path().to_path_buf(),
        thread_id: "thread-1".into(),
        cwd: directory.path().to_path_buf(),
    };
    let request = serde_json::from_value(json!({"action":"events","scope":"all","limit":1}))
        .expect("first page request");
    let first = cache
        .execute_request(&context, request)
        .expect("real first cursor page");
    assert!(first["nextCursor"].is_object());
    let active_query = {
        let entries = cache.state.entries.lock().expect("cache");
        Arc::clone(&entries.snapshots.front().expect("cached snapshot").reader)
    };
    let weak = Arc::downgrade(&active_query);
    let readers = File::options()
        .read(true)
        .write(true)
        .open(root.join(".readers.lock"))
        .expect("real reclamation lock");
    assert!(matches!(readers.try_lock(), Err(TryLockError::WouldBlock)));
    journal
        .append_record(UsageRecord::new(
            "later", /*observed_at*/ 12, /*input_tokens*/ 1, /*output_tokens*/ 0,
        ))
        .expect("capture after first page");
    publisher
        .publish_batch(JournalReadLimits {
            max_bytes: 1024 * 1024,
            max_entries: 100,
        })
        .expect("publish while the old view is pinned");
    // Wait for the cache's own expiry notification, without making a second
    // usage request or invoking the cache's cleanup from the test thread.
    let entries = cache.state.entries.lock().expect("cache");
    let (entries, _) = cache
        .state
        .changed
        .wait_timeout_while(entries, Duration::from_secs(/*secs*/ 10), |entries| {
            !entries.snapshots.is_empty()
        })
        .expect("expiry notification");
    assert!(
        entries.snapshots.is_empty(),
        "idle cache must expire its own lease"
    );
    drop(entries);
    assert_eq!(weak.strong_count(), 1);
    assert!(
        matches!(readers.try_lock(), Err(TryLockError::WouldBlock)),
        "expiry must preserve the active query's pin"
    );
    let page = active_query
        .page(
            TimeRange::all(),
            &UsageFilter::default(),
            RecordKind::All,
            /*after*/ None,
            /*limit*/ 10,
        )
        .expect("active consistent old view");
    assert_eq!(page.records, vec![first_record, second_record]);
    drop(active_query);
    assert_eq!(weak.strong_count(), 0);
    readers
        .try_lock()
        .expect("reclamation gate opens without another usage request");
}
