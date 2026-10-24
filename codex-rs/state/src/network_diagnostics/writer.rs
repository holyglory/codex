use super::NetworkDiagnostic;
use super::open;
use crate::SqliteConfig;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

#[derive(Clone)]
pub(crate) struct NetworkSink {
    sender: mpsc::Sender<Command>,
    dropped: Arc<AtomicU64>,
}

enum Command {
    Event(NetworkDiagnostic),
    Flush(oneshot::Sender<()>),
}

const MAX_BATCH_EVENTS: usize = 128;

async fn insert_batch(
    db: &sqlx::SqlitePool,
    events: &[NetworkDiagnostic],
    missing: u64,
) -> Result<(), sqlx::Error> {
    let mut transaction = db.begin().await?;
    for (index, event) in events.iter().enumerate() {
        let mut details = event.details.clone();
        if index == 0 && missing > 0 {
            details.insert(
                "dropped_records_before_this_event".to_string(),
                missing.into(),
            );
        }
        sqlx::query("INSERT INTO network_events (timestamp_ms, thread_id, turn_id, event, details) VALUES (?, ?, ?, ?, ?)")
            .bind(event.timestamp_ms).bind(&event.thread_id).bind(&event.turn_id)
            .bind(&event.event).bind(serde_json::Value::Object(details.into_iter().collect()).to_string())
            .execute(&mut *transaction).await?;
    }
    transaction.commit().await
}

impl NetworkSink {
    pub(crate) fn start(sqlite: SqliteConfig) -> Self {
        let (sender, mut receiver) = mpsc::channel(/*buffer*/ 4096);
        let dropped = Arc::new(AtomicU64::new(0));
        let gaps = Arc::clone(&dropped);
        tokio::spawn(async move {
            let mut pool: Option<sqlx::SqlitePool> = None;
            while let Some(command) = receiver.recv().await {
                let Command::Event(event) = command else {
                    if let Command::Flush(reply) = command {
                        if let Some(db) = pool.take() {
                            db.close().await;
                        }
                        let _ = reply.send(());
                    }
                    continue;
                };
                // Coalesce only records already queued, with no batching delay.
                // A flush is a barrier: do not include records that follow it.
                let mut events = vec![event];
                let mut flush_reply = None;
                while events.len() < MAX_BATCH_EVENTS {
                    match receiver.try_recv() {
                        Ok(Command::Event(event)) => events.push(event),
                        Ok(Command::Flush(reply)) => {
                            flush_reply = Some(reply);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                // Retry this exact batch on a storage outage. One durable
                // transaction avoids a separate disk flush for every queued
                // event while preventing a partially written batch on retry.
                loop {
                    if pool.is_none() {
                        match open(sqlite.home()).await {
                            Ok(db) => pool = Some(db),
                            Err(_) => {
                                eprintln!(
                                    "network diagnostics database unavailable; retaining pending evidence"
                                );
                                tokio::time::sleep(Duration::from_secs(/*secs*/ 5)).await;
                                continue;
                            }
                        }
                    }
                    let Some(db) = pool.as_ref() else {
                        continue;
                    };
                    let missing = gaps.load(Ordering::Relaxed);
                    let result = insert_batch(db, &events, missing).await;
                    if result.is_ok() {
                        gaps.fetch_sub(missing, Ordering::Relaxed);
                        break;
                    }
                    eprintln!("network diagnostics write failed; retaining pending evidence");
                    tokio::time::sleep(Duration::from_secs(/*secs*/ 5)).await;
                }
                if let Some(reply) = flush_reply {
                    if let Some(db) = pool.take() {
                        db.close().await;
                    }
                    let _ = reply.send(());
                }
            }
            if let Some(pool) = pool {
                pool.close().await;
            }
        });
        Self { sender, dropped }
    }

    pub(crate) fn record(&self, event: NetworkDiagnostic) {
        if self.sender.try_send(Command::Event(event)).is_err() {
            self.dropped.fetch_add(/*val*/ 1, Ordering::Relaxed);
            eprintln!("network diagnostics queue unavailable; incident record lost");
        }
    }

    pub(crate) async fn flush(&self) {
        let (reply, done) = oneshot::channel();
        let flush = async {
            self.sender
                .send(Command::Flush(reply))
                .await
                .map_err(|_| ())?;
            done.await.map_err(|_| ())
        };
        if !matches!(
            tokio::time::timeout(Duration::from_secs(/*secs*/ 10), flush).await,
            Ok(Ok(()))
        ) {
            eprintln!(
                "network diagnostics flush incomplete; pending evidence has not been confirmed durable"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;
    use std::collections::BTreeMap;

    fn event(name: &str) -> NetworkDiagnostic {
        NetworkDiagnostic {
            id: 0,
            timestamp_ms: 1,
            thread_id: None,
            turn_id: None,
            event: name.to_string(),
            details: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn batch_rollback_preserves_exact_records_and_loss_marker_on_retry() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let pool = open(directory.path()).await?;
        sqlx::raw_sql("CREATE TRIGGER reject_second BEFORE INSERT ON network_events WHEN NEW.event = 'second' BEGIN SELECT RAISE(ABORT, 'fixture storage failure'); END;")
            .execute(&pool).await?;
        let events = [event("first"), event("second")];
        assert!(insert_batch(&pool, &events, /*missing*/ 2).await.is_err());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM network_events")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0);

        sqlx::query("DROP TRIGGER reject_second")
            .execute(&pool)
            .await?;
        insert_batch(&pool, &events, /*missing*/ 2).await?;
        let rows = sqlx::query("SELECT id, event, details FROM network_events ORDER BY id")
            .fetch_all(&pool)
            .await?;
        assert_eq!(rows.len(), 2);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.get::<i64, _>("id"), index as i64 + 1);
            assert_eq!(row.get::<String, _>("event"), events[index].event);
            let details: serde_json::Value = serde_json::from_str(row.get("details"))?;
            assert_eq!(
                details,
                if index == 0 {
                    serde_json::json!({"dropped_records_before_this_event": 2})
                } else {
                    serde_json::json!({})
                }
            );
        }
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn flush_barrier_does_not_wait_for_later_failing_records() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = open(directory.path()).await?;
        sqlx::raw_sql("CREATE TRIGGER reject_later BEFORE INSERT ON network_events WHEN NEW.event = 'later' BEGIN SELECT RAISE(ABORT, 'fixture storage failure'); END;")
            .execute(&pool).await?;
        let sqlite = SqliteConfig::new_for_testing(
            codex_utils_absolute_path::AbsolutePathBuf::try_from(directory.path())?,
        );
        let sink = NetworkSink::start(sqlite);
        let (reply, done) = oneshot::channel();
        // Queue the barrier between the records before the current-thread
        // runtime can start the writer. The later record cannot join its batch.
        sink.record(event("first"));
        sink.sender
            .try_send(Command::Flush(reply))
            .map_err(|_| anyhow::anyhow!("fixture flush queue unexpectedly unavailable"))?;
        sink.record(event("later"));
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), done).await??;
        let names: Vec<String> = sqlx::query_scalar("SELECT event FROM network_events ORDER BY id")
            .fetch_all(&pool)
            .await?;
        assert_eq!(names, ["first"]);

        sqlx::query("DROP TRIGGER reject_later")
            .execute(&pool)
            .await?;
        sink.flush().await;
        let names: Vec<String> = sqlx::query_scalar("SELECT event FROM network_events ORDER BY id")
            .fetch_all(&pool)
            .await?;
        assert_eq!(names, ["first", "later"]);
        drop(sink);
        pool.close().await;
        Ok(())
    }
}
