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
