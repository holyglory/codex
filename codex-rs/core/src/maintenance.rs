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
        if session.flush_rollout().await.is_err() {
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
    ) -> codex_protocol::error::Result<crate::TurnInputSubmission> {
        self.session
            .services
            .agent_control
            .ensure_execution_capacity_for_turn_start(self)
            .await?;
        self.io
            .submit_recover_turn(Default::default(), options, None, turn_id)
            .await
    }
}

/// Pending agent mail belongs to its conversation, never the usage/event log.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MaintenanceMail {
    pub communication: codex_protocol::protocol::InterAgentCommunication,
    pub options: crate::TurnStartOptions,
}

#[derive(serde::Deserialize)]
#[serde(tag = "type", content = "payload")]
enum HistoryIdentity {
    #[serde(rename = "response_item")]
    ResponseItem(HistoryPayloadIdentity),
    #[serde(rename = "inter_agent_communication")]
    AgentMail(HistoryPayloadIdentity),
    #[serde(other)]
    Other,
}

#[derive(serde::Deserialize)]
struct HistoryPayloadIdentity {
    #[serde(default)]
    id: Option<codex_protocol::ResponseItemId>,
}

impl crate::CodexThread {
    pub async fn maintenance_mailbox(&self) -> Vec<MaintenanceMail> {
        self.session.input_queue.maintenance_mailbox().await
    }

    /// Restore only mail not already recorded before a prior recovery attempt.
    /// The streaming decoder ignores payload content, including compacted history.
    pub async fn restore_maintenance_mailbox(
        &self,
        mail: Vec<MaintenanceMail>,
    ) -> std::io::Result<()> {
        if mail.is_empty() {
            return Ok(());
        }
        let path = self
            .rollout_path()
            .ok_or_else(|| std::io::Error::other("missing mailbox history"))?;
        let path = path.clone();
        let wanted: std::collections::BTreeSet<_> =
            mail.iter()
                .map(|entry| {
                    entry.communication.id.clone().ok_or_else(|| {
                        std::io::Error::other("mailbox entry lacks its stable identity")
                    })
                })
                .collect::<std::io::Result<_>>()?;
        let mut seen = tokio::task::spawn_blocking(
            move || -> std::io::Result<std::collections::BTreeSet<_>> {
                let input = std::io::BufReader::new(std::fs::File::open(path)?);
                let mut seen = std::collections::BTreeSet::new();
                for row in
                    serde_json::Deserializer::from_reader(input).into_iter::<HistoryIdentity>()
                {
                    let row = row.map_err(std::io::Error::other)?;
                    let payload = match row {
                        HistoryIdentity::ResponseItem(payload)
                        | HistoryIdentity::AgentMail(payload) => Some(payload),
                        HistoryIdentity::Other => None,
                    };
                    if let Some(id) = payload.and_then(|payload| payload.id)
                        && wanted.contains(&id)
                    {
                        seen.insert(id);
                    }
                    if seen.len() == wanted.len() {
                        break;
                    }
                }
                Ok(seen)
            },
        )
        .await
        .map_err(std::io::Error::other)??;
        seen.extend(
            self.maintenance_mailbox()
                .await
                .into_iter()
                .filter_map(|entry| entry.communication.id),
        );
        for entry in mail {
            if entry
                .communication
                .id
                .as_ref()
                .is_some_and(|id| seen.contains(id))
            {
                continue;
            }
            if let Some(id) = entry.communication.id.clone() {
                seen.insert(id);
            }
            self.session
                .input_queue
                .enqueue_mailbox_communication(entry.communication, entry.options)
                .await;
        }
        Ok(())
    }
}
