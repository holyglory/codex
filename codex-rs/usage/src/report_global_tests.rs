use super::*;
use crate::ActivitySpanEventKind;
use crate::NewActivitySpan;
use crate::NewActivitySpanEvent;
use pretty_assertions::assert_eq;

#[cfg(unix)]
#[tokio::test]
async fn global_component_initializes_and_reopens_from_core_schema() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("global worker");
    assert!(store.report_cache_status().await.expect("status").ready);
    store.report_refresh.cancel();
    sqlx::raw_sql(
        "DROP TRIGGER IF EXISTS _usage_report_global_operation_insert;
         DROP TRIGGER IF EXISTS _usage_report_global_operation_terminal;
         DROP TRIGGER IF EXISTS _usage_report_global_token;
         DROP TRIGGER IF EXISTS _usage_report_global_coverage;
         DROP TRIGGER IF EXISTS _usage_report_global_classification;
         DROP TRIGGER IF EXISTS _usage_report_global_gap;
         DROP TRIGGER IF EXISTS _usage_report_global_span_insert;
         DROP TRIGGER IF EXISTS _usage_report_global_span_end;
         DROP TABLE IF EXISTS _usage_report_global_gap_events;
         DROP TABLE IF EXISTS _usage_report_global_gap_v9;
         DROP TABLE IF EXISTS _usage_report_global_tools;
         DROP TABLE IF EXISTS _usage_report_global_time;
         DROP TABLE IF EXISTS _usage_report_global_dirty_hour_ranges;
         DROP TABLE IF EXISTS _usage_report_global_request_hours;
         DROP TABLE IF EXISTS _usage_report_global_request;
         DROP TABLE IF EXISTS _usage_report_global_unpriced_meta;
         DROP TABLE IF EXISTS _usage_report_global_unresolved_meta;
         DROP TABLE IF EXISTS _usage_report_global_unpriced;
         DROP TABLE IF EXISTS _usage_report_global_unresolved;
         DROP TABLE IF EXISTS _usage_report_global_dirty_hours;
         DROP TABLE IF EXISTS _usage_report_global_dirty_operations;
         DROP TABLE IF EXISTS _usage_report_global_classifications;
         DROP TABLE IF EXISTS _usage_report_global_counts;
         DROP TABLE IF EXISTS _usage_report_global_operations;
         DROP TABLE IF EXISTS _usage_report_global_meta;",
    )
    .execute(&store.pool)
    .await
    .expect("remove v9 derived component");
    store.close().await;
    let reopened = UsageStore::open(temp.path()).await.expect("reopen");
    crate::report_cache::ensure(&reopened.pool, &reopened.report_refresh.reader)
        .await
        .expect("rebuild global component");
    assert!(
        reopened
            .report_cache_status()
            .await
            .expect("reopened status")
            .ready
    );
}

#[cfg(unix)]
#[tokio::test]
async fn global_component_upgrade_rebuilds_only_derived_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("global worker");
    sqlx::query(
        "UPDATE _usage_report_global_meta SET component_version=0, ready=1 WHERE singleton=1",
    )
    .execute(&store.pool)
    .await
    .expect("mark old component");
    crate::report_cache::prepare(&store.pool)
        .await
        .expect("upgrade component");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT component_version FROM _usage_report_global_meta WHERE singleton=1",
        )
        .fetch_one(&store.pool)
        .await
        .expect("component version"),
        crate::report_global::COMPONENT_VERSION
    );
    let mut connection = store.pool.acquire().await.expect("status connection");
    assert!(
        !crate::report_global::is_ready_on(&mut connection)
            .await
            .expect("upgraded readiness")
    );
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("rebuild upgraded component");
    assert!(store.report_cache_status().await.expect("status").ready);
}

