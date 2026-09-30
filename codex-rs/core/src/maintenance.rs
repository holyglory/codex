//! Cooperative, reversible pause at a persisted model/tool boundary.
//!
//! A pause is owned by the caller's handle. Dropping the handle releases the
//! same in-memory turn; it never interrupts a tool or marks user work stopped.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenancePauseStatus {
    Requested,
    Paused,
    BackgroundWork,
    PersistenceFailed,
    Released,
}

#[derive(Debug, Default)]
pub(crate) struct MaintenanceGate {
    request: Mutex<Weak<PauseRequest>>,
}

#[derive(Debug)]
pub(crate) struct PauseRequest {
    status: watch::Sender<MaintenancePauseStatus>,
    released: CancellationToken,
}

/// Owns a single pause request. Release or drop always allows execution to continue.
/// A Paused receipt is valid only while this handle remains alive and unreleased.
pub struct MaintenancePause {
    request: Arc<PauseRequest>,
    status: watch::Receiver<MaintenancePauseStatus>,
    resume_on_drop: bool,
}

impl MaintenanceGate {
    pub(crate) fn request(&self) -> Option<MaintenancePause> {
        let mut current = self
            .request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current
            .upgrade()
            .is_some_and(|request| !request.released.is_cancelled())
        {
            return None;
        }
        let (status, receiver) = watch::channel(MaintenancePauseStatus::Requested);
        let request = Arc::new(PauseRequest {
            status,
            released: CancellationToken::new(),
        });
        *current = Arc::downgrade(&request);
        Some(MaintenancePause {
            request,
            status: receiver,
            resume_on_drop: true,
        })
    }

    pub(crate) fn pending(&self) -> Option<Arc<PauseRequest>> {
        self.request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upgrade()
            .filter(|request| !request.released.is_cancelled())
    }
}

impl MaintenancePause {
    pub fn status(&self) -> MaintenancePauseStatus {
        *self.status.borrow()
    }

    pub async fn changed(&mut self) -> MaintenancePauseStatus {
        let _ = self.status.changed().await;
        self.status()
    }

    /// Irreversibly keeps this runtime parked while its owning process exits.
    /// Call only after its complete recovery state has been durably committed.
    pub fn commit(mut self) {
        self.resume_on_drop = false;
    }

    pub fn release(&self) {
        self.request.released.cancel();
        self.request
            .status
            .send_replace(MaintenancePauseStatus::Released);
    }
}

impl Drop for MaintenancePause {
    fn drop(&mut self) {
        if self.resume_on_drop { self.release(); }
    }
}

impl PauseRequest {
    pub(crate) async fn checkpoint(
        &self,
        session: &crate::session::session::Session,
        cancellation: &CancellationToken,
    ) {
        if self.released.is_cancelled() || cancellation.is_cancelled() {
            return;
        }
        if session.services.code_mode_service.has_active_cells()
            || !session.services.unified_exec_manager.list_processes().await.is_empty()
        {
            self.status.send_replace(MaintenancePauseStatus::BackgroundWork);
            return;
        }
        // Async hooks must publish their outputs before the checkpoint is sealed.
        let hooks = session.services.hooks.load_full();
        tokio::select! {
            _ = hooks.wait_for_async_hooks() => {}
            _ = self.released.cancelled() => return,
            _ = cancellation.cancelled() => return,
        }
        // Unconsumed hook output is handled by the next normal step, not discarded.
        if !session.async_hook_results.is_empty() {
            self.status.send_replace(MaintenancePauseStatus::BackgroundWork);
            return;
        }
        // This call is made only between complete sampling/tool steps. A failed
        // flush is reported to the maintenance owner, never made a fatal turn error.
        if session.flush_rollout().await.is_err() {
            self.status
                .send_replace(MaintenancePauseStatus::PersistenceFailed);
            return;
        }
        if self.released.is_cancelled() || cancellation.is_cancelled() {
            return;
        }
        self.status.send_replace(MaintenancePauseStatus::Paused);
        tokio::select! {
            _ = self.released.cancelled() => {}
            _ = cancellation.cancelled() => {}
        }
        self.status.send_replace(MaintenancePauseStatus::Released);
    }
}
