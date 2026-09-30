//! Shared on-disk candidate set for managed daemon restarts.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use codex_core::path_utils::write_atomically;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct RecoverySnapshot {
    #[serde(skip)]
    pub loaded: BTreeSet<String>,
    pub interrupted: BTreeMap<String, InterruptedTurn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<MaintenanceSnapshot>,
}

/// A complete, generation-bound set of runtimes sealed before maintenance commit.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MaintenanceSnapshot {
    pub operation_id: String,
    pub source_pid: u32,
    /// Roots have no parent; children are loaded after their immediate owner.
    pub parents: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub mailboxes: BTreeMap<String, Vec<codex_core::MaintenanceMail>>,
    pub turn_contexts: BTreeMap<String, codex_core::MaintenanceTurnContext>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct InterruptedTurn {
    pub turn_id: String,
    pub output_schema: Option<serde_json::Value>,
    pub service_tier: Option<String>,
    pub cyber_access_program: Option<codex_protocol::turn_input::CyberAccessProgram>,
    /// Only local execution with thread-owned configuration can continue automatically.
    /// Older snapshots without this identity are reloaded without continuation.
    pub local_environment: Option<codex_app_server_protocol::ThreadEnvironment>,
}

// Old servers accept the array and skip this non-thread entry during best-effort
// restoration. Keeping metadata in the same atomic file avoids stale sidecars.
const INTERRUPTION_PREFIX: &str = "codex-interrupted-v1:";
const MAINTENANCE_PREFIX: &str = "codex-maintenance-v1:";

pub fn read_snapshot(path: &Path) -> io::Result<RecoverySnapshot> {
    let mut loaded: BTreeSet<String> = match std::fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents).map_err(io::Error::other)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(RecoverySnapshot::default()),
        Err(err) => return Err(err),
    };
    let mut snapshot = RecoverySnapshot::default();
    let mut invalid_maintenance = false;
    let mut maintenance_seen = false;
    loaded.retain(|entry| {
        if let Some(metadata) = entry.strip_prefix(MAINTENANCE_PREFIX) {
            if maintenance_seen {
                invalid_maintenance = true;
            }
            maintenance_seen = true;
            match serde_json::from_str::<RecoverySnapshot>(metadata) {
                Ok(saved) if saved.maintenance.is_some() => snapshot = saved,
                _ => invalid_maintenance = true,
            }
            return false;
        }
        if let Some(metadata) = entry.strip_prefix(INTERRUPTION_PREFIX) {
            if let Ok(saved) = serde_json::from_str::<RecoverySnapshot>(metadata) {
                snapshot = saved;
            }
            false
        } else {
            true
        }
    });
    if let Some(maintenance) = &snapshot.maintenance {
        invalid_maintenance |= maintenance.operation_id.is_empty()
            || maintenance.source_pid == 0
            || maintenance.parents.keys().cloned().collect::<BTreeSet<_>>() != loaded
            || snapshot.interrupted.keys().any(|id| !loaded.contains(id))
            || maintenance.mailboxes.keys().any(|id| !loaded.contains(id))
            || maintenance
                .turn_contexts
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
                != snapshot
                    .interrupted
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>();
    }
    if invalid_maintenance {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid maintenance checkpoint",
        ));
    }
    snapshot.interrupted.retain(|id, _| loaded.contains(id));
    snapshot.loaded = loaded;
    Ok(snapshot)
}

pub fn read_candidates(path: &Path) -> io::Result<BTreeSet<String>> {
    Ok(read_snapshot(path)?.loaded)
}

pub fn write_candidates(path: &Path, candidates: &BTreeSet<String>) -> io::Result<()> {
    write_snapshot(
        path,
        &RecoverySnapshot {
            loaded: candidates.clone(),
            ..Default::default()
        },
    )
}

pub fn write_snapshot(path: &Path, snapshot: &RecoverySnapshot) -> io::Result<()> {
    let mut saved = snapshot.loaded.clone();
    if !snapshot.interrupted.is_empty() || snapshot.maintenance.is_some() {
        let prefix = if snapshot.maintenance.is_some() {
            MAINTENANCE_PREFIX
        } else {
            INTERRUPTION_PREFIX
        };
        saved.insert(format!(
            "{prefix}{}",
            serde_json::to_string(snapshot).map_err(io::Error::other)?
        ));
    }
    write_atomically(
        path,
        &serde_json::to_string(&saved).map_err(io::Error::other)?,
    )?;
    if snapshot.maintenance.is_some() {
        std::fs::File::open(path)?.sync_all()?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}
