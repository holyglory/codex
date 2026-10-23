use super::*;
use crate::ActivitySpanEventKind;
use crate::NewActivitySpan;
use crate::NewActivitySpanEvent;
use pretty_assertions::assert_eq;

type RequestHour = (i64, Option<i64>, Option<i64>, i64, i64);

#[derive(Debug, Eq, PartialEq)]
struct GlobalProjectionSnapshot {
    classifications: Vec<(String, String, String, i64)>,
    unresolved: Vec<(String, String, i64)>,
    unpriced: Vec<String>,
    gap: i64,
    gap_events: Vec<(String, i64, String)>,
    time: Vec<(i64, String, String, i64, i64, i64, i64)>,
    tools: Vec<(i64, String, i64, i64, i64)>,
    request: (Option<i64>, Option<i64>, i64, i64),
    request_hours: Vec<RequestHour>,
}

async fn global_projection_snapshot(pool: &sqlx::SqlitePool) -> GlobalProjectionSnapshot {
    GlobalProjectionSnapshot {
        classifications: sqlx::query_as(
            "SELECT phase,activity,provenance,operation_count FROM _usage_report_global_classifications ORDER BY phase,activity,provenance",
        )
        .fetch_all(pool)
        .await
        .expect("global classifications"),
        unresolved: sqlx::query_as(
            "SELECT operation_id,scope_kind,unresolved FROM _usage_report_global_unresolved ORDER BY operation_id,scope_kind",
        )
        .fetch_all(pool)
        .await
        .expect("global unresolved projection"),
        unpriced: sqlx::query_scalar(
            "SELECT operation_id FROM _usage_report_global_unpriced ORDER BY operation_id",
        )
        .fetch_all(pool)
        .await
        .expect("global unpriced projection"),
        gap: sqlx::query_scalar(
            "SELECT has_gap FROM _usage_report_global_gap_v9 WHERE singleton=1",
        )
        .fetch_one(pool)
        .await
        .expect("global gap projection"),
        gap_events: sqlx::query_as(
            "SELECT event_id,occurred_at_ms,coverage_state FROM _usage_report_global_gap_events ORDER BY occurred_at_ms,event_id",
        )
        .fetch_all(pool)
        .await
        .expect("global gap events"),
        time: sqlx::query_as(
            "SELECT hour_index,kind,label,measured_ms,unknown_intervals,zero_intervals,interval_count FROM _usage_report_global_time ORDER BY hour_index,kind,label",
        )
        .fetch_all(pool)
        .await
        .expect("global time projection"),
        tools: sqlx::query_as(
            "SELECT hour_index,outcome,measured_ms,unknown_intervals,operation_count FROM _usage_report_global_tools ORDER BY hour_index,outcome",
        )
        .fetch_all(pool)
        .await
        .expect("global tools projection"),
        request: sqlx::query_as(
            "SELECT min_start_ms,max_end_ms,unknown_intervals,request_count FROM _usage_report_global_request WHERE singleton=1",
        )
        .fetch_one(pool)
        .await
        .expect("global request projection"),
        request_hours: sqlx::query_as(
            "SELECT hour_index,min_start_ms,max_end_ms,unknown_intervals,request_count FROM _usage_report_global_request_hours ORDER BY hour_index",
        )
        .fetch_all(pool)
        .await
        .expect("global request hours"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn global_component_initializes_and_reopens_from_core_schema() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 41, /*started_at_ms*/ 0)
        .await
        .expect("process");
    insert_thread(&store, "reopen-thread").await;
    let mut operation = operation(process_id, "reopen-thread", OperationKind::ModelRequest);
    operation.started_at_ms = 1_000;
    record_request(&store, &operation).await;
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
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("global worker");
    let status = store.report_cache_status().await.expect("status");
    assert!(status.ready, "unexpected initial status: {status:?}");
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
         DROP TABLE IF EXISTS _usage_report_global_seed;
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
    assert_eq!(
        reopened
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("reopened summary")
            .operation_count,
        1
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
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 46, /*started_at_ms*/ 0)
        .await
        .expect("process");
    insert_thread(&store, "upgrade-thread").await;
    let mut operation = operation(process_id, "upgrade-thread", OperationKind::ModelRequest);
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
        .record_token_observation(&token(
            request,
            RepositoryBucket::Unknown,
            Some(5),
            CoverageState::Complete,
        ))
        .await
        .expect("token");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("refresh populated component");
    let before = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("before upgrade");
    store.report_refresh.cancel();
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
    assert_eq!(
        store
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("after upgrade"),
        before
    );
}

#[cfg(unix)]
#[tokio::test]
async fn v2_global_upgrade_preserves_partial_queues_and_aggregates() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("initial global worker");
    store.report_refresh.cancel();

    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 48, /*started_at_ms*/ 0)
        .await
        .expect("process");
    insert_thread(&store, "v2-upgrade-thread").await;
    let mut applied = operation(process_id, "v2-upgrade-thread", OperationKind::LocalTool);
    applied.started_at_ms = 1_000;
    store
        .begin_operation(&applied)
        .await
        .expect("applied operation");
    store
        .finish_operation(&TerminalOperation {
            operation_id: applied.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 2_000,
            duration_ns: 1_000_000,
            error_category: None,
        })
        .await
        .expect("applied terminal");
    let mut model = operation(process_id, "v2-upgrade-thread", OperationKind::ModelRequest);
    model.started_at_ms = 3_000;
    record_request(&store, &model).await;
    store
        .finish_operation(&TerminalOperation {
            operation_id: model.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 4_000,
            duration_ns: 1_000_000,
            error_category: None,
        })
        .await
        .expect("model terminal");
    sqlx::query(
        "INSERT INTO coverage_events(event_id,operation_id,scope_kind,coverage_state,reason_code,occurred_at_ms) VALUES ('v2-partial-coverage',?,'model_attempt','partial',NULL,4_000), ('v2-global-gap',NULL,'global','partial',NULL,5_000)",
    )
    .bind(model.id.as_string())
    .execute(&store.pool)
    .await
    .expect("coverage projections");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("apply v2 aggregate");

    let mut queued = operation(process_id, "v2-upgrade-thread", OperationKind::LocalTool);
    queued.started_at_ms = 7_200_000;
    store
        .begin_operation(&queued)
        .await
        .expect("queued operation");
    let before_meta = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64)>(
        "SELECT component_version,core_schema_version,core_generation,ready,dirty_revision,applied_revision FROM _usage_report_global_meta WHERE singleton=1",
    )
    .fetch_one(&store.pool)
    .await
    .expect("v2 metadata");
    let before_counts = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT operations,model_requests,unfinished_operations FROM _usage_report_global_counts WHERE singleton=1",
    )
    .fetch_one(&store.pool)
    .await
    .expect("v2 aggregates");
    let before_operations: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT operation_id,operation_kind,terminal_status FROM _usage_report_global_operations ORDER BY operation_id",
    )
    .fetch_all(&store.pool)
    .await
    .expect("v2 operation projection");
    let before_dirty_operations: Vec<String> = sqlx::query_scalar(
        "SELECT operation_id FROM _usage_report_global_dirty_operations ORDER BY operation_id",
    )
    .fetch_all(&store.pool)
    .await
    .expect("v2 operation queue");
    let before_dirty_hours: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT start_hour,end_hour FROM _usage_report_global_dirty_hour_ranges ORDER BY start_hour",
    )
    .fetch_all(&store.pool)
    .await
    .expect("v2 hour queue");
    let before_projection = global_projection_snapshot(&store.pool).await;
    assert_eq!(before_meta.0, crate::report_global::COMPONENT_VERSION);
    assert_eq!(before_operations.len(), 2);
    assert!(!before_projection.classifications.is_empty());
    assert!(!before_projection.unresolved.is_empty());
    assert!(!before_projection.unpriced.is_empty());
    assert_eq!(before_projection.gap, 1);
    assert!(!before_projection.gap_events.is_empty());
    assert!(!before_projection.time.is_empty());
    assert!(!before_projection.tools.is_empty());
    assert!(before_projection.request.3 > 0);
    assert!(!before_projection.request_hours.is_empty());
    assert!(before_dirty_operations.contains(&queued.id.as_string()));
    assert!(
        before_dirty_hours
            .iter()
            .any(|(start, end)| *start == 2 && *end == 2)
    );

    // A v2 cache had no frontier table. Dropping it also models a rollback
    // from the older binary after a newer binary had previously opened the
    // same database and left additive metadata behind.
    sqlx::raw_sql("DROP TABLE _usage_report_global_seed;")
        .execute(&store.pool)
        .await
        .expect("remove v3 frontier");
    sqlx::query("UPDATE _usage_report_global_meta SET component_version=2")
        .execute(&store.pool)
        .await
        .expect("mark v2 component");

    let blocked_refresh = store
        .report_refresh
        .reader
        .acquire()
        .await
        .expect("hold refresh admission");
    store.close().await;
    let reopened = UsageStore::open(temp.path())
        .await
        .expect("reopen v2 cache");
    assert_eq!(
        sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
            "SELECT core_schema_version,core_generation,ready,dirty_revision,applied_revision FROM _usage_report_global_meta WHERE singleton=1",
        )
        .fetch_one(&reopened.pool)
        .await
        .expect("preserved metadata"),
        (
            before_meta.1,
            before_meta.2,
            before_meta.3,
            before_meta.4,
            before_meta.5,
        )
    );
    assert_eq!(
        sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT operations,model_requests,unfinished_operations FROM _usage_report_global_counts WHERE singleton=1",
        )
        .fetch_one(&reopened.pool)
        .await
        .expect("preserved aggregates"),
        before_counts
    );
    assert_eq!(
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT operation_id,operation_kind,terminal_status FROM _usage_report_global_operations ORDER BY operation_id",
        )
        .fetch_all(&reopened.pool)
        .await
        .expect("preserved operation projection"),
        before_operations
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT operation_id FROM _usage_report_global_dirty_operations ORDER BY operation_id",
        )
        .fetch_all(&reopened.pool)
        .await
        .expect("preserved operation queue"),
        before_dirty_operations
    );
    assert_eq!(
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT start_hour,end_hour FROM _usage_report_global_dirty_hour_ranges ORDER BY start_hour",
        )
        .fetch_all(&reopened.pool)
        .await
        .expect("preserved hour queue"),
        before_dirty_hours
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT component_version FROM _usage_report_global_meta WHERE singleton=1",
        )
        .fetch_one(&reopened.pool)
        .await
        .expect("upgraded version"),
        crate::report_global::COMPONENT_VERSION
    );
    assert_eq!(
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT cursor,high_water FROM _usage_report_global_seed ORDER BY source",
        )
        .fetch_all(&reopened.pool)
        .await
        .expect("completed v3 frontiers")
        .iter()
        .filter(|(cursor, high_water)| cursor != high_water)
        .count(),
        0
    );
    assert_eq!(
        global_projection_snapshot(&reopened.pool).await,
        before_projection
    );
    // A v2 binary leaves this additive table behind when it rolls a v3 cache
    // back to its own schema. Re-upgrading must replace stale frontiers too.
    sqlx::query("UPDATE _usage_report_global_meta SET component_version=2")
        .execute(&reopened.pool)
        .await
        .expect("mark rolled-back component");
    sqlx::query("UPDATE _usage_report_global_seed SET cursor=0, high_water=0")
        .execute(&reopened.pool)
        .await
        .expect("stale rolled-back frontier");
    crate::report_cache::prepare(&reopened.pool)
        .await
        .expect("replace rolled-back frontier");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM _usage_report_global_seed WHERE cursor <> high_water",
        )
        .fetch_one(&reopened.pool)
        .await
        .expect("rolled-back frontier state"),
        0
    );
    drop(blocked_refresh);
    reopened.report_refresh.cancel();
    crate::report_cache::ensure(&reopened.pool, &reopened.report_refresh.reader)
        .await
        .expect("resume queued v2 work");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _usage_report_global_dirty_operations",)
            .fetch_one(&reopened.pool)
            .await
            .expect("drained operation queue"),
        0
    );
}

