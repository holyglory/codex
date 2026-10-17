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
    let started = std::time::Instant::now();
    assert!(matches!(
        store.require_report_ready(None).await,
        Err(UsageStoreError::ReportWarming(_))
    ));
    assert!(matches!(
        store
            .require_report_ready(Some(
                UtcTimeRange::new(i64::MIN, 2000).expect("unbounded start")
            ))
            .await,
        Err(UsageStoreError::ReportWarming(_))
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    let finite = UtcTimeRange::new(1000, 2000).expect("finite window");
    store
        .require_report_ready(Some(finite))
        .await
        .expect("finite review during warming");
    let review = store
        .performance_review_packet(crate::PerformanceReviewQuery {
            time_range: Some(finite),
            ..Default::default()
        })
        .await
        .expect("cold finite review");
    assert_eq!(review.coverage.raw_operations, 1);
    // The source-owned upgrade resumes exactly the saved frontier.
    sqlx::query("UPDATE _usage_report_cache_meta SET schema_version = 7")
        .execute(&store.pool)
        .await
        .expect("previous version");
    crate::report_cache::prepare(&store.pool)
        .await
        .expect("compatible upgrade");
    assert_eq!(
        store
            .report_cache_status()
            .await
            .expect("resumed status")
            .progress,
        status.progress
    );

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
    let cached_global = reopened
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("cached global comparison");
    let scoped = UsageSummaryQuery {
        worktree_ids: None,
        include_unassigned_worktrees: true,
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
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("canonical global comparison"),
        cached_global
    );
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

#[cfg(unix)]
#[tokio::test]
async fn cached_global_coverage_gaps_match_canonical_reporting() {
    for (
        scope_kind,
        state,
        reason_code,
        terminal,
        model_request,
        operation_id,
        expected_unfinished,
    ) in [
        (
            "operation",
            CoverageState::Partial,
            Some("provider_error"),
            true,
            false,
            true,
            0,
        ),
        (
            "model_attempt",
            CoverageState::Partial,
            None,
            false,
            false,
            true,
            1,
        ),
        (
            "model_attempt",
            CoverageState::Partial,
            None,
            true,
            true,
            true,
            0,
        ),
        (
            "process",
            CoverageState::Unknown,
            None,
            true,
            false,
            false,
            0,
        ),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = UsageStore::open(temp.path()).await.expect("store");
        let process = ProcessId::new();
        store
            .register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 900)
            .await
            .expect("process");
        insert_thread(&store, "coverage").await;
        let operation = operation(process, "coverage", OperationKind::ModelRequest);
        store.begin_operation(&operation).await.expect("operation");
        if model_request {
            record_request(&store, &operation).await;
        }
        if terminal {
            store
                .finish_operation(&TerminalOperation {
                    operation_id: operation.id,
                    status: TerminalStatus::Completed,
                    occurred_at_ms: 1_100,
                    duration_ns: 100_000_000,
                    error_category: None,
                })
                .await
                .expect("terminal");
        }
        store
            .record_coverage(&NewCoverageEvent {
                event_id: FactEventId::new(),
                operation_id: operation_id.then_some(operation.id),
                scope_kind: CoverageScopeKind::new(scope_kind).expect("scope"),
                state,
                reason_code: reason_code.map(|code| CoverageReasonCode::new(code).expect("reason")),
                occurred_at_ms: 1_100,
            })
            .await
            .expect("coverage");
        crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
            .await
            .expect("warm cache");
        assert!(
            crate::report_cache::is_ready(&store.pool)
                .await
                .expect("cache ready")
        );
        let cached = store
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("cached summary");
        assert!(cached.coverage.has_gaps);
        assert_eq!(cached.coverage.unfinished_operations, expected_unfinished);
        sqlx::query("DELETE FROM _usage_report_cache_meta")
            .execute(&store.pool)
            .await
            .expect("canonical comparison");
        let canonical = store
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("canonical summary");
        assert_eq!(cached, canonical);
    }

    for (scope_kind, operation_kind, provider_tokens) in [
        ("model_attempt", OperationKind::ModelRequest, Some(25)),
        ("tool_attempt", OperationKind::LocalTool, None),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = UsageStore::open(temp.path()).await.expect("store");
        let process = ProcessId::new();
        store
            .register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 900)
            .await
            .expect("process");
        insert_thread(&store, "resolved").await;
        let operation = operation(process, "resolved", operation_kind);
        if let Some(value) = provider_tokens {
            let request = record_request(&store, &operation).await;
            store
                .record_token_observation(&token(
                    request,
                    RepositoryBucket::Unknown,
                    Some(value),
                    CoverageState::Complete,
                ))
                .await
                .expect("provider total");
        } else {
            store
                .begin_operation(&operation)
                .await
                .expect("tool operation");
            store
                .record_tool_invocation(&NewToolInvocation {
                    id: ToolInvocationId::new(),
                    operation_id: operation.id,
                    operation_kind,
                    tool_kind: ToolKind::new("builtin").expect("kind"),
                    safe_tool_name: ToolName::new("apply_patch").expect("name"),
                    operation_family: OperationFamily::new("filesystem").expect("family"),
                    observation_timing: ObservationTiming::new("runtime").expect("timing"),
                    covering_model_request_id: None,
                    execution_group_id: None,
                    execution_role: ToolExecutionRole::Standalone,
                })
                .await
                .expect("tool invocation");
        }
        store
            .finish_operation(&TerminalOperation {
                operation_id: operation.id,
                status: TerminalStatus::Completed,
                occurred_at_ms: 1_100,
                duration_ns: 100_000_000,
                error_category: None,
            })
            .await
            .expect("terminal");
        store
            .record_coverage(&NewCoverageEvent {
                event_id: FactEventId::new(),
                operation_id: Some(operation.id),
                scope_kind: CoverageScopeKind::new(scope_kind).expect("scope"),
                state: CoverageState::Partial,
                reason_code: None,
                occurred_at_ms: 1_100,
            })
            .await
            .expect("resolved coverage");
        crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
            .await
            .expect("warm resolved cache");
        let cached = store
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("resolved cached summary");
        assert!(!cached.coverage.has_gaps);
        sqlx::query("DELETE FROM _usage_report_cache_meta")
            .execute(&store.pool)
            .await
            .expect("resolved canonical comparison");
        let canonical = store
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("resolved canonical summary");
        assert_eq!(cached, canonical);
        assert!(!canonical.coverage.has_gaps);
    }
}