#[cfg(unix)]
#[tokio::test]
async fn global_gap_uses_latest_null_operation_coverage_event() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("global worker");
    sqlx::query(
        "INSERT INTO coverage_events(event_id,operation_id,scope_kind,coverage_state,reason_code,occurred_at_ms) VALUES ('global-gap-partial',NULL,'global','partial',NULL,100)",
    )
    .execute(&store.pool)
    .await
    .expect("partial global gap");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("apply partial gap");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT has_gap FROM _usage_report_global_gap_v9 WHERE singleton=1",
        )
        .fetch_one(&store.pool)
        .await
        .expect("partial gap state"),
        1
    );
    sqlx::query(
        "INSERT INTO coverage_events(event_id,operation_id,scope_kind,coverage_state,reason_code,occurred_at_ms) VALUES ('global-gap-complete',NULL,'global','complete',NULL,200)",
    )
    .execute(&store.pool)
    .await
    .expect("complete global coverage");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("apply complete gap");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT has_gap FROM _usage_report_global_gap_v9 WHERE singleton=1",
        )
        .fetch_one(&store.pool)
        .await
        .expect("complete gap state"),
        0
    );
}

#[cfg(unix)]
#[tokio::test]
async fn global_hour_timing_matches_canonical_for_waits_unknown_and_zero_tools() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 0)
        .await
        .expect("process");
    insert_thread(&store, "timing-thread").await;

    let mut request_operation = operation(process_id, "timing-thread", OperationKind::ModelRequest);
    request_operation.started_at_ms = 1_000;
    record_request(&store, &request_operation).await;
    store
        .finish_operation(&TerminalOperation {
            operation_id: request_operation.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 5_000,
            duration_ns: 4_000_000_000,
            error_category: None,
        })
        .await
        .expect("finish request");
    let wait_id = ActivitySpanId::new();
    store
        .begin_activity_span(&NewActivitySpan {
            id: wait_id,
            operation_id: request_operation.id,
            activity_state: ActivityState::UserWait,
            started_at_ms: 2_000,
        })
        .await
        .expect("begin wait");
    store
        .record_activity_span_event(&NewActivitySpanEvent {
            event_id: FactEventId::new(),
            activity_span_id: wait_id,
            kind: ActivitySpanEventKind::Ended,
            occurred_at_ms: 3_000,
        })
        .await
        .expect("end wait");

    let mut tool_operation = operation(process_id, "timing-thread", OperationKind::LocalTool);
    tool_operation.started_at_ms = 1_500;
    store
        .begin_operation(&tool_operation)
        .await
        .expect("begin tool");
    store
        .finish_operation(&TerminalOperation {
            operation_id: tool_operation.id,
            status: TerminalStatus::Failed,
            occurred_at_ms: 3_500,
            duration_ns: 2_000_000_000,
            error_category: None,
        })
        .await
        .expect("finish tool");

    let mut zero_tool = operation(process_id, "timing-thread", OperationKind::LocalTool);
    zero_tool.started_at_ms = 6_000;
    store
        .begin_operation(&zero_tool)
        .await
        .expect("begin zero tool");
    store
        .finish_operation(&TerminalOperation {
            operation_id: zero_tool.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 6_000,
            duration_ns: 0,
            error_category: None,
        })
        .await
        .expect("finish zero tool");

    let mut open_operation = operation(process_id, "timing-thread", OperationKind::ModelRequest);
    open_operation.started_at_ms = 7_000;
    let _open_request = record_request(&store, &open_operation).await;

    store.report_refresh.cancel();
    let all = UsageSummaryQuery {
        worktree_ids: None,
        include_unassigned_worktrees: true,
        thread_id: None,
        repository_id: None,
        account_profile_ref: None,
        time_range: None,
    };
    let mut oracle = store.pool.acquire().await.expect("canonical connection");
    let canonical = store
        .read_usage_summary(all, &mut oracle)
        .await
        .expect("canonical timing");
    drop(oracle);
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("derived timing");
    let derived = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("global timing");
    assert_eq!(derived.timing, canonical.timing);
    assert_eq!(derived.tools, canonical.tools);
}