#[cfg(unix)]
#[tokio::test]
async fn newer_global_component_is_preserved_for_newer_reader() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("initial global worker");
    store.report_refresh.cancel();
    sqlx::query(
        "UPDATE _usage_report_global_meta SET component_version=?, ready=1 WHERE singleton=1",
    )
    .bind(crate::report_global::COMPONENT_VERSION + 1)
    .execute(&store.pool)
    .await
    .expect("mark future global component");
    sqlx::query(
        "UPDATE _usage_report_global_counts SET operations=17, model_requests=11 WHERE singleton=1",
    )
    .execute(&store.pool)
    .await
    .expect("future aggregate marker");
    assert!(
        !crate::report_cache::prepare(&store.pool)
            .await
            .expect("future prepare")
    );
    assert!(
        !crate::report_global::is_supported(&store.pool)
            .await
            .expect("future support")
    );
    let started = std::time::Instant::now();
    assert!(matches!(
        store.require_report_ready(/*range*/ None).await,
        Err(UsageStoreError::ReportWarming(_))
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("future component remains untouched");
    assert_eq!(
        sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT component_version,operations,model_requests FROM _usage_report_global_meta CROSS JOIN _usage_report_global_counts WHERE _usage_report_global_meta.singleton=1 AND _usage_report_global_counts.singleton=1",
        )
        .fetch_one(&store.pool)
        .await
        .expect("future component marker"),
        (
            crate::report_global::COMPONENT_VERSION + 1,
            17,
            11,
        )
    );
}

