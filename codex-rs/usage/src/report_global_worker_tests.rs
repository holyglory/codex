use super::*;

#[cfg(unix)]
#[tokio::test]
async fn hour_rebuild_rollback_preserves_queue_for_resume() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = crate::UsageStore::open(temp.path()).await.expect("store");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("initial global cache");
    store.report_refresh.cancel();

    let mut tx = store.pool.begin().await.expect("mark dirty hour");
    sqlx::query(
        "INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) VALUES (0,2),(1,3)",
    )
    .execute(&mut *tx)
    .await
    .expect("dirty hour");
    sqlx::query(
        "INSERT INTO _usage_report_global_dirty_operations(operation_id) VALUES ('missing-operation')",
    )
    .execute(&mut *tx)
    .await
    .expect("dirty operation");
    sqlx::query(
        "UPDATE _usage_report_global_meta SET dirty_revision=dirty_revision+1 WHERE singleton=1",
    )
    .execute(&mut *tx)
    .await
    .expect("dirty revision");
    tx.commit().await.expect("commit dirty marker");

    let mut tx = store.pool.begin().await.expect("start hour rebuild");
    rebuild_hour(&mut tx, /*hour*/ 0)
        .await
        .expect("rebuild hour");
    tx.rollback().await.expect("rollback hour rebuild");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM _usage_report_global_dirty_hour_ranges WHERE start_hour=0",
        )
        .fetch_one(&store.pool)
        .await
        .expect("dirty marker after rollback"),
        1
    );

    sqlx::query(
        "INSERT INTO coverage_events(event_id,operation_id,scope_kind,coverage_state,reason_code,occurred_at_ms) VALUES ('capture-after-global-rollback',NULL,'global','complete',NULL,1)",
    )
    .execute(&store.pool)
    .await
    .expect("capture after rollback");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM coverage_events WHERE event_id='capture-after-global-rollback'",
        )
        .fetch_one(&store.pool)
        .await
        .expect("captured fact"),
        1
    );

    assert!(
        !crate::report_global::step(&store.pool)
            .await
            .expect("resume hour")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM _usage_report_global_dirty_hour_ranges WHERE start_hour=0",
        )
        .fetch_one(&store.pool)
        .await
        .expect("dirty marker after resume"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM _usage_report_global_dirty_operations WHERE operation_id='missing-operation'",
        )
        .fetch_one(&store.pool)
        .await
        .expect("dirty operation after concurrent resume"),
        0
    );
    for _ in 0..3 {
        assert!(
            !crate::report_global::step(&store.pool)
                .await
                .expect("drain overlapping hours")
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM _usage_report_global_dirty_hour_ranges",
        )
        .fetch_one(&store.pool)
        .await
        .expect("overlapping hours drained"),
        0
    );
    assert!(
        crate::report_global::step(&store.pool)
            .await
            .expect("finish global cache")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sustained_hour_rebuild_does_not_starve_operation_queue() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = crate::UsageStore::open(temp.path()).await.expect("store");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("initial global cache");
    store.report_refresh.cancel();

    let process_id = crate::ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 43, /*started_at_ms*/ 0)
        .await
        .expect("process");
    sqlx::query(
        "INSERT INTO threads(id,parent_thread_id,source_kind,created_at_ms) VALUES ('fairness-thread',NULL,'cli',0)",
    )
    .execute(&store.pool)
    .await
    .expect("thread");
    sqlx::query(
        "INSERT INTO operations(id,process_id,thread_id,turn_id,agent_id,parent_operation_id,retry_of_operation_id,rework_of_operation_id,operation_kind,started_at_ms,taxonomy_version,phase,activity,activity_state,attribution_provenance) VALUES ('starving-operation',?,'fairness-thread',NULL,NULL,NULL,NULL,NULL,'local_tool',0,1,'implementation','coding','tool_active','agent_declared')",
    )
    .bind(process_id.as_string())
    .execute(&store.pool)
    .await
    .expect("operation");

    for _ in 0..4 {
        sqlx::query(
            "INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) VALUES (0,0) ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour)",
        )
        .execute(&store.pool)
        .await
        .expect("hour capture");
        sqlx::query(
            "UPDATE _usage_report_global_meta SET dirty_revision=dirty_revision+1 WHERE singleton=1",
        )
        .execute(&store.pool)
        .await
        .expect("dirty revision");
        assert!(
            !crate::report_global::step(&store.pool)
                .await
                .expect("refresh step")
        );
    }

    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM _usage_report_global_dirty_operations WHERE operation_id='starving-operation'",
        )
        .fetch_one(&store.pool)
        .await
        .expect("operation queue"),
        0
    );
    assert!(
        crate::report_global::step(&store.pool)
            .await
            .expect("finish global cache")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ready_global_refreshes_after_capture_before_unbounded_report() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = crate::UsageStore::open(temp.path()).await.expect("store");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("initial global cache");
    assert!(
        crate::report_cache::is_global_ready(&store.pool)
            .await
            .expect("initial readiness")
    );
    store.report_refresh.cancel();

    let process_id = crate::ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 44, /*started_at_ms*/ 0)
        .await
        .expect("process");
    sqlx::query(
        "INSERT INTO threads(id,parent_thread_id,source_kind,created_at_ms) VALUES ('post-ready-thread',NULL,'cli',0)",
    )
    .execute(&store.pool)
    .await
    .expect("thread");
    store
        .require_report_ready(None)
        .await
        .expect("ready guard before capture");
    store
        .begin_operation(&crate::NewOperation {
            work_context: None,
            id: crate::OperationId::new(),
            process_id,
            thread_id: Some(crate::ThreadId::new("post-ready-thread").expect("thread id")),
            turn_id: None,
            agent_id: None,
            parent_operation_id: None,
            retry_of_operation_id: None,
            rework_of_operation_id: None,
            kind: crate::OperationKind::LocalTool,
            started_at_ms: 1_000,
            phase: crate::Phase::Implementation,
            activity: crate::Activity::Coding,
            activity_state: crate::ActivityState::ToolActive,
            attribution_provenance: crate::AttributionProvenance::AgentDeclared,
        })
        .await
        .expect("operation");
    assert!(
        !crate::report_cache::is_global_ready(&store.pool)
            .await
            .expect("dirty readiness")
    );

    let query = crate::UsageSummaryQuery {
        worktree_ids: None,
        include_unassigned_worktrees: true,
        thread_id: None,
        repository_id: None,
        account_profile_ref: None,
        time_range: None,
    };
    assert!(matches!(
        store.usage_summary_query_strict(query.clone()).await,
        Err(crate::UsageStoreError::ReportWarming(_))
    ));

    store
        .require_report_ready(None)
        .await
        .expect("first bounded catch-up");
    let summary = store
        .usage_summary_query_strict(query.clone())
        .await
        .expect("first unbounded summary after refresh");
    assert_eq!(summary.operation_count, 1);

    store
        .begin_operation(&crate::NewOperation {
            work_context: None,
            id: crate::OperationId::new(),
            process_id,
            thread_id: Some(crate::ThreadId::new("post-ready-thread").expect("thread id")),
            turn_id: None,
            agent_id: None,
            parent_operation_id: None,
            retry_of_operation_id: None,
            rework_of_operation_id: None,
            kind: crate::OperationKind::LocalTool,
            started_at_ms: 2_000,
            phase: crate::Phase::Implementation,
            activity: crate::Activity::Coding,
            activity_state: crate::ActivityState::ToolActive,
            attribution_provenance: crate::AttributionProvenance::AgentDeclared,
        })
        .await
        .expect("second operation");
    store
        .require_report_ready(None)
        .await
        .expect("second bounded catch-up");
    let summary = store
        .usage_summary_query_strict(query)
        .await
        .expect("second unbounded summary after refresh");
    assert_eq!(summary.operation_count, 2);
}
