//! Saves persistent root thread IDs and restores them through the shared internal resume path.
//! Recovery uses normal cold-resume semantics without delaying readiness for runtime loading.
//! Already-loaded runtimes remain owned by their current clients.

use std::io;
use std::path::PathBuf;

use codex_app_server_transport::daemon_recovery;

pub(crate) async fn snapshot(
    path: PathBuf,
    saved: daemon_recovery::RecoverySnapshot,
) -> io::Result<()> {
    // A forced exit must not wait for file I/O in Tokio's blocking pool.
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("daemon-snapshot".into())
        .spawn(move || {
            let result = daemon_recovery::write_snapshot(&path, &saved);
            if result.is_err()
                && let Err(err) = std::fs::remove_file(&path)
                && err.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!("failed to clear stale daemon recovery file: {err}");
            }
            let _ = result_tx.send(result);
        })?;
    result_rx.await.map_err(io::Error::other)?
}

/// Restore runtimes while preserving committed maintenance until its owner verifies readiness.
pub(crate) async fn start_recovery(
    path: PathBuf,
    processor: std::sync::Arc<crate::message_processor::MessageProcessor>,
) -> io::Result<tokio::task::JoinHandle<()>> {
    let read_path = path.clone();
    let candidates = tokio::task::spawn_blocking(move || {
        let path = read_path;
        let candidates = daemon_recovery::read_snapshot(&path)?;
        if candidates.maintenance.is_some() {
            return Ok(candidates);
        }
        // Valid legacy snapshots retain their established one-start lifetime.
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(candidates),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(candidates),
            Err(err) => Err(err),
        }
    })
    .await
    .map_err(io::Error::other)??;
    if candidates.maintenance.is_some() {
        let admission = processor
            .turn_admission
            .begin_maintenance()
            .ok_or_else(|| io::Error::other("maintenance restoration is already active"))?;
        processor.turn_admission.seal_maintenance();
        processor.turn_admission.restoration_started();
        return Ok(tokio::spawn(async move {
            match processor.restore_maintenance(candidates, admission).await {
                Ok(()) => {
                    // Only the detached owner may acknowledge this generation after
                    // real fresh-session verification. A crash before that point must
                    // leave the checkpoint available to the previous release.
                    processor.turn_admission.restoration_finished();
                }
                Err(reason) => {
                    // A partial restore cannot advertise readiness or consume the
                    // receipt needed for operator recovery.
                    processor.turn_admission.restoration_failed();
                    tracing::error!(%reason, "maintenance restoration incomplete; new sessions remain available");
                }
            }
        }));
    }
    Ok(tokio::spawn(async move {
        processor.restore_daemon_threads(candidates).await;
    }))
}