#[cfg(unix)]
#[tokio::test]
async fn finite_summary_uses_core_cache_while_global_component_warms() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("initial cache");
    store.report_refresh.cancel();
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 45, /*started_at_ms*/ 0)
        .await
        .expect("process");
    insert_thread(&store, "finite-warming-thread").await;
    let mut operation = operation(
        process_id,
        "finite-warming-thread",
        OperationKind::ModelRequest,
    );
    operation.started_at_ms = 1_000;
    record_request(&store, &operation).await;
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
    let mut connection = store.pool.acquire().await.expect("connection");
    let summary = store
        .read_usage_summary(
            UsageSummaryQuery {
                worktree_ids: None,
                include_unassigned_worktrees: true,
                thread_id: None,
                repository_id: None,
                account_profile_ref: None,
                time_range: Some(
                    UtcTimeRange::new(/*start_ms*/ 0, /*end_ms*/ 3_000).expect("finite range"),
                ),
            },
            &mut connection,
        )
        .await
        .expect("finite summary");
    assert_eq!(summary.operation_count, 1);
    assert!(
        !crate::report_global::is_ready_on(&mut connection)
            .await
            .expect("global status")
    );
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

    let mut crossing_operation =
        operation(process_id, "timing-thread", OperationKind::ModelRequest);
    crossing_operation.started_at_ms = 3_599_000;
    record_request(&store, &crossing_operation).await;
    store
        .finish_operation(&TerminalOperation {
            operation_id: crossing_operation.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 3_601_000,
            duration_ns: 2_000_000_000,
            error_category: None,
        })
        .await
        .expect("finish crossing request");

    let mut skewed_operation = operation(process_id, "timing-thread", OperationKind::ModelRequest);
    skewed_operation.started_at_ms = 9_000;
    record_request(&store, &skewed_operation).await;
    store
        .finish_operation(&TerminalOperation {
            operation_id: skewed_operation.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 8_000,
            duration_ns: 0,
            error_category: None,
        })
        .await
        .expect("finish skewed request");

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
        .ensure_worktree(
            &worktree,
            &repository,
            &label("global-worktree"),
            /*observed_at_ms*/ 1,
        )
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
                time_range: Some(
                    UtcTimeRange::new(/*start_ms*/ 0, /*end_ms*/ 3_000).expect("finite range"),
                ),
            },
            &mut oracle,
        )
        .await
        .expect("canonical counter summary");
    assert_eq!(global.operation_count, canonical.operation_count);
    assert_eq!(global.model_request_count, canonical.model_request_count);
    assert_eq!(global.classifications, canonical.classifications);
}
