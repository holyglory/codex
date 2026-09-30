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
