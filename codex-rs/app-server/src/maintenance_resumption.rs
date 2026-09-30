//! Stop remains actionable while a saved turn is waiting to resume.
use codex_app_server_transport::daemon_recovery::RecoverySnapshot;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ResumeState {
    Pending,
    Started,
    Cancelled,
    Finished,
}

#[derive(Debug)]
pub(crate) struct PendingResume {
    pub(crate) turn_id: String,
    pub(crate) cancelled: CancellationToken,
    /// Dispatch and cancellation serialize at the actual core submission boundary.
    pub(crate) pending: tokio::sync::Mutex<ResumeState>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MaintenanceResumption {
    entries: Arc<Mutex<BTreeMap<String, Arc<PendingResume>>>>,
    checkpoint: Arc<tokio::sync::Mutex<Option<(PathBuf, RecoverySnapshot)>>>,
}

impl MaintenanceResumption {
    pub(crate) async fn initialize(&self, path: PathBuf, snapshot: RecoverySnapshot) {
        let entries = snapshot
            .interrupted
            .iter()
            .map(|(id, turn)| {
                (
                    id.clone(),
                    Arc::new(PendingResume {
                        turn_id: turn.turn_id.clone(),
                        cancelled: CancellationToken::new(),
                        pending: tokio::sync::Mutex::new(ResumeState::Pending),
                    }),
                )
            })
            .collect();
        *self.checkpoint.lock().await = Some((path, snapshot));
        *self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = entries;
    }

    pub(crate) fn get(&self, thread_id: &str) -> Option<Arc<PendingResume>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(thread_id)
            .cloned()
    }

    pub(crate) async fn cancel(&self, thread_id: &str, turn_id: &str) -> std::io::Result<bool> {
        let Some(entry) = self.get(thread_id) else {
            return Ok(false);
        };
        if !turn_id.is_empty() && turn_id != entry.turn_id {
            return Ok(false);
        }
        // Wake a capacity wait before taking the dispatch lock. After dispatch,
        // the caller uses the ordinary active-turn interruption path instead.
        entry.cancelled.cancel();
        let pending = entry.pending.lock().await;
        match *pending {
            ResumeState::Started | ResumeState::Finished => return Ok(false),
            ResumeState::Cancelled => return Ok(true),
            ResumeState::Pending => {}
        }
        let mut checkpoint = self.checkpoint.lock().await;
        let (path, snapshot) = checkpoint
            .as_mut()
            .ok_or_else(|| std::io::Error::other("maintenance checkpoint unavailable"))?;
        snapshot.interrupted.remove(thread_id);
        if let Some(maintenance) = snapshot.maintenance.as_mut() {
            maintenance.turn_contexts.remove(thread_id);
            maintenance
                .stopped
                .insert(thread_id.to_string(), entry.turn_id.clone());
        }
        crate::daemon_thread_recovery::snapshot(path.clone(), snapshot.clone()).await?;
        // The restored runtime will emit its ordinary aborted-turn event. The
        // acknowledged Stop is already durable even if this process dies first.
        Ok(true)
    }

    pub(crate) fn finish(&self) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}
