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
        *task = Some(tokio::spawn(async move {
            let mut delay = Duration::from_millis(250);
            loop {
                match crate::report_cache::ensure(&pool, &reader).await {
                    Ok(()) => break,
                    Err(_error) if pool.is_closed() => break,
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
}

impl Drop for ReportRefresh {
    fn drop(&mut self) {
        self.cancel();
    }
}
