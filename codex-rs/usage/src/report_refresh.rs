//! One cooperative refresh and one summary reader per source in this process.
//! SQLite serializes page commits across processes; progress is durable and
//! every page can be resumed by a later opener after cancellation or failure.
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::task::JoinHandle;

static REFRESHES: OnceLock<Mutex<HashMap<PathBuf, Weak<ReportRefresh>>>> = OnceLock::new();

pub(crate) struct ReportRefresh {
    task: Mutex<Option<JoinHandle<()>>>,
    pub(crate) reader: Arc<tokio::sync::Mutex<()>>,
}

impl ReportRefresh {
    pub(crate) fn for_source(path: &Path) -> std::io::Result<Arc<Self>> {
        let path = path.canonicalize()?;
        let mut registry = REFRESHES.get_or_init(Mutex::default).lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retain(|_, refresh| refresh.strong_count() > 0);
        if let Some(existing) = registry.get(&path).and_then(Weak::upgrade) { return Ok(existing); }
        let refresh = Arc::new(Self {task:Mutex::new(None),reader:Arc::new(tokio::sync::Mutex::new(()))});
        registry.insert(path,Arc::downgrade(&refresh));
        Ok(refresh)
    }

    pub(crate) fn cancel(&self) {
        if let Some(task) = self.task.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() { task.abort(); }
    }

    pub(crate) fn kick(&self, pool: SqlitePool) {
        let mut task = self.task.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if task.as_ref().is_some_and(|task| !task.is_finished()) { return; }
        let reader = Arc::clone(&self.reader);
        *task = Some(tokio::spawn(async move {
            if crate::report_cache::ensure(&pool, &reader).await.is_err() && !pool.is_closed() {
                tracing::warn!("usage report refresh failed; canonical reporting remains available");
            }
        }));
    }
}

impl Drop for ReportRefresh {
    fn drop(&mut self) { self.cancel(); }
}