#[cfg(unix)]
#[tokio::test]
async fn global_worktree_branch_matches_canonical_assignments() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 43, /*started_at_ms*/ 0)
        .await
        .expect("process");
    insert_thread(&store, "worktree-thread").await;
    let repository = store
        .resolve_repository(
            &identity("/global-worktree-repository"),
            &label("global-worktree-repository"),
            /*observed_at_ms*/ 1,
        )
        .await
        .expect("repository");
    let worktree = WorktreeId::new("a".repeat(64)).expect("worktree");
    store
        .ensure_worktree(&worktree, &repository, &label("global-worktree"), 1)
        .await
        .expect("worktree");
    let mut operation = operation(process_id, "worktree-thread", OperationKind::ModelRequest);
    operation.started_at_ms = 1_000;
    let request = record_request(&store, &operation).await;
    store
        .finish_operation(&TerminalOperation {
            operation_id: operation.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 2_000,
            duration_ns: 1_000_000_000,
            error_category: None,
        })
        .await
        .expect("finish operation");
    store
        .record_repository_attribution(&NewRepositoryAttribution {
            event_id: FactEventId::new(),
            operation_id: operation.id,
            repository_id: Some(repository.clone()),
            worktree_id: Some(worktree.clone()),
            kind: RepositoryAttributionKind::Primary,
            provenance: RepositoryAttributionProvenance::RuntimeObserved,
            occurred_at_ms: 1_001,
        })
        .await
        .expect("worktree attribution");
    store
        .record_token_observation(&token(
            request,
            RepositoryBucket::Single(repository),
            Some(9),
            CoverageState::Complete,
        ))
        .await
        .expect("worktree token");

    store.report_refresh.cancel();
    let all = UsageSummaryQuery {
        worktree_ids: None,
        include_unassigned_worktrees: true,
        thread_id: None,
        repository_id: None,
        account_profile_ref: None,
        time_range: None,
    };
    let mut oracle = store.pool.acquire().await.expect("canonical connection");
    let canonical = store
        .read_usage_summary(all, &mut oracle)
        .await
        .expect("canonical worktree");
    drop(oracle);
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("derived worktree");
    let derived = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("global worktree");
    assert_eq!(derived.worktrees, canonical.worktrees);
    assert_eq!(derived.worktree_coverage, canonical.worktree_coverage);
}

#[cfg(unix)]
#[tokio::test]
async fn global_counters_follow_terminal_classification_and_late_token_facts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 44, /*started_at_ms*/ 0)
        .await
        .expect("process");
    insert_thread(&store, "correction-thread").await;
    let mut operation = operation(process_id, "correction-thread", OperationKind::ModelRequest);
    operation.started_at_ms = 1_000;
    let request = record_request(&store, &operation).await;
    store
        .record_token_observation(&token(
            request,
            RepositoryBucket::Unknown,
            Some(3),
            CoverageState::Complete,
        ))
        .await
        .expect("initial token");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("initial global cache");
    store
        .finish_operation(&TerminalOperation {
            operation_id: operation.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 2_000,
            duration_ns: 1_000_000_000,
            error_category: None,
        })
        .await
        .expect("late terminal");
    store
        .record_classification(&NewClassificationEvent {
            event_id: FactEventId::new(),
            operation_id: operation.id,
            phase: Phase::Testing,
            activity: Activity::UnitTesting,
            activity_state: ActivityState::ModelActive,
            provenance: AttributionProvenance::UserCorrected,
            supersedes_event_id: None,
            occurred_at_ms: 2_001,
        })
        .await
        .expect("late classification");
    store
        .record_token_observation(&token(
            request,
            RepositoryBucket::Unknown,
            Some(4),
            CoverageState::Complete,
        ))
        .await
        .expect("late token");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("rebuild corrections");
    let global = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("global correction summary");
    assert_eq!(global.coverage.unfinished_operations, 0);
    assert_eq!(global.operation_count, 1);
    assert_eq!(global.model_request_count, 1);
    assert_eq!(
        global
            .tokens
            .iter()
            .map(|token| token.measured_tokens)
            .sum::<i64>(),
        7
    );
    assert_eq!(
        global
            .classifications
            .iter()
            .find(|classification| classification.phase == "testing")
            .map(|classification| classification.count),
        Some(1)
    );
    let mut oracle = store
        .pool
        .acquire()
        .await
        .expect("canonical counter connection");
    let canonical = store
        .read_usage_summary(
            UsageSummaryQuery {
                worktree_ids: None,
                include_unassigned_worktrees: true,
                thread_id: None,
                repository_id: None,
                account_profile_ref: None,
                time_range: Some(UtcTimeRange::new(0, 3_000).expect("finite range")),
            },
            &mut oracle,
        )
        .await
        .expect("canonical counter summary");
    assert_eq!(global.operation_count, canonical.operation_count);
    assert_eq!(global.model_request_count, canonical.model_request_count);
    assert_eq!(global.classifications, canonical.classifications);
}
