use super::*;
use pretty_assertions::assert_eq;

#[cfg(unix)]
#[tokio::test]
async fn cache_backfill_resumes_pages_and_includes_new_facts_once() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process = ProcessId::new();
    store.register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await.expect("process");
    insert_thread(&store, "backfill").await;
    let op = operation(process, "backfill", OperationKind::ModelRequest);
    let request = record_request(&store, &op).await;
    for _ in 0..2_050 {
        store.record_token_observation(&token(request, RepositoryBucket::Unknown,
            Some(1), CoverageState::Complete)).await.expect("history");
    }
    let expected = store.usage_summary(UsageSummaryScope::All).await.expect("warm");
    sqlx::query("DELETE FROM _usage_report_cache_meta").execute(&store.pool).await.expect("invalidate derived cache");
    crate::report_cache::prepare(&store.pool).await.expect("start backfill");
    assert!(!crate::report_cache::is_ready(&store.pool).await.expect("not ready"));
    assert!(!crate::report_cache::backfill::step(&store.pool).await.expect("first page"));
    // A newly captured observation is beyond the historical high water. It is
    // maintained by the trigger, and must not be added again after restart.
    store.record_token_observation(&token(request, RepositoryBucket::Unknown,
        Some(7), CoverageState::Complete)).await.expect("concurrent capture");
    let during = store.usage_summary(UsageSummaryScope::All).await.expect("canonical fallback");
    assert_eq!(during.tokens[0].measured_tokens, expected.tokens[0].measured_tokens + 7);
    store.close().await;
    let reopened = UsageStore::open(temp.path()).await.expect("resume remaining pages");
    assert_eq!(reopened.usage_summary(UsageSummaryScope::All).await.expect("rebuilt"), during);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM token_observations")
        .fetch_one(&reopened.pool).await.expect("raw history"), 2_051);
    let another = UsageStore::open(temp.path()).await.expect("second opener");
    assert_eq!(another.usage_summary(UsageSummaryScope::All).await.expect("unchanged"), during);
}
