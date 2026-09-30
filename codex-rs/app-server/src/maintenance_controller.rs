//! Owns a reversible maintenance lease until checkpoint commit.
use super::MessageProcessor;
use codex_app_server_transport::maintenance::MaintenanceCommand;
use codex_app_server_transport::maintenance::MaintenanceConnection;
use codex_app_server_transport::maintenance::MaintenanceResponse;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

impl MessageProcessor {
    pub(crate) async fn restore_maintenance(
        &self,
        snapshot: codex_app_server_transport::daemon_recovery::RecoverySnapshot,
        admission: crate::turn_admission::MaintenanceAdmission,
    ) -> Result<(), &'static str> {
        let continuations = self
            .thread_processor
            .restore_maintenance_threads(snapshot)
            .await?;
        drop(admission);
        self.thread_processor
            .resume_maintenance_turns(continuations)
            .await
    }

    pub(crate) async fn maintenance_connection(
        self: Arc<Self>,
        connection: MaintenanceConnection,
        recovery_path: PathBuf,
        committed: CancellationToken,
    ) {
        let mut owned = false;
        async {
        let MaintenanceConnection { command, reply, mut commit, cancelled } = connection;
        let pid = std::process::id();
        if matches!(command, MaintenanceCommand::Status) {
            let _ = reply.send(MaintenanceResponse::Status { pid, accepting: self.turn_admission.accepting(), restored: self.turn_admission.restoration_complete(), executable: std::env::current_exe().unwrap_or_default() });
            return;
        }
        let MaintenanceCommand::Prepare { operation_id, pid: expected_pid } = command else {
            let _ = reply.send(MaintenanceResponse::Failed { reason: "prepareRequired".into() });
            return;
        };
        if expected_pid != pid || operation_id.is_empty() || operation_id.len() > 64
            || !operation_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            let _ = reply.send(MaintenanceResponse::Failed { reason: "invalidIdentity".into() });
            return;
        }
        let Some(admission) = self.turn_admission.begin_maintenance() else {
            let _ = reply.send(MaintenanceResponse::Failed { reason: "busy".into() });
            return;
        };
        owned = true;
        let mut paused = crate::request_processors::PausedThreads::default();
        let preparation = async {
            let mut active = self.turn_admission.subscribe_active();
            while *active.borrow_and_update() != 0 {
                active.changed().await.map_err(|_| "admissionUnavailable")?;
            }
            self.thread_processor.pause_for_maintenance(&mut paused, &cancelled).await
        };
        let result = tokio::select! {
            _ = cancelled.cancelled() => Err("cancelled"),
            result = tokio::time::timeout(Duration::from_secs(60), preparation) => result.unwrap_or(Err("pauseDeadline")),
        };
        match result {
            Ok(()) => {},
            Err(reason) => {
                let _ = reply.send(MaintenanceResponse::Failed { reason: reason.into() });
                return;
            }
        };
        if reply.send(MaintenanceResponse::Ready { operation_id: operation_id.clone(), pid }).is_err() { return; }
        let decision = tokio::select! {
            _ = cancelled.cancelled() => None,
            decision = tokio::time::timeout(Duration::from_secs(30), commit.recv()) => decision.ok().flatten(),
        };
        if !matches!(decision, Some(MaintenanceCommand::Commit { operation_id: id, pid: process }) if id == operation_id && process == pid) { return; }
        self.turn_admission.seal_maintenance();
        let sealed = async {
            let mut active = self.turn_admission.subscribe_active();
            while *active.borrow_and_update() != 0 {
                active.changed().await.map_err(|_| "admissionUnavailable")?;
            }
            self.thread_processor.pause_for_maintenance(&mut paused, &cancelled).await
        };
        if !matches!(tokio::time::timeout(Duration::from_secs(10), sealed).await, Ok(Ok(()))) { return; }
        // Re-evaluate stopped turns after Ready: user cancellation must win over
        // the previously requested maintenance resume.
        let snapshot = match self.thread_processor.maintenance_snapshot(operation_id.clone(), &paused).await {
            Ok(snapshot) => snapshot,
            Err(reason) => { tracing::warn!(%reason, "maintenance checkpoint rejected"); return; }
        };
        // A late write after timeout remains an unselected generation, never the
        // recovery snapshot consumed by startup.
        let staged = recovery_path.with_extension(format!("maintenance-{operation_id}.json"));
        let saved = tokio::select! {
            _ = cancelled.cancelled() => false,
            result = tokio::time::timeout(Duration::from_secs(10), crate::daemon_thread_recovery::snapshot(staged.clone(), snapshot)) => matches!(result, Ok(Ok(()))),
        };
        if !saved || cancelled.is_cancelled() { return; }
        if std::fs::rename(&staged, &recovery_path).is_err() { return; }
        self.turn_admission.begin_drain();
        paused.commit();
        drop(admission);
        committed.cancel();
        }.await;
        if owned && !committed.is_cancelled() {
            self.thread_processor.resume_idle_maintenance_work().await;
        }
    }
}
