//! Real publication through the same request/cache boundary used by the tool.
use super::*;
use codex_usage::GlobalReviewPublication;
use codex_usage::GlobalReviewPublisher;
use codex_usage::GlobalReviewReader;
use codex_usage::UsageOutcome;
use codex_usage::WorkBindingUpdate;
use codex_usage::capture_work_binding_transition;
use pretty_assertions::assert_eq;

fn capture_outcome(home: &std::path::Path, ordinal: u64) {
    let thread = format!("thread-{ordinal}");
    capture_work_binding_transition(
        home,
        WorkBindingUpdate {
            version: 1,
            native_project_id: Some("explicit-project".into()),
            thread_id: thread.clone(),
            workstream_id: Some("explicit-stream".into()),
            outcome_id: Some(format!("outcome-{ordinal}")),
            transition_sequence: 1,
            source: "agent_declared".into(),
            authority: "explicit_agent_declaration".into(),
        },
    )
    .expect("explicit outcome boundary");
    let at = 1_000 + ordinal as i64 * 100;
    let mut start = UsageRecord::new_millis(
        format!("start-{ordinal}"),
        at,
        /*input_tokens*/ 0,
        /*output_tokens*/ 0,
    )
    .with_operation(format!("operation-{ordinal}"))
    .with_thread(thread.clone())
    .with_repository("fixture-repository")
    .with_account("fixture-account");
    start.event = Some("model_request_started".into());
    start.started_at_ms = Some(at);
    start.agent_id = Some(thread);
    let mut finish = start.clone();
    finish.id = format!("finish-{ordinal}");
    finish.observed_at_ms = Some(at + 50);
    finish.ended_at_ms = Some(at + 50);
    finish.duration_ms = Some(50);
    finish.event = Some("model_request_finished".into());
    finish.outcome = Some(UsageOutcome::Completed);
    finish.coverage = Some("recorded".into());
    finish.input_tokens = 10 + ordinal;
    finish.provider_total_tokens = Some(10 + ordinal);
    UsageJournal::new(
        home.join(codex_usage::USAGE_DIRECTORY_NAME)
            .join("events.jsonl"),
    )
    .append_batch(vec![
        JournalPayload::Record(start),
        JournalPayload::Record(finish),
    ])
    .expect("capture real operation");
}

fn publish(home: &std::path::Path) {
    UsagePublisher::open_at(home.join(codex_usage::USAGE_DIRECTORY_NAME))
        .expect("canonical publisher")
        .publish_batch(JournalReadLimits {
            max_bytes: 1024 * 1024,
            max_entries: 100,
        })
        .expect("canonical prefix");
}

