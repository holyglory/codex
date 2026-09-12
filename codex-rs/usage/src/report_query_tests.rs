use super::*;
use pretty_assertions::assert_eq;

#[cfg(unix)]
#[tokio::test]
async fn scoped_reports_preserve_results_when_unrelated_history_grows() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process = ProcessId::new();
    store.register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 900).await.expect("process");
    insert_thread(&store, "selected").await;
    insert_thread(&store, "unrelated").await;
    let repository = store.resolve_repository(&identity("/selected"), &label("selected"), /*observed_at_ms*/ 1).await.expect("repository");
    let selected = operation(process, "selected", OperationKind::ModelRequest);
    let request = record_request(&store, &selected).await;
    attribute(&store, selected.id, Some(repository.clone()), RepositoryAttributionKind::Primary).await;
    store.record_token_observation(&token(request, RepositoryBucket::Single(repository.clone()), Some(25), CoverageState::Complete)).await.expect("tokens");
    let queries = [
        UsageSummaryQuery { thread_id: Some(ThreadId::new("selected").expect("thread")), repository_id: None, account_profile_ref: None, time_range: Some(UtcTimeRange::new(1_000, 2_000).expect("range")) },
        UsageSummaryQuery { thread_id: None, repository_id: Some(repository.clone()), account_profile_ref: None, time_range: Some(UtcTimeRange::new(1_000, 2_000).expect("range")) },
    ];
    let mut expected = Vec::new();
    for query in &queries {
        expected.push(store.usage_summary_query(query.clone()).await.expect("baseline"));
    }
    for _ in 0..100 {
        let unrelated = operation(process, "unrelated", OperationKind::ModelRequest);
        let request = record_request(&store, &unrelated).await;
        store.record_token_observation(&token(request, RepositoryBucket::Unknown, Some(1_000), CoverageState::Partial)).await.expect("unrelated tokens");
    }
    let mut outside_window = token(request, RepositoryBucket::Single(repository), Some(100), CoverageState::Partial);
    outside_window.observed_at_ms = 2_000;
    store.record_token_observation(&outside_window).await.expect("later observation");
    let mut actual = Vec::new();
    for query in queries {
        actual.push(store.usage_summary_query(query).await.expect("scoped summary"));
    }
    assert_eq!(actual, expected);
}

#[cfg(unix)]
#[tokio::test]
async fn completed_capture_keeps_history_without_reporting_the_start_as_a_gap() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process = ProcessId::new();
    store.register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 900).await.expect("process");
    insert_thread(&store, "complete").await;
    let op = operation(process, "complete", OperationKind::ModelRequest);
    let request = record_request(&store, &op).await;
    store.record_token_observation(&token(request, RepositoryBucket::Unknown, Some(25), CoverageState::Complete)).await.expect("tokens");
    for (state, time) in [(CoverageState::CaptureStarted, 1_000), (CoverageState::Partial, 1_100)] {
        store.record_coverage(&NewCoverageEvent {event_id:FactEventId::new(),operation_id:Some(op.id),
            scope_kind:CoverageScopeKind::new("model_attempt").expect("scope"),state,reason_code:None,
            occurred_at_ms:time}).await.expect("capture");
    }
    let pending = store.usage_summary(UsageSummaryScope::All).await.expect("pending summary");
    assert!(pending.coverage.has_gaps);
    store.finish_operation(&TerminalOperation {operation_id:op.id,status:TerminalStatus::Completed,
        occurred_at_ms:1_100,duration_ns:100_000_000,error_category:None}).await.expect("terminal");
    let complete = store.usage_summary(UsageSummaryScope::All).await.expect("complete summary");
    assert_eq!(complete.coverage, CoverageSummary {overall_state:"complete".into(),
        event_counts:vec![CoverageCount {state:"capture_started".into(),count:1},CoverageCount {state:"partial".into(),count:1}],
        token_observation_counts:vec![CoverageCount {state:"complete".into(),count:1}],has_gaps:false,unfinished_operations:0});
    let structured = StructuredUsageSummary::new(&complete, None);
    assert_eq!(structured.coverage.dimensions.recorded_tokens, "complete");
    assert_eq!(structured.coverage.dimensions.timing_unknown_intervals, 0);
    store.record_coverage(&NewCoverageEvent {event_id:FactEventId::new(),operation_id:Some(op.id),
        scope_kind:CoverageScopeKind::new("model_attempt").expect("scope"),state:CoverageState::Unknown,
        reason_code:None,occurred_at_ms:1_200}).await.expect("missing evidence");
    let missing = store.usage_summary(UsageSummaryScope::All).await.expect("missing summary");
    assert!(missing.coverage.has_gaps);
    assert_eq!(missing.tokens, complete.tokens);
}
