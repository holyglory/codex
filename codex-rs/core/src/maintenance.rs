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
    pub(crate) requested: tokio::sync::Notify,
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
    lifecycle: watch::Receiver<codex_protocol::protocol::AgentStatus>,
    resume_on_drop: bool,
}

impl MaintenanceGate {
    pub(crate) fn request(
        &self,
        lifecycle: watch::Receiver<codex_protocol::protocol::AgentStatus>,
    ) -> Option<MaintenancePause> {
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
        self.requested.notify_waiters();
        Some(MaintenancePause {
            request,
            status: receiver,
            lifecycle,
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
        tokio::select! {
            _ = self.status.changed() => {}
            _ = self.lifecycle.changed() => {}
        }
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
        if self.resume_on_drop {
            self.release();
        }
    }
}

pub(crate) enum CheckpointWake {
    Released,
    RecordInput,
}

impl PauseRequest {
    pub(crate) async fn checkpoint(
        &self,
        session: &crate::session::session::Session,
        cancellation: &CancellationToken,
    ) -> CheckpointWake {
        if self.released.is_cancelled() || cancellation.is_cancelled() {
            return CheckpointWake::Released;
        }
        if session.services.code_mode_service.has_active_cells()
            || !session
                .services
                .unified_exec_manager
                .list_processes()
                .await
                .is_empty()
        {
            self.status
                .send_replace(MaintenancePauseStatus::BackgroundWork);
            return CheckpointWake::Released;
        }
        // Async hooks must publish their outputs before the checkpoint is sealed.
        let hooks = session.services.hooks.load_full();
        tokio::select! {
            _ = hooks.wait_for_async_hooks() => {}
            _ = self.released.cancelled() => return CheckpointWake::Released,
            _ = cancellation.cancelled() => return CheckpointWake::Released,
        }
        // Unconsumed hook output is handled by the next normal step, not discarded.
        if !session.async_hook_results.is_empty() {
            self.status
                .send_replace(MaintenancePauseStatus::BackgroundWork);
            return CheckpointWake::Released;
        }
        // This call is made only between complete sampling/tool steps. A failed
        // flush is reported to the maintenance owner, never made a fatal turn error.
        let flushed = tokio::select! {
            result = session.flush_rollout() => result,
            _ = self.released.cancelled() => return CheckpointWake::Released,
            _ = cancellation.cancelled() => return CheckpointWake::Released,
        };
        if flushed.is_err() {
            self.status
                .send_replace(MaintenancePauseStatus::PersistenceFailed);
            return CheckpointWake::Released;
        }
        if self.released.is_cancelled() || cancellation.is_cancelled() {
            return CheckpointWake::Released;
        }
        let turn_state = session
            .active_turn
            .lock()
            .await
            .as_ref()
            .map(|turn| turn.turn_state.clone());
        let (mut input, pending) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;
        if pending.is_some()
            && session
                .input_queue
                .has_pending_input(&session.active_turn)
                .await
        {
            self.status.send_replace(MaintenancePauseStatus::Requested);
            return CheckpointWake::RecordInput;
        }
        self.status.send_replace(MaintenancePauseStatus::Paused);
        loop {
            tokio::select! {
                _ = self.released.cancelled() => break,
                _ = cancellation.cancelled() => break,
                changed = input.changed() => {
                    if changed.is_err() { break; }
                    if session.input_queue.has_pending_input(&session.active_turn).await {
                        self.status.send_replace(MaintenancePauseStatus::Requested);
                        return CheckpointWake::RecordInput;
                    }
                }
            }
        }
        self.status.send_replace(MaintenancePauseStatus::Released);
        CheckpointWake::Released
    }
}

impl crate::CodexThread {
    /// Re-enters a sealed maintenance checkpoint under its original turn identity.
    /// The host must verify persisted terminal events and permissions first.
    pub async fn resume_maintenance_checkpoint(
        &self,
        turn_id: String,
        options: crate::TurnStartOptions,
        context: crate::MaintenanceTurnContext,
        cancelled: &CancellationToken,
    ) -> codex_protocol::error::Result<crate::TurnInputSubmission> {
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => return Ok(crate::TurnInputSubmission::NotSubmitted { reason: crate::NotSubmittedReason::Superseded }),
            result = self.session.services.agent_control.ensure_execution_capacity_for_turn_start(self) => result?,
        }
        if cancelled.is_cancelled() {
            return Ok(crate::TurnInputSubmission::NotSubmitted {
                reason: crate::NotSubmittedReason::Superseded,
            });
        }
        self.thread_extension_data().insert(
            crate::session::maintenance_recovery::PendingMaintenanceContext(std::sync::Mutex::new(
                Some((turn_id.clone(), context)),
            )),
        );
        self.io
            .submit_recover_turn(Default::default(), options, None, turn_id)
            .await
    }
}

impl crate::CodexThread {
    /// Requests a reversible maintenance pause after the current model/tool step.
    /// The caller must separately account for background processes and queued input.
    pub fn request_maintenance_pause(&self) -> Option<crate::MaintenancePause> {
        self.thread_extension_data()
            .get::<crate::maintenance::MaintenanceGate>()?
            .request(self.io.agent_status.clone())
    }

    /// Reports whether this runtime currently owns any executing turn task.
    pub async fn maintenance_is_idle(&self) -> bool {
        self.session.active_turn.lock().await.is_none()
    }

    /// Pending turn input must reach its persisted checkpoint before commit.
    pub async fn maintenance_has_pending_input(&self) -> bool {
        !self.maintenance_is_idle().await
            && self
                .session
                .input_queue
                .has_pending_input(&self.session.active_turn)
                .await
    }

    /// Checks process-local work that cannot be transferred in a maintenance checkpoint.
    pub async fn maintenance_has_background_work(&self) -> bool {
        self.session.services.code_mode_service.has_active_cells()
            || !self.list_background_terminals().await.is_empty()
            || futures::FutureExt::now_or_never(
                self.session
                    .services
                    .hooks
                    .load_full()
                    .wait_for_async_hooks(),
            )
            .is_none()
            || !self.session.async_hook_results.is_empty()
    }
}

impl crate::CodexThread {
    pub async fn record_maintenance_stop(&self, turn_id: String) -> std::io::Result<()> {
        self.session
            .send_event_raw(codex_protocol::protocol::Event {
                id: turn_id.clone(),
                msg: codex_protocol::protocol::EventMsg::TurnAborted(
                    codex_protocol::protocol::TurnAbortedEvent {
                        turn_id: Some(turn_id),
                        reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                        started_at: None,
                        completed_at: None,
                        duration_ms: None,
                    },
                ),
            })
            .await;
        self.flush_rollout().await
    }
}
