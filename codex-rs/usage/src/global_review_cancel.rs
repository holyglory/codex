//! Runtime-owned cancellation for one background analytics publisher connection.
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use duckdb::Connection;
use duckdb::InterruptHandle;

use crate::Result;
use crate::StorageError;

#[derive(Default)]
pub(crate) struct GlobalReviewCancellation {
    cancelled: AtomicBool,
    commit_gate: Mutex<()>,
    active: Mutex<Option<Arc<InterruptHandle>>>,
}

impl std::fmt::Debug for GlobalReviewCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GlobalReviewCancellation")
            .field("cancelled", &self.cancelled.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl GlobalReviewCancellation {
    pub(crate) fn cancel(&self) {
        {
            let _gate = self
                .commit_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.cancelled.store(true, Ordering::Release);
        }
        if let Ok(active) = self.active.lock()
            && let Some(handle) = active.as_ref()
        {
            handle.interrupt();
        }
    }

    /// Linearize the final rename against cancellation. Serialization, SQL and
    /// durability barriers belong outside this short publication gate.
    pub(crate) fn commit(&self, publish: impl FnOnce() -> Result<()>) -> Result<()> {
        let _gate = self.commit_gate.lock().map_err(|_| {
            StorageError::InvalidData("global analytics commit lock poisoned".into())
        })?;
        self.check()?;
        publish()
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(StorageError::InvalidData(
                "global analytics publication cancelled".into(),
            ));
        }
        Ok(())
    }

    /// An idle interrupt does not survive DuckDB's next query startup. Check
    /// the durable flag again before admitting each background query stage.
    pub(crate) fn run_stage<T>(&self, stage: impl FnOnce() -> Result<T>) -> Result<T> {
        self.check()?;
        stage()
    }

    pub(crate) fn register(self: &Arc<Self>, connection: &Connection) -> Result<Registration> {
        let mut active = self.active.lock().map_err(|_| {
            StorageError::InvalidData("global analytics cancellation lock poisoned".into())
        })?;
        self.check()?;
        if active.is_some() {
            return Err(StorageError::InvalidData(
                "global analytics connection already registered".into(),
            ));
        }
        *active = Some(connection.interrupt_handle());
        Ok(Registration {
            cancellation: Arc::clone(self),
        })
    }
}

pub(crate) struct Registration {
    cancellation: Arc<GlobalReviewCancellation>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut active) = self.cancellation.active.lock() {
            *active = None;
        }
    }
}
