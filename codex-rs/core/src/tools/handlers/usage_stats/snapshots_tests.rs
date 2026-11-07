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

#[test]
fn outcome_continuation_freezes_report_filters_and_original_lease() {
    let directory = tempdir().expect("directory");
    let mut store = codex_usage::UsageStore::open(directory.path()).expect("store");
    store
        .append_record(
            UsageRecord::new(
                "one", /*observed_at*/ 10, /*input_tokens*/ 1, /*output_tokens*/ 0,
            )
            .with_thread("thread-1")
            .with_repository("repo-1"),
        )
        .expect("record");
    store
        .materialize_snapshot(codex_usage::Freshness::Fresh)
        .expect("publish");
    let context = UsageStatsContext {
        codex_home: directory.path().to_path_buf(),
        thread_id: "thread-1".into(),
        cwd: directory.path().to_path_buf(),
    };
    let params = json!({"action":"performance_review","scope":"all", "repository":"repo-1",
        "thread_id":"current", "include_descendants":true, "outcome_limit":1});
    let request: UsageStatsArgs = serde_json::from_value(params.clone()).expect("request");
    let mut query = serde_json::to_value(&request).expect("signature");
    for field in [
        "cursor_id",
        "cursor_sort_value",
        "outcome_cursor",
        "limit",
        "outcome_limit",
    ] {
        query.as_object_mut().expect("signature").remove(field);
    }
    let reader = Arc::new(UsageReader::open(directory.path()).expect("reader"));
    let mut report = execute(&reader, &context, request).expect("observed report");
    // No production outcome source exists yet. Fixture rows exercise the cache
    // boundary; the integration test separately verifies its honest empty set.
    report["outcomes"]["coverage"] = json!("fixture_only");
    report["outcomes"]["rows"] = json!(
        (0..12)
            .map(|id| json!({"id":format!("fixture-{id}")}))
            .collect::<Vec<_>>()
    );
    report["outcomes"]["totalRows"] = json!(12);
    let snapshot = uuid::Uuid::new_v4().simple().to_string();
    let first = outcomes::page(&report, &snapshot, /*offset*/ 0, /*limit*/ 1).expect("first page");
    let created = Instant::now() - Duration::from_secs(/*secs*/ 1);
    let cache = SnapshotCache::new(Duration::from_secs(/*secs*/ 60));
    cache
        .state
        .entries
        .lock()
        .expect("cache")
        .snapshots
        .push_back(Pinned {
            id: snapshot.clone(),
            created,
            context: (
                context.codex_home.clone(),
                context.thread_id.clone(),
                context.cwd.clone(),
            ),
            query,
            reader,
            review: Some(Arc::new(report.clone())),
        });
    store
        .append_record(
            UsageRecord::new(
                "later", /*observed_at*/ 11, /*input_tokens*/ 100,
                /*output_tokens*/ 0,
            )
            .with_thread("thread-1")
            .with_repository("repo-1"),
        )
        .expect("new observation");
    store
        .materialize_snapshot(codex_usage::Freshness::Fresh)
        .expect("new generation");
    let mut continuation = params;
    continuation["outcome_cursor"] = first["outcomes"]["nextCursor"].clone();
    let second = cache
        .execute_request(
            &context,
            serde_json::from_value(continuation.clone()).expect("continuation"),
        )
        .expect("same snapshot");
    assert_eq!(
        second,
        outcomes::page(&report, &snapshot, /*offset*/ 1, /*limit*/ 1)
            .expect("expected frozen report")
    );
    let mut default_limit = continuation.clone();
    default_limit
        .as_object_mut()
        .expect("request")
        .remove("outcome_limit");
    assert_eq!(
        cache
            .execute_request(
                &context,
                serde_json::from_value(default_limit).expect("default")
            )
            .expect("default page"),
        outcomes::page(&report, &snapshot, /*offset*/ 1, /*limit*/ 8).expect("eight rows")
    );
    let mut final_page = continuation.clone();
    final_page["outcome_limit"] = json!(50);
    assert_eq!(
        cache
            .execute_request(
                &context,
                serde_json::from_value(final_page).expect("larger limit")
            )
            .expect("remaining rows"),
        outcomes::page(&report, &snapshot, /*offset*/ 1, /*limit*/ 50).expect("complete remainder")
    );
    assert_eq!(
        cache
            .state
            .entries
            .lock()
            .expect("cache")
            .snapshots
            .front()
            .expect("retained")
            .created,
        created
    );
    for (field, value) in [
        ("repository", json!("repo-2")),
        ("account", json!("another-account")),
        ("thread_id", json!("another-thread")),
        ("from_at_ms", json!(1)),
        ("to_at_ms", json!(1000)),
        ("scope", json!("current_chat")),
        ("include_descendants", json!(false)),
        ("agent_id", json!("another-agent")),
        ("outcome_limit", json!(0)),
        ("outcome_limit", json!(51)),
        ("action", json!("events")),
        ("cursor_id", json!("operation-cursor")),
    ] {
        let mut changed = continuation.clone();
        changed[field] = value;
        assert!(
            cache
                .execute_request(
                    &context,
                    serde_json::from_value(changed).expect("changed request")
                )
                .is_err(),
            "changed {field}"
        );
    }
    let other_context = UsageStatsContext {
        codex_home: context.codex_home.clone(),
        thread_id: "another-owner".into(),
        cwd: context.cwd.clone(),
    };
    assert!(
        cache
            .execute_request(
                &other_context,
                serde_json::from_value(continuation.clone()).expect("other owner")
            )
            .is_err()
    );
    cache
        .state
        .entries
        .lock()
        .expect("cache")
        .snapshots
        .front_mut()
        .expect("retained")
        .created = Instant::now() - Duration::from_secs(/*secs*/ 120);
    let error = cache
        .execute_request(
            &context,
            serde_json::from_value(continuation).expect("expired cursor"),
        )
        .expect_err("expired lease");
    assert!(error.to_string().contains("expired"));
}
