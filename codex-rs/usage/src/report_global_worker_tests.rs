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

    store
        .require_report_ready(None)
        .await
        .expect("first bounded catch-up");
    let summary = store
        .usage_summary(crate::UsageSummaryScope::All)
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
        .usage_summary(crate::UsageSummaryScope::All)
        .await
        .expect("second unbounded summary after refresh");
    assert_eq!(summary.operation_count, 2);
}

#[cfg(unix)]
#[tokio::test]
async fn hour_rebuild_keeps_current_wait_span_for_old_open_operation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = crate::UsageStore::open(temp.path()).await.expect("store");
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("initial global cache");
    store.report_refresh.cancel();

    let process_id = crate::ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 45, /*started_at_ms*/ 0)
        .await
        .expect("process");
    sqlx::query(
        "INSERT INTO threads(id,parent_thread_id,source_kind,created_at_ms) VALUES ('lifetime-span-thread',NULL,'cli',0)",
    )
    .execute(&store.pool)
    .await
    .expect("thread");
    let operation = crate::NewOperation {
        work_context: None,
        id: crate::OperationId::new(),
        process_id,
        thread_id: Some(crate::ThreadId::new("lifetime-span-thread").expect("thread id")),
        turn_id: None,
        agent_id: None,
        parent_operation_id: None,
        retry_of_operation_id: None,
        rework_of_operation_id: None,
        kind: crate::OperationKind::LocalTool,
        started_at_ms: -1_000,
        phase: crate::Phase::Implementation,
        activity: crate::Activity::Coding,
        activity_state: crate::ActivityState::ToolActive,
        attribution_provenance: crate::AttributionProvenance::AgentDeclared,
    };
    store
        .begin_operation(&operation)
        .await
        .expect("old open operation");
    let span = crate::NewActivitySpan {
        id: crate::ActivitySpanId::new(),
        operation_id: operation.id,
        activity_state: crate::ActivityState::UserWait,
        started_at_ms: 1_000,
    };
    store.begin_activity_span(&span).await.expect("wait span");
    store
        .record_activity_span_event(&crate::NewActivitySpanEvent {
            event_id: crate::FactEventId::new(),
            activity_span_id: span.id,
            kind: crate::ActivitySpanEventKind::Ended,
            occurred_at_ms: 3_000,
        })
        .await
        .expect("end wait span");

    sqlx::query("UPDATE _usage_report_cache_meta SET ready=0 WHERE singleton=1")
        .execute(&store.pool)
        .await
        .expect("invalidate core cache for canonical oracle");
    let query = crate::UsageSummaryQuery {
        worktree_ids: None,
        include_unassigned_worktrees: true,
        thread_id: None,
        repository_id: None,
        account_profile_ref: None,
        time_range: None,
    };
    let mut connection = store.pool.acquire().await.expect("canonical connection");
    let canonical = store
        .read_usage_summary(query.clone(), &mut connection)
        .await
        .expect("canonical lifetime span summary");
    drop(connection);
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("bounded lifetime span refresh");
    let derived = store
        .usage_summary_query(query)
        .await
        .expect("derived lifetime span summary");
    assert_eq!(derived.timing, canonical.timing);
    assert_eq!(derived.tools, canonical.tools);
}

