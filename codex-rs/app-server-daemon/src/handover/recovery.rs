//! Durable checkpoint ownership and compatible recovery after owner failure.
use super::*;

pub(crate) async fn recovery_binary(daemon: &Daemon) -> Result<Option<PathBuf>> {
    let snapshot =
        codex_app_server_transport::daemon_recovery::read_snapshot(&daemon.recovery_file()?)?;
    let Some(checkpoint) = snapshot.maintenance else {
        return Ok(None);
    };
    let record = read(daemon)?;
    ensure!(
        checkpoint.operation_id == record.operation_id,
        "checkpoint has no matching lifecycle owner"
    );
    // Recovery must use the last known serving release, even if an installer
    // selected another package after the handover owner died.
    ensure!(
        compatibility(&record.previous).await? == record.compatibility,
        "previous release no longer matches the committed checkpoint"
    );
    Ok(Some(record.previous))
}

// The lifecycle lock prevents another operation from replacing this generation
// between verification and acknowledgment.
pub(super) fn acknowledge(daemon: &Daemon, record: &HandoverStatus) -> Result<()> {
    let path = daemon.recovery_file()?;
    let snapshot = codex_app_server_transport::daemon_recovery::read_snapshot(&path)?;
    ensure!(
        snapshot
            .maintenance
            .as_ref()
            .is_some_and(|saved| saved.operation_id == record.operation_id),
        "committed checkpoint changed before acknowledgment"
    );
    std::fs::remove_file(path)?;
    Ok(())
}

/// Reconcile an orphan before a new request can overwrite its owner record.
/// The caller holds the ordinary daemon lifecycle lock throughout verification.
pub(crate) async fn finish_orphaned(daemon: &Daemon) -> Result<()> {
    let snapshot =
        codex_app_server_transport::daemon_recovery::read_snapshot(&daemon.recovery_file()?)?;
    let Some(checkpoint) = snapshot.maintenance else {
        return Ok(());
    };
    let mut record = read(daemon)?;
    ensure!(
        checkpoint.operation_id == record.operation_id,
        "checkpoint has no matching lifecycle owner"
    );
    let MaintenanceResponse::Status { executable, .. } = status_at(&daemon.socket_path).await?
    else {
        anyhow::bail!("cannot inspect recovered server");
    };
    let serving = executable_identity(&executable).await?;
    let target = executable_identity(&record.target).await.ok();
    let previous = executable_identity(&record.previous).await?;
    ensure!(
        target.as_ref() == Some(&serving) || serving == previous,
        "unexpected release owns the recovery endpoint"
    );
    ensure!(
        compatibility(&executable).await? == record.compatibility,
        "recovered release no longer matches the checkpoint"
    );
    wait_ready(daemon, &executable).await?;
    acknowledge(daemon, &record)?;
    record.phase = if target.as_ref() == Some(&serving) {
        HandoverPhase::Succeeded
    } else {
        HandoverPhase::RolledBack
    };
    record.reason = Some("Verified recovery after the activation owner exited".into());
    save(daemon, &record)
}
