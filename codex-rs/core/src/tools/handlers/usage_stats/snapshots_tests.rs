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
    let filter = query_filters::resolve(&context, &request).expect("resolved query");
    let mut report = execute(&reader, &context, request, filter.clone()).expect("observed report");
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
            filter,
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

#[test]
fn continuation_keeps_resolved_account_and_repository_after_alias_changes() {
    use codex_account_registry::AccountMetadata;
    use codex_account_registry::AccountRegistry;
    use codex_account_registry::RegistryStore;
    use codex_protocol::auth::AuthMode;

    let directory = tempdir().expect("directory");
    let home = directory.path().join("home");
    let cwd = directory.path().join("linked");
    std::fs::create_dir_all(&cwd).expect("linked checkout");
    let primary_a = directory.path().join("primary-a");
    let primary_b = directory.path().join("primary-b");
    for primary in [&primary_a, &primary_b] {
        let admin = primary.join(".git/worktrees/linked");
        std::fs::create_dir_all(&admin).expect("Git administrative directory");
        std::fs::write(primary.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        std::fs::write(admin.join("commondir"), "../..\n").expect("common directory");
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", cwd.join(".git").display()),
        )
        .expect("validated backlink");
    }
    let point_to = |primary: &std::path::Path| {
        std::fs::write(
            cwd.join(".git"),
            format!(
                "gitdir: {}\n",
                primary.join(".git/worktrees/linked").display()
            ),
        )
        .expect("linked checkout pointer");
    };
    point_to(&primary_a);
    let repository_a = crate::usage_identity::repository_id(&cwd).expect("repository A");
    let repository_b = crate::usage_identity::repository_id(&primary_b).expect("repository B");
    assert_ne!(repository_a, repository_b);
    let account_a = AccountMetadata::new(
        "selected".parse().expect("alias"),
        AuthMode::ApiKey,
        chrono::Utc::now(),
    );
    let account_b = AccountMetadata::new(
        "other".parse().expect("alias"),
        AuthMode::ApiKey,
        chrono::Utc::now(),
    );
    let registry = RegistryStore::new(&home);
    registry
        .create(&AccountRegistry {
            accounts: vec![account_a.clone(), account_b.clone()],
            default_account_id: Some(account_a.id.clone()),
            ..AccountRegistry::default()
        })
        .expect("account registry");
    let record = |id: &str, account: &AccountMetadata, repository: &str, input_tokens| {
        UsageRecord::new(
            id,
            /*observed_at*/ 10,
            input_tokens,
            /*output_tokens*/ 0,
        )
        .with_account(account.id.to_string())
        .with_repository(repository)
        .with_thread("thread-1")
    };
    let original = [
        record("a-1", &account_a, &repository_a, /*input_tokens*/ 1),
        record("a-2", &account_a, &repository_a, /*input_tokens*/ 2),
        record("a-3", &account_a, &repository_a, /*input_tokens*/ 3),
    ];
    let mut store = codex_usage::UsageStore::open(&home).expect("usage store");
    for fact in original.iter().cloned().chain([
        record(
            "b-in-a",
            &account_b,
            &repository_a,
            /*input_tokens*/ 100,
        ),
        record(
            "a-in-b",
            &account_a,
            &repository_b,
            /*input_tokens*/ 1000,
        ),
        record(
            "b-in-b",
            &account_b,
            &repository_b,
            /*input_tokens*/ 2000,
        ),
    ]) {
        store.append_record(fact).expect("record");
    }
    store
        .materialize_snapshot(codex_usage::Freshness::Fresh)
        .expect("first generation");
    let context = UsageStatsContext {
        codex_home: home,
        thread_id: "thread-1".into(),
        cwd: cwd.clone(),
    };
    let cache = SnapshotCache::new(LIFETIME);
    let params =
        json!({"action":"events", "scope":"current_repository", "account":"selected", "limit":1});
    let first = cache
        .execute_request(
            &context,
            serde_json::from_value(params.clone()).expect("first request"),
        )
        .expect("account A first page");
    registry
        .compare_and_swap(registry.read().expect("registry").generation, |registry| {
            registry.accounts[0].alias = "renamed".parse().expect("new alias");
            registry.accounts[1].alias = "selected".parse().expect("reassigned alias");
        })
        .expect("rename with current registry generation");
    store
        .append_record(record(
            "b-after",
            &account_b,
            &repository_b,
            /*input_tokens*/ 4000,
        ))
        .expect("new B observation");
    store
        .materialize_snapshot(codex_usage::Freshness::Fresh)
        .expect("new generation");
    let continuation = |page: &Value| {
        let mut request = params.clone();
        request["cursor_id"] = page["nextCursor"]["id"].clone();
        request["cursor_sort_value"] = page["nextCursor"]["sortValue"].clone();
        serde_json::from_value(request).expect("continuation")
    };
    let second = cache
        .execute_request(&context, continuation(&first))
        .expect("A after alias reassignment");
    point_to(&primary_b);
    assert_eq!(
        crate::usage_identity::repository_id(&context.cwd),
        Some(repository_b.clone())
    );
    let third = cache
        .execute_request(&context, continuation(&second))
        .expect("A after current repository changes");
    assert_eq!(
        [&first, &second, &third]
            .into_iter()
            .flat_map(|page| page["data"].as_array().expect("rows").clone())
            .collect::<Vec<_>>(),
        original.iter().map(record_fields).collect::<Vec<_>>()
    );
    assert!(third["nextCursor"].is_null());
    for page in [&second, &third] {
        assert_eq!(
            (&page["snapshotGeneration"], &page["journalWatermark"]),
            (&first["snapshotGeneration"], &first["journalWatermark"])
        );
    }
    {
        let entries = cache.state.entries.lock().expect("cache");
        let pinned = entries.snapshots.front().expect("pinned query");
        assert_eq!(
            pinned.filter,
            UsageFilter::default()
                .for_account(account_a.id.to_string())
                .for_repository(repository_a.clone())
        );
        assert_eq!(
            pinned
                .reader
                .totals(TimeRange::all(), &pinned.filter)
                .expect("frozen totals"),
            codex_usage::UsageTotals {
                records: 3,
                input_tokens: 6,
                ..codex_usage::UsageTotals::default()
            }
        );
    }
    for (field, value) in [
        ("account", json!("renamed")),
        ("repository", json!(repository_a)),
    ] {
        let mut changed = serde_json::to_value(continuation(&first)).expect("query");
        changed[field] = value;
        let error = cache
            .execute_request(
                &context,
                serde_json::from_value(changed).expect("changed query"),
            )
            .expect_err("query mismatch");
        assert!(error.to_string().contains("does not match"));
    }
    let fresh = cache
        .execute_request(
            &context,
            serde_json::from_value(params).expect("fresh request"),
        )
        .expect("new alias target");
    assert_eq!(
        (
            &fresh["data"][0]["accountId"],
            &fresh["data"][0]["repositoryId"]
        ),
        (&json!(account_b.id.to_string()), &json!(repository_b))
    );
}
