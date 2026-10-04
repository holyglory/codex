use super::*;
use pretty_assertions::assert_eq;

#[cfg(unix)]
#[tokio::test]
async fn cache_backfill_resumes_pages_and_includes_new_facts_once() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process = ProcessId::new();
    store
        .register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("process");
    insert_thread(&store, "backfill").await;
    let op = operation(process, "backfill", OperationKind::ModelRequest);
    let request = record_request(&store, &op).await;
    for minute in 0..2_050 {
        let mut observation = token(
            request,
            RepositoryBucket::Unknown,
            Some(1),
            CoverageState::Complete,
        );
        observation.observed_at_ms = 1_100 + minute * 60_000;
        store
            .record_token_observation(&observation)
            .await
            .expect("history");
    }
    // Reproduce a historical correction fork: two unsuperseded roots for one
    // operation must still produce one deterministic derived owner row.
    for (event_id, occurred_at_ms, activity) in [
        ("10000000-0000-4000-8000-000000000001", 1_050, "coding"),
        ("10000000-0000-4000-8000-000000000002", 1_060, "diagnosis"),
    ] {
        sqlx::query(
            "INSERT INTO classification_events(event_id, operation_id, taxonomy_version, phase, activity, activity_state, provenance, supersedes_event_id, occurred_at_ms) VALUES (?, ?, 1, 'implementation', ?, 'model_active', 'agent_declared', NULL, ?)",
        )
        .bind(event_id)
        .bind(op.id.as_string())
        .bind(activity)
        .bind(occurred_at_ms)
        .execute(&store.pool)
        .await
        .expect("forked classification root");
    }
    let expected = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("warm");
    sqlx::query("DELETE FROM _usage_report_cache_meta")
        .execute(&store.pool)
        .await
        .expect("invalidate derived cache");
    crate::report_cache::prepare(&store.pool)
        .await
        .expect("start backfill");
    let status = store.report_cache_status().await.expect("cache status");
    assert!(!status.ready);
    assert!(status.progress.iter().any(|progress| {
        progress.source == "token_observations" && progress.high_water > progress.cursor
    }));
    assert!(
        !crate::report_cache::is_ready(&store.pool)
            .await
            .expect("not ready")
    );
    assert!(matches!(
        crate::report_cache::backfill::step(&store.pool)
            .await
            .expect("first page"),
        crate::report_cache::backfill::Progress::Advanced
    ));
    // A newly captured observation is beyond the historical high water. It is
    // maintained by the trigger, and must not be added again after restart.
    store
        .record_token_observation(&token(
            request,
            RepositoryBucket::Unknown,
            Some(7),
            CoverageState::Complete,
        ))
        .await
        .expect("concurrent capture");
    let during = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("canonical fallback");
    assert_eq!(
        during.tokens[0].measured_tokens,
        expected.tokens[0].measured_tokens + 7
    );
    store.report_refresh.cancel();
    sqlx::query("DELETE FROM _usage_report_cache_meta")
        .execute(&store.pool)
        .await
        .expect("prepare isolated interruption");
    crate::report_cache::prepare(&store.pool)
        .await
        .expect("restart rebuild");
    assert!(matches!(
        crate::report_cache::backfill::step(&store.pool)
            .await
            .expect("durable page"),
        crate::report_cache::backfill::Progress::Advanced
    ));
    store.close().await;
    let blocked_refresh = store
        .report_refresh
        .reader
        .acquire()
        .await
        .expect("hold refresh admission");
    let (reopened, parallel) =
        tokio::join!(UsageStore::open(temp.path()), UsageStore::open(temp.path()));
    let reopened = reopened.expect("open without waiting for old history");
    let parallel = parallel.expect("concurrent opener");
    assert!(std::sync::Arc::ptr_eq(
        &reopened.report_refresh,
        &parallel.report_refresh
    ));
    assert!(
        !crate::report_cache::is_ready(&reopened.pool)
            .await
            .expect("refresh still paused")
    );
    drop(blocked_refresh);
    crate::report_cache::ensure(&reopened.pool, &reopened.report_refresh.reader)
        .await
        .expect("warm cache");
    assert_eq!(
        reopened
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("rebuilt"),
        during
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM token_observations")
            .fetch_one(&reopened.pool)
            .await
            .expect("raw history"),
        2_051
    );
    let scoped = UsageSummaryQuery {
        thread_id: Some(ThreadId::new("backfill").expect("thread")),
        repository_id: None,
        account_profile_ref: None,
        time_range: Some(UtcTimeRange::new(3_600_123, 18_000_999).expect("range")),
    };
    let rolled = reopened
        .usage_summary_query(scoped.clone())
        .await
        .expect("inner hours and raw boundaries");
    assert_eq!(rolled.tokens[0].measured_tokens, 240);
    let compact_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _usage_report_token_hours")
        .fetch_one(&reopened.pool)
        .await
        .expect("hour rows");
    assert!(compact_rows < 40);
    sqlx::query("DELETE FROM _usage_report_cache_meta")
        .execute(&reopened.pool)
        .await
        .expect("canonical comparison");
    assert_eq!(
        reopened
            .usage_summary_query(scoped)
            .await
            .expect("raw window"),
        rolled
    );
    let another = UsageStore::open(temp.path()).await.expect("second opener");
    assert_eq!(
        another
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("unchanged"),
        during
    );
    another.report_refresh.cancel();
    sqlx::query(
        "UPDATE _usage_report_cache_meta SET schema_version = schema_version + 1, ready = 0",
    )
    .execute(&another.pool)
    .await
    .expect("future derived schema");
    assert!(matches!(
        crate::report_cache::backfill::step(&another.pool)
            .await
            .expect("respect newer owner"),
        crate::report_cache::backfill::Progress::Superseded
    ));
    assert_eq!(
        another
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("newer cache uses raw facts"),
        during
    );
}
