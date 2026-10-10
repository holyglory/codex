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
                // Retry this exact event on a storage outage. Network work remains
                // independent; the bounded queue reports any evidence it cannot keep.
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
                    let mut details = event.details.clone();
                    if missing > 0 {
                        details.insert(
                            "dropped_records_before_this_event".to_string(),
                            missing.into(),
                        );
                    }
                    let result = sqlx::query("INSERT INTO network_events (timestamp_ms, thread_id, turn_id, event, details) VALUES (?, ?, ?, ?, ?)")
                        .bind(event.timestamp_ms).bind(&event.thread_id).bind(&event.turn_id)
                        .bind(&event.event).bind(serde_json::Value::Object(details.into_iter().collect()).to_string())
                        .execute(db).await;
                    if result.is_ok() {
                        gaps.fetch_sub(missing, Ordering::Relaxed);
                        break;
                    }
                    eprintln!("network diagnostics write failed; retaining pending evidence");
                    tokio::time::sleep(Duration::from_secs(/*secs*/ 5)).await;
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
