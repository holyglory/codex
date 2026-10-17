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
        "INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) VALUES (0,0)",
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
    rebuild_hour(&mut tx, 0).await.expect("rebuild hour");
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
    assert!(
        !crate::report_global::step(&store.pool)
            .await
            .expect("resume dirty operation")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM _usage_report_global_dirty_operations WHERE operation_id='missing-operation'",
        )
        .fetch_one(&store.pool)
        .await
        .expect("dirty operation after resume"),
        0
    );
    assert!(
        crate::report_global::step(&store.pool)
            .await
            .expect("finish global cache")
    );
}