#[test]
fn completed_global_review_keeps_its_prefix_across_capture_restart_and_pagination() {
    let home = tempdir().expect("home");
    for ordinal in 0..2 {
        capture_outcome(home.path(), ordinal);
    }
    publish(home.path());
    capture_outcome(home.path(), /*ordinal*/ 2);
    let root = home.path().join(codex_usage::USAGE_DIRECTORY_NAME);
    assert_eq!(
        GlobalReviewPublisher::open_at(&root)
            .expect("background publisher")
            .expect("new candidate")
            .publish()
            .expect("complete background publication"),
        GlobalReviewPublication::Published
    );
    let completed = GlobalReviewReader::open_at(&root)
        .expect("reader")
        .expect("completed snapshot");
    let source = completed.snapshot().clone();
    assert!(source.included_journal_watermark > source.source_watermark);
    let context = UsageStatsContext {
        codex_home: home.path().to_owned(),
        thread_id: "current".into(),
        cwd: home.path().to_owned(),
    };
    let params = json!({"action":"performance_review","outcome_limit":1});
    let cache = SnapshotCache::new(LIFETIME);
    let first = cache
        .execute_request(
            &context,
            serde_json::from_value(params.clone()).expect("request"),
        )
        .expect("public handler first page");
    assert_eq!(first["projection"], "global_review_snapshot");
    assert_eq!(
        first["snapshotGeneration"],
        source.source_snapshot_generation
    );
    assert_eq!(first["watermark"], source.source_watermark);
    assert_eq!(first["journalWatermark"], source.included_journal_watermark);
    assert_eq!(first["sourceSnapshotId"], source.snapshot_id);
    assert_eq!(first["snapshotPublishedAtMs"], source.published_at_ms);
    assert_eq!(first["outcomes"]["totalRows"], 3);
    assert_eq!(
        first["outcomes"]["rows"]
            .as_array()
            .expect("first row")
            .len(),
        1
    );
    for field in ["counts", "tokens", "cost", "timing", "lifecycle"] {
        assert_eq!(first.get(field), completed.report().get(field));
    }
    assert!(
        cache
            .state
            .entries
            .lock()
            .expect("cache")
            .snapshots
            .front()
            .expect("outcome page")
            .reader
            .is_none()
    );
    capture_outcome(home.path(), /*ordinal*/ 3);
    publish(home.path());
    let restarted = SnapshotCache::new(LIFETIME);
    let stale = restarted
        .execute_request(
            &context,
            serde_json::from_value(params.clone()).expect("new request"),
        )
        .expect("restart serves last completed report");
    assert_eq!(stale["snapshotGeneration"], first["snapshotGeneration"]);
    assert_eq!(stale["journalWatermark"], first["journalWatermark"]);
    assert_eq!(stale["cost"], first["cost"]);
    assert_eq!(stale["tokens"], first["tokens"]);
    assert_eq!(stale["freshness"], "stale");
    assert!(
        stale["pendingEntries"]
            .as_u64()
            .expect("pending newer capture")
            > 0
    );
    // A corrupted live manifest cannot turn this read into a foreground rebuild.
    let manifest = std::fs::read(root.join("manifest.json")).expect("preserve manifest");
    std::fs::write(root.join("manifest.json"), b"invalid live manifest")
        .expect("isolate unavailable live source");
    let independent = restarted
        .execute_request(
            &context,
            serde_json::from_value(params.clone()).expect("request"),
        )
        .expect("completed report is independent of live metadata");
    assert_eq!(independent["sourceSnapshotId"], first["sourceSnapshotId"]);
    let mut next = params.clone();
    next["outcome_cursor"] = first["outcomes"]["nextCursor"].clone();
    let second = cache
        .execute_request(
            &context,
            serde_json::from_value(next).expect("continuation"),
        )
        .expect("continuation remains on original response");
    assert_eq!(second["snapshotGeneration"], first["snapshotGeneration"]);
    assert_eq!(second["journalWatermark"], first["journalWatermark"]);
    assert_eq!(second["pendingEntries"], first["pendingEntries"]);
    assert_eq!(
        second["outcomes"]["rows"][0],
        completed.report()["outcomes"]["rows"][1]
    );
    std::fs::write(root.join("manifest.json"), manifest).expect("restore live source");
    assert_eq!(
        GlobalReviewPublisher::open_at(&root)
            .expect("publisher")
            .expect("new source")
            .publish()
            .expect("publish newer complete report"),
        GlobalReviewPublication::Published
    );
    let newer = restarted
        .execute_request(
            &context,
            serde_json::from_value(params).expect("new first page"),
        )
        .expect("new first page sees latest completed report");
    assert_ne!(newer["sourceSnapshotId"], first["sourceSnapshotId"]);
    assert!(newer["snapshotGeneration"].as_u64().unwrap() > source.source_snapshot_generation);
    assert_eq!(newer["outcomes"]["totalRows"], 4);
}

#[test]
fn global_review_requires_publication_and_never_absorbs_filtered_or_other_actions() {
    let home = tempdir().expect("home");
    capture_outcome(home.path(), /*ordinal*/ 0);
    publish(home.path());
    let context = UsageStatsContext {
        codex_home: home.path().to_owned(),
        thread_id: "thread-0".into(),
        cwd: home.path().to_owned(),
    };
    let cache = SnapshotCache::new(LIFETIME);
    let missing = cache
        .execute_request(
            &context,
            serde_json::from_value(json!({"action":"performance_review"})).expect("request"),
        )
        .expect_err("no foreground fallback before initial publication");
    assert!(missing.to_string().contains("global_review_pending"));
    for params in [
        json!({"action":"performance_review","scope":"current_chat"}),
        json!({"action":"performance_review","repository":"fixture-repository"}),
        json!({"action":"performance_review","account":"acct_00000000000000000000000000000000"}),
        json!({"action":"performance_review","agent_id":"00000000-0000-0000-0000-000000000000"}),
        json!({"action":"performance_review","from_at_ms":1_000,"to_at_ms":2_000}),
        json!({"action":"task_tree_summary","root_thread_id":"current","include_descendants":false}),
        json!({"action":"summary","scope":"all"}),
    ] {
        let request: UsageStatsArgs = serde_json::from_value(params).expect("scoped request");
        let filter = query_filters::resolve(&context, &request).expect("explicit scope");
        assert!(!super::super::super::global_review::selects(
            &request, &filter
        ));
        let value = cache
            .execute_request(&context, request)
            .expect("existing scoped/live path");
        assert!(value.get("projection").is_none());
    }
}
