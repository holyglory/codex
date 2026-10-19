//! One cooperative refresh and one summary reader per source in this process.
//! SQLite serializes page commits across processes; progress is durable and
//! every page can be resumed by a later opener after cancellation or failure.
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;
use tokio::task::JoinHandle;
use tokio::time::Duration;

static REFRESHES: OnceLock<Mutex<HashMap<PathBuf, Weak<ReportRefresh>>>> = OnceLock::new();

pub(crate) struct ReportRefresh {
    task: Mutex<Option<JoinHandle<()>>>,
    pub(crate) reader: Arc<tokio::sync::Semaphore>,
    ready: Arc<tokio::sync::Notify>,
}

impl ReportRefresh {
    pub(crate) fn for_source(path: &Path) -> std::io::Result<Arc<Self>> {
        let path = path.canonicalize()?;
        let mut registry = REFRESHES
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retain(|_, refresh| refresh.strong_count() > 0);
        if let Some(existing) = registry.get(&path).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        let refresh = Arc::new(Self {
            task: Mutex::new(None),
            reader: Arc::new(tokio::sync::Semaphore::new(/*permits*/ 1)),
            ready: Arc::new(tokio::sync::Notify::new()),
        });
        registry.insert(path, Arc::downgrade(&refresh));
        Ok(refresh)
    }

    pub(crate) fn cancel(&self) {
        if let Some(task) = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
        self.ready.notify_waiters();
    }

    pub(crate) fn kick(&self, pool: SqlitePool) {
        let mut task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let reader = Arc::clone(&self.reader);
        let ready = Arc::clone(&self.ready);
        *task = Some(tokio::spawn(async move {
            let mut delay = Duration::from_millis(250);
            loop {
                match crate::report_cache::ensure(&pool, &reader).await {
                    Ok(()) => {
                        ready.notify_waiters();
                        break;
                    }
                    Err(_error) if pool.is_closed() => {
                        ready.notify_waiters();
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(
                            sqlite_code = crate::report_status::sqlite_code(&error),
                            stage = "refresh",
                            retry_after_ms = delay.as_millis(),
                            "usage report refresh failed; canonical reporting remains available"
                        );
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(30));
                    }
                }
            }
        }));
    }

    pub(crate) async fn wait_for_ready(
        &self,
        pool: &SqlitePool,
        timeout: Duration,
    ) -> Result<bool, sqlx::Error> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.ready.notified();
            let ready =
                tokio::time::timeout_at(deadline, crate::report_cache::is_global_ready(pool)).await;
            match ready {
                Ok(Ok(true)) => return Ok(true),
                Ok(Err(error)) => return Err(error),
                Ok(Ok(false)) => {}
                Err(_) => return Ok(false),
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Ok(false);
            }
        }
    }
}

impl Drop for ReportRefresh {
    fn drop(&mut self) {
        self.cancel();
    }
}
