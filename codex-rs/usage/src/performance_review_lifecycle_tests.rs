//! SQLite worker lifetime is isolated here because small public review fixtures
//! cannot deterministically keep a statement running until cancellation.
use super::*;
use crate::report_read::ReportRead;
use sqlx::Connection;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

const SLOW_SQL: &str = "WITH RECURSIVE counter(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM counter WHERE n<1000000000) SELECT SUM(n) FROM counter";

#[tokio::test]
async fn report_deadline_interrupts_sql_and_releases_the_snapshot() {
    let fixture = Fixture::new().await;
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut read = ReportRead::acquire(
        &fixture.store.pool,
        &fixture.store.report_refresh.reader,
        deadline,
    )
    .await
    .expect("reader");
    let mut tx = read.connection.begin().await.expect("snapshot");
    sqlx::query("SELECT * FROM operations")
        .fetch_all(&mut *tx)
        .await
        .expect("pin snapshot");
    fixture
        .store
        .begin_operation(&fixture.operation(1_000_000))
        .await
        .expect("capture alongside report");
    let result = sqlx::query_scalar::<_, i64>(SLOW_SQL)
        .fetch_one(&mut *tx)
        .await
        .map_err(UsageStoreError::Database);
    drop(tx);
    assert!(matches!(
        read.finish(result, deadline).await,
        Err(UsageStoreError::ReportTimedOut)
    ));
    drop(read);
    let (busy, _, _): (i64, i64, i64) = tokio::time::timeout(
        Duration::from_secs(1),
        sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)").fetch_one(&fixture.store.pool),
    )
    .await
    .expect("snapshot released promptly")
    .expect("checkpoint");
    std::assert_eq!(busy, 0);
    std::assert_eq!(fixture.packet().await.coverage.raw_operations, 1);
}

#[tokio::test]
async fn dropped_report_stops_sql_without_waiting_for_the_deadline() {
    let fixture = Fixture::new().await;
    let pool = fixture.store.pool.clone();
    let admission = Arc::clone(&fixture.store.report_refresh.reader);
    let (started, running) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut read =
            ReportRead::acquire(&pool, &admission, Instant::now() + Duration::from_secs(15))
                .await
                .expect("reader");
        let mut tx = read.connection.begin().await.expect("snapshot");
        sqlx::query("SELECT * FROM operations")
            .fetch_all(&mut *tx)
            .await
            .expect("pin snapshot");
        started.send(()).expect("signal snapshot");
        sqlx::query_scalar::<_, i64>(SLOW_SQL)
            .fetch_one(&mut *tx)
            .await
    });
    running.await.expect("report started");
    fixture
        .store
        .begin_operation(&fixture.operation(1_000_000))
        .await
        .expect("capture alongside report");
    task.abort();
    assert!(task.await.expect_err("cancelled report").is_cancelled());
    let (busy, _, _): (i64, i64, i64) = tokio::time::timeout(
        Duration::from_secs(1),
        sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)").fetch_one(&fixture.store.pool),
    )
    .await
    .expect("snapshot released promptly")
    .expect("checkpoint");
    std::assert_eq!(busy, 0);
    std::assert_eq!(fixture.packet().await.coverage.raw_operations, 1);
}

#[tokio::test]
async fn overlapping_report_admission_leaves_connections_for_capture() {
    let fixture = Fixture::new().await;
    let permit = fixture
        .store
        .report_refresh
        .reader
        .acquire()
        .await
        .expect("occupied reporting slot");
    let mut reports = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let pool = fixture.store.pool.clone();
        let admission = Arc::clone(&fixture.store.report_refresh.reader);
        reports.spawn(async move {
            matches!(
                ReportRead::acquire(
                    &pool,
                    &admission,
                    Instant::now() + Duration::from_millis(100)
                )
                .await,
                Err(UsageStoreError::ReportBusy)
            )
        });
    }
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.store.begin_operation(&fixture.operation(1_000_000)),
    )
    .await
    .expect("capture not queued behind reports")
    .expect("capture");
    while let Some(result) = reports.join_next().await {
        assert!(result.expect("report admission"));
    }
    drop(permit);
    std::assert_eq!(fixture.packet().await.coverage.raw_operations, 1);
}
