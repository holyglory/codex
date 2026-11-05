use super::*;
use codex_usage::UsageRecord;
use serde_json::json;
use tempfile::tempdir;

fn args(action: UsageStatsAction) -> UsageStatsArgs {
    UsageStatsArgs {
        action,
        scope: Some(UsageStatsScope::All),
        repository: None,
        account: None,
        thread_id: None,
        root_thread_id: None,
        include_descendants: None,
        agent_id: None,
        detail: None,
        from_at_ms: None,
        to_at_ms: None,
        limit: Some(10),
        cursor_sort_value: None,
        cursor_id: None,
        outcome_cursor: None,
        outcome_limit: None,
    }
}

fn record(id: &str, observed_at: i64) -> UsageRecord {
    UsageRecord::new(id, observed_at, 10, 3)
        .with_thread("thread-1")
        .with_operation(format!("operation-{id}"))
        .with_repository("repo-1")
        .with_tool("shell")
        .with_activity("coding", "implementation")
}

#[test]
fn summary_respects_current_chat_and_account_filters() {
    let directory = tempdir().expect("temporary directory");
    let mut store = UsageStore::open(directory.path()).expect("open usage store");
    store.append_record(record("one", 10)).expect("append one");
    store
        .append_record(UsageRecord::new("other", 11, 100, 100).with_thread("thread-2"))
        .expect("append other");

    let context = UsageStatsContext {
        codex_home: directory.path().to_path_buf(),
        thread_id: "thread-1".to_string(),
    };
    let mut request = args(UsageStatsAction::Summary);
    request.scope = Some(UsageStatsScope::CurrentChat);
    let value = execute(&mut store, &context, request).expect("summary");
    assert_eq!(value["records"], json!(1));
    assert_eq!(value["inputTokens"], json!(10));
    assert_eq!(value["scope"], json!("current_chat"));
}

#[test]
fn event_queries_return_metadata_and_a_cursor() {
    let directory = tempdir().expect("temporary directory");
    let mut store = UsageStore::open(directory.path()).expect("open usage store");
    store.append_record(record("one", 10)).expect("append one");
    store.append_record(record("two", 11)).expect("append two");
    let context = UsageStatsContext {
        codex_home: directory.path().to_path_buf(),
        thread_id: "thread-1".to_string(),
    };
    let mut request = args(UsageStatsAction::Events);
    request.limit = Some(1);
    let value = execute(&mut store, &context, request).expect("events");
    assert_eq!(value["kind"], json!("usageEvents"));
    assert_eq!(value["data"].as_array().expect("data").len(), 1);
    assert!(value["nextCursor"].is_object());
    assert!(value.get("snapshotGeneration").is_some());
}

#[test]
fn dimension_actions_expose_recorded_rows() {
    let directory = tempdir().expect("temporary directory");
    let mut store = UsageStore::open(directory.path()).expect("open usage store");
    store.append_record(record("one", 10)).expect("append one");
    let context = UsageStatsContext {
        codex_home: directory.path().to_path_buf(),
        thread_id: "thread-1".to_string(),
    };
    for action in [
        UsageStatsAction::Repositories,
        UsageStatsAction::Tools,
        UsageStatsAction::Activities,
        UsageStatsAction::PerformanceReview,
    ] {
        let value = execute(&mut store, &context, args(action)).expect("dimension query");
        assert_eq!(value["data"].as_array().expect("data").len(), 1);
        assert!(value.get("freshness").is_some());
    }
}
