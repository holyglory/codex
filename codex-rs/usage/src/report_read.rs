//! Report admission and SQLite work share a deadline and cancellation lifetime.
use crate::UsageStoreError;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::pool::PoolConnection;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio::time::timeout_at;

pub(crate) const REPORT_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);

pub(crate) struct ReportRead {
    pub(crate) connection: PoolConnection<Sqlite>,
    cancellation: SqliteDeadline,
    _permit: OwnedSemaphorePermit,
}

impl ReportRead {
    pub(crate) async fn acquire(
        pool: &SqlitePool,
        admission: &Arc<Semaphore>,
        deadline: Instant,
    ) -> Result<Self, UsageStoreError> {
        // Admission happens before acquiring a connection. Queued reports cannot
        // consume the connections that persist new usage records.
        let permit = timeout_at(deadline, Arc::clone(admission).acquire_owned())
            .await
            .map_err(|_| UsageStoreError::ReportBusy)?
            .map_err(|_| UsageStoreError::ReportBusy)?;
        let mut connection = timeout_at(deadline, pool.acquire())
            .await
            .map_err(|_| UsageStoreError::ReportBusy)?
            .map_err(UsageStoreError::Database)?;
        // Never return a cancelled connection, temporary tables, or a progress
        // callback to another borrower. SQLite closes after its worker drains.
        connection.close_on_drop();
        let mut read = Self {
            connection,
            cancellation: SqliteDeadline::default(),
            _permit: permit,
        };
        let cancelled = Arc::clone(&read.cancellation.cancelled);
        timeout_at(deadline, read.connection.lock_handle())
            .await
            .map_err(|_| UsageStoreError::ReportTimedOut)?
            .map_err(UsageStoreError::Database)?
            .set_progress_handler(/*num_ops*/ 1_000, move || {
                !cancelled.load(Ordering::Relaxed) && Instant::now() < deadline
            });
        timeout_at(deadline, sqlx::raw_sql(
            "PRAGMA busy_timeout = 100; PRAGMA temp_store = FILE; PRAGMA cache_size = -2048; PRAGMA temp.cache_size = -512;",
        ).execute(&mut *read.connection))
        .await
        .map_err(|_| UsageStoreError::ReportTimedOut)?
        .map_err(UsageStoreError::Database)?;
        Ok(read)
    }

    pub(crate) async fn finish<T>(
        &mut self,
        result: Result<T, UsageStoreError>,
        deadline: Instant,
    ) -> Result<T, UsageStoreError> {
        self.cancellation.cancelled.store(true, Ordering::Relaxed);
        // lock_handle waits for the interrupted SQL to leave SQLite. The
        // transaction's queued rollback completes before we release admission.
        if let Ok(mut handle) = self.connection.lock_handle().await {
            handle.remove_progress_handler();
        }
        if Instant::now() >= deadline {
            return Err(UsageStoreError::ReportTimedOut);
        }
        result.map_err(|error| match error {
            UsageStoreError::Database(sqlx::Error::Database(ref database))
                if database.code().is_some_and(|code| {
                    code.parse::<u32>()
                        .is_ok_and(|code| matches!(code & 255, 5 | 6))
                }) =>
            {
                UsageStoreError::ReportBusy
            }
            error => error,
        })
    }
}

#[derive(Default)]
pub(crate) struct SqliteDeadline {
    cancelled: Arc<AtomicBool>,
}

impl SqliteDeadline {
    pub(crate) async fn install(
        &self,
        connection: &mut sqlx::SqliteConnection,
        deadline: Instant,
    ) -> Result<(), sqlx::Error> {
        let cancelled = Arc::clone(&self.cancelled);
        connection
            .lock_handle()
            .await?
            .set_progress_handler(/*num_ops*/ 1_000, move || {
                !cancelled.load(Ordering::Relaxed) && Instant::now() < deadline
            });
        Ok(())
    }
}

impl Drop for SqliteDeadline {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}
