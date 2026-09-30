//! Versioned, bounded maintenance control over the existing private daemon socket.
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum MaintenanceCommand {
    Prepare { operation_id: String, pid: u32 },
    Commit { operation_id: String, pid: u32 },
    Cancel,
    Status,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum MaintenanceResponse {
    Ready { operation_id: String, pid: u32 },
    Committed { operation_id: String, pid: u32 },
    Failed { reason: String },
    Status { pid: u32, accepting: bool },
}

#[derive(Debug)]
pub struct MaintenanceConnection {
    pub command: MaintenanceCommand,
    pub reply: oneshot::Sender<MaintenanceResponse>,
    pub commit: mpsc::Receiver<MaintenanceCommand>,
    pub cancelled: CancellationToken,
}