#[cfg(unix)]
#[tokio::test]
async fn missing_global_component_seeds_history_after_open() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = crate::UsageStore::open(temp.path()).await.expect("store");
    store.report_refresh.cancel();
    let process_id = crate::ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 47, /*started_at_ms*/ 0)
        .await
        .expect("process");
    sqlx::query(
        "INSERT INTO threads(id,parent_thread_id,source_kind,created_at_ms) VALUES ('seed-thread',NULL,'cli',0)",
    )
    .execute(&store.pool)
    .await
    .expect("thread");
    let mut tx = store.pool.begin().await.expect("seed transaction");
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i+1 FROM n WHERE i<9999) INSERT INTO operations(id,process_id,thread_id,operation_kind,started_at_ms,taxonomy_version,phase,activity,activity_state,attribution_provenance) SELECT printf('seed-op-%05d',i),?,'seed-thread','local_tool',i*1000,1,'implementation','coding','tool_active','agent_declared' FROM n",
    )
    .bind(process_id.as_string())
    .execute(&mut *tx)
    .await
    .expect("operations");
    sqlx::query(
        "INSERT INTO operations(id,process_id,thread_id,operation_kind,started_at_ms,taxonomy_version,phase,activity,activity_state,attribution_provenance) VALUES ('seed-model-op',?,'seed-thread','model_request',10000000,1,'implementation','coding','model_active','agent_declared')",
    )
    .bind(process_id.as_string())
    .execute(&mut *tx)
    .await
    .expect("model operation");
    sqlx::query(
        "INSERT INTO model_requests(id,operation_id,provider_kind,model,transport_kind,attempt_number,client_origin) VALUES ('seed-model-request','seed-model-op','openai','gpt-6-astra','sse',1,'seed')",
    )
    .execute(&mut *tx)
    .await
    .expect("model request");
    sqlx::query(
        "INSERT INTO operation_events(event_id,operation_id,event_kind,terminal,occurred_at_ms,duration_ns) VALUES ('seed-model-event','seed-model-op','completed',1,10001000,1000000000)",
    )
    .execute(&mut *tx)
    .await
    .expect("model terminal");
    sqlx::query(
        "INSERT INTO token_observations(id,model_request_id,source_event_id,category_path,token_count,unit,measurement_provenance,coverage_state,repository_bucket,observed_at_ms) VALUES ('seed-token','seed-model-request','00000000-0000-0000-0000-000000000001','input_tokens',9,'tokens','provider_reported','complete','unknown',10000500)",
    )
    .execute(&mut *tx)
    .await
    .expect("token observation");
    sqlx::query(
        "INSERT INTO coverage_events(event_id,operation_id,scope_kind,coverage_state,reason_code,occurred_at_ms) VALUES ('seed-partial-coverage','seed-model-op','model_attempt','partial',NULL,10001000), ('seed-global-gap',NULL,'global','partial',NULL,10001100)",
    )
    .execute(&mut *tx)
    .await
    .expect("coverage events");
    sqlx::query(
        "INSERT INTO activity_spans(id,operation_id,activity_state,started_at_ms) VALUES ('seed-span','seed-model-op','user_wait',10000200)",
    )
    .execute(&mut *tx)
    .await
    .expect("activity span");
    sqlx::query(
        "INSERT INTO activity_span_events(event_id,activity_span_id,event_kind,occurred_at_ms) VALUES ('seed-span-end','seed-span','ended',10000300)",
    )
    .execute(&mut *tx)
    .await
    .expect("activity span end");
    tx.commit().await.expect("seed commit");
    sqlx::raw_sql(super::schema::RESET_SQL)
        .execute(&store.pool)
        .await
        .expect("remove global component");
    let blocked_refresh = store
        .report_refresh
        .reader
        .acquire()
        .await
        .expect("hold refresh admission");
    store.close().await;

    let started = std::time::Instant::now();
    let reopened = crate::UsageStore::open(temp.path()).await.expect("reopen");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    reopened.report_refresh.cancel();
    drop(blocked_refresh);
    assert_eq!(
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT cursor,high_water FROM _usage_report_global_seed WHERE source='operations'",
        )
        .fetch_one(&reopened.pool)
        .await
        .expect("seed frontier"),
        (0, 10_001)
    );
    let seed_before_page: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT source,cursor,high_water FROM _usage_report_global_seed ORDER BY source",
    )
    .fetch_all(&reopened.pool)
    .await
    .expect("all seed frontiers");
    assert_eq!(seed_before_page.len(), 4);
    assert!(
        seed_before_page
            .iter()
            .all(|(_, cursor, high_water)| *cursor == 0 && *high_water > 0)
    );
    assert!(
        !crate::report_cache::is_global_ready(&reopened.pool)
            .await
            .expect("warming global cache")
    );
    loop {
        match crate::report_cache::backfill::step(&reopened.pool)
            .await
            .expect("prepare core page")
        {
            crate::report_cache::backfill::Progress::Ready => break,
            crate::report_cache::backfill::Progress::Advanced => {}
            crate::report_cache::backfill::Progress::ReaderBusy => {
                tokio::task::yield_now().await;
            }
            crate::report_cache::backfill::Progress::Superseded => {
                panic!("unexpected newer core cache")
            }
        }
    }
    assert!(
        !crate::report_global::step(&reopened.pool)
            .await
            .expect("single bounded seed page")
    );
    let seed_after_page: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT source,cursor,high_water FROM _usage_report_global_seed ORDER BY source",
    )
    .fetch_all(&reopened.pool)
    .await
    .expect("seed frontier after one page");
    let progressed: Vec<_> = seed_before_page
        .iter()
        .zip(&seed_after_page)
        .filter(|((_, before, _), (_, after, _))| after > before)
        .collect();
    assert_eq!(progressed.len(), 1);
    assert!(progressed[0].1.1 - progressed[0].0.1 <= 1024);
    crate::report_cache::ensure(&reopened.pool, &reopened.report_refresh.reader)
        .await
        .expect("finish seeded history");
    assert!(
        crate::report_cache::is_global_ready(&reopened.pool)
            .await
            .expect("seeded global cache")
    );
}
