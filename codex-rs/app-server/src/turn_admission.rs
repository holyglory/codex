//! Tracks client runtime changes and turn starts admitted before a shutdown drain.

use codex_app_server_protocol::JSONRPCErrorError;
use codex_extension_api::TurnStartAdmission;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::watch;

use crate::error_code::server_draining_error;

#[derive(Debug, Default)]
struct AdmissionState {
    closed: bool,
    maintenance: bool,
    sealed: bool,
    restoring: bool,
    restoration_failed: bool,
    active: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct TurnAdmission {
    pub(crate) resumptions: crate::maintenance_resumption::MaintenanceResumption,
    state: Arc<Mutex<AdmissionState>>,
    active_tx: watch::Sender<usize>,
    maintenance_tx: watch::Sender<bool>,
}

impl Default for TurnAdmission {
    fn default() -> Self {
        Self {
            resumptions: Default::default(),
            state: Arc::new(Mutex::new(AdmissionState::default())),
            active_tx: watch::channel(0).0,
            maintenance_tx: watch::channel(false).0,
        }
    }
}

pub(crate) struct TurnPermit(TurnAdmission);

/// Reopening is automatic on failed preparation or owner disconnect.
pub(crate) struct MaintenanceAdmission(TurnAdmission);

impl Drop for MaintenanceAdmission {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.maintenance = false;
        state.sealed = false;
        self.0.maintenance_tx.send_replace(false);
    }
}

impl TurnAdmission {
    pub(crate) fn restoration_failed(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.restoring = false;
        state.restoration_failed = true;
    }

    pub(crate) fn restoration_complete(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.restoring && !state.restoration_failed
    }

    pub(crate) fn restoration_started(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .restoring = true;
    }

    pub(crate) fn restoration_finished(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .restoring = false;
    }

    pub(crate) fn begin_maintenance(&self) -> Option<MaintenanceAdmission> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || state.maintenance || state.restoring || state.restoration_failed {
            return None;
        }
        state.maintenance = true;
        self.maintenance_tx.send_replace(true);
        Some(MaintenanceAdmission(self.clone()))
    }

    pub(crate) fn seal_maintenance(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sealed = true;
    }

    pub(crate) fn accepting(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.closed && !state.sealed
    }

    pub(crate) fn maintenance_requested(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .maintenance
    }

    pub(crate) fn begin_drain(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
    }

    pub(crate) fn subscribe_active(&self) -> watch::Receiver<usize> {
        self.active_tx.subscribe()
    }

    // Admit and close take the same short lock. The permit keeps shutdown from
    // finishing while an earlier request is still preparing or submitting work.
    pub(crate) fn admit_interrupt(&self) -> Result<TurnPermit, JSONRPCErrorError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.restoring {
            state.active += 1;
            self.active_tx.send_replace(state.active);
            return Ok(TurnPermit(self.clone()));
        }
        drop(state);
        self.admit()
    }

    pub(crate) fn admit(&self) -> Result<TurnPermit, JSONRPCErrorError> {
        let restoring = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .restoring;
        // Loading seals admission. Once the complete graph is present, fresh
        // sessions remain usable while recovered turns wait for normal capacity.
        let permit = self.try_admit();
        permit.ok_or_else(|| {
            if restoring || self.maintenance_requested() {
                codex_app_server_protocol::JSONRPCErrorError {
                    code: -32600,
                    message: "Server is preparing an upgrade; retry after reconnecting".to_string(),
                    data: Some(
                        serde_json::json!({"reason": "serverSwitching", "retryAfterMs": 250}),
                    ),
                }
            } else {
                server_draining_error()
            }
        })
    }

    fn try_admit(&self) -> Option<TurnPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || state.sealed {
            return None;
        }
        state.active += 1;
        self.active_tx.send_replace(state.active);
        Some(TurnPermit(self.clone()))
    }
}

impl TurnStartAdmission for TurnAdmission {
    fn maintenance_requested(&self) -> bool {
        self.maintenance_requested()
    }

    fn maintenance_released(&self) -> codex_extension_api::ExtensionFuture<'_, ()> {
        Box::pin(async move {
            let mut state = self.maintenance_tx.subscribe();
            while *state.borrow_and_update() {
                if state.changed().await.is_err() {
                    break;
                }
            }
        })
    }

    fn admit_turn_start(&self) -> Option<Box<dyn Send>> {
        self.try_admit()
            .map(|permit| Box::new(permit) as Box<dyn Send>)
    }
}

impl Drop for TurnPermit {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active -= 1;
        self.0.active_tx.send_replace(state.active);
    }
}

#[cfg(test)]
#[path = "turn_admission_tests.rs"]
mod tests;
