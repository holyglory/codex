//! Removes stale snapshots before planned daemon replacements.

use std::io::ErrorKind;

use anyhow::Context;
use anyhow::Result;

use crate::Daemon;

pub(crate) fn discard_pending(daemon: &Daemon) -> Result<()> {
    match std::fs::remove_file(daemon.recovery_file()?) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).context("failed to clear daemon recovery file"),
    }
}

/// A fresh start after owner failure must retain a committed cooperative handoff.
/// Explicit stop/restart still use discard_pending to preserve their intent.
pub(crate) fn prepare_fresh_start(daemon: &Daemon) -> Result<()> {
    let path = daemon.recovery_file()?;
    match codex_app_server_transport::daemon_recovery::read_snapshot(&path) {
        Ok(snapshot) if snapshot.maintenance.is_some() => return Ok(()),
        // Preserve unreadable evidence. The server reports incomplete recovery
        // while keeping ordinary new work available.
        Err(_) => return Ok(()),
        Ok(_) => {}
    }
    discard_pending(daemon)
}
