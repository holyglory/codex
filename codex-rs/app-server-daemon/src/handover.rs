//! Cooperative upgrades owned by the existing detached daemon lifecycle.
use crate::Daemon;
use crate::backend::PidBackend;
use crate::client;
use crate::managed_install::{executable_identity, resolved_managed_codex_bin};
use anyhow::{Context, Result, ensure};
use codex_app_server_transport::maintenance::{MaintenanceCommand, MaintenanceResponse};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HandoverPhase {
    Queued,
    Preparing,
    Committing,
    Starting,
    Succeeded,
    Cancelled,
    Failed,
    RolledBack,
    NeedsAttention,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoverStatus {
    pub operation_id: String,
    pub phase: HandoverPhase,
    pub target: PathBuf,
    pub previous: PathBuf,
    pub reason: Option<String>,
}

fn record_path(daemon: &Daemon) -> PathBuf {
    daemon.pid_file.with_file_name("handover.json")
}

fn save(daemon: &Daemon, record: &HandoverStatus) -> Result<()> {
    let path = record_path(daemon);
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().context("handover parent")?)?;
    serde_json::to_writer(&mut temporary, record)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path).map_err(|error| error.error)?;
    Ok(())
}

fn read(daemon: &Daemon) -> Result<HandoverStatus> {
    let bytes = std::fs::read(record_path(daemon))?;
    ensure!(bytes.len() < 65536, "invalid handover receipt");
    Ok(serde_json::from_slice(&bytes)?)
}

async fn status_at(path: &Path) -> Result<MaintenanceResponse> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut socket = client::connect_at(path, "ws://localhost/daemon/maintenance").await?;
        socket.send(Message::Text(serde_json::to_string(&MaintenanceCommand::Status)?.into())).await?;
        let frame = socket.next().await.context("maintenance status closed")??;
        let Message::Text(text) = frame else { anyhow::bail!("invalid maintenance status") };
        ensure!(text.len() < 16384, "invalid maintenance status size");
        let response = serde_json::from_str(&text)?;
        socket.close(None).await?;
        Ok(response)
    }).await.context("maintenance status timed out")?
}

/// Requests a detached operation and returns before any participating agent pauses.
pub async fn request_handover() -> Result<HandoverStatus> {
    let daemon = Daemon::from_environment()?;
    let _lock = daemon.acquire_operation_lock().await?;
    request_locked(&daemon).await
}

pub(crate) async fn request_locked(daemon: &Daemon) -> Result<HandoverStatus> {
    let target = resolved_managed_codex_bin(&daemon.current_managed_codex_bin()?).await?;
    let owner_binary = target.clone();
    let worker = PidBackend::new_handover(owner_binary, daemon.pid_file.with_file_name("handover.pid"));
    if worker.is_starting_or_running().await? {
        let record = read(&daemon)?;
        ensure!(record.target == target, "another handover target is already owned");
        return Ok(record);
    }
    let MaintenanceResponse::Status { executable: previous, accepting: true, .. } = status_at(&daemon.socket_path).await?
        else { anyhow::bail!("server is not accepting cooperative maintenance") };
    let record = HandoverStatus {
        operation_id: format!("handover-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()),
        phase: HandoverPhase::Queued,
        target,
        previous,
        reason: None,
    };
    save(&daemon, &record)?;
    let _ = std::fs::remove_file(daemon.pid_file.with_file_name("handover.cancel"));
    worker.start().await?;
    Ok(record)
}

pub async fn handover_status() -> Result<HandoverStatus> {
    read(&Daemon::from_environment()?)
}

pub async fn cancel_handover() -> Result<HandoverStatus> {
    let daemon = Daemon::from_environment()?;
    let record = read(&daemon)?;
    ensure!(matches!(record.phase, HandoverPhase::Queued | HandoverPhase::Preparing),
        "handover has reached its commit boundary; cancellation cannot interrupt recovery");
    std::fs::write(daemon.pid_file.with_file_name("handover.cancel"), &record.operation_id)?;
    Ok(record)
}

async fn compatibility(binary: &Path) -> Result<Vec<u8>> {
    let output = tokio::time::timeout(Duration::from_secs(10), Command::new(binary)
        .args(["app-server", "daemon", "handover-compatibility"])
        .stdin(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).output()).await??;
    ensure!(output.status.success() && output.stdout.len() < 65536,
        "package does not support verified cooperative handover");
    Ok(output.stdout)
}

async fn wait_ready(daemon: &Daemon, expected: &Path) -> Result<()> {
    let identity = executable_identity(expected).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(MaintenanceResponse::Status { executable, accepting: true, .. }) = status_at(&daemon.socket_path).await
            && executable_identity(&executable).await? == identity
        {
            client::probe(&daemon.socket_path).await?;
            client::verify_session_admission(&daemon.socket_path).await?;
            return Ok(());
        }
        ensure!(tokio::time::Instant::now() < deadline, "replacement did not become ready");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Runs only in the detached PID-backed handover owner, outside agent turns.
pub async fn run_handover() -> Result<()> {
    let daemon = Daemon::from_environment()?;
    let _lock = daemon.acquire_operation_lock().await?;
    let mut record = read(&daemon)?;
    let result = perform(&daemon, &mut record).await;
    if let Err(error) = result {
        if !matches!(record.phase, HandoverPhase::RolledBack | HandoverPhase::NeedsAttention) {
            record.phase = HandoverPhase::Failed;
        }
        record.reason = Some("handover failed; inspect the retained owner log".into());
        save(&daemon, &record)?;
        return Err(error);
    }
    save(&daemon, &record)?;
    Ok(())
}

async fn perform(daemon: &Daemon, record: &mut HandoverStatus) -> Result<()> {
    ensure!(resolved_managed_codex_bin(&daemon.current_managed_codex_bin()?).await? == record.target,
        "selected package changed before handover");
    // Equality of embedded migrations is intentionally conservative. Schema
    // changes require an explicit migration/recovery plan, not binary rollback.
    ensure!(compatibility(&record.target).await? == compatibility(&record.previous).await?,
        "package schemas differ; cooperative rollback has not been established");
    let settings = daemon.load_settings().await?;
    let backend = daemon.running_backend_instance(&settings).await?.context("managed server is not running")?;
    ensure!(backend.running_executable_identity().await? == Some(executable_identity(&record.previous).await?),
        "serving package changed before handover");
    #[cfg(windows)]
    crate::backend::windows::ensure_detached_launch(&record.target)?;
    let MaintenanceResponse::Status { pid, accepting: true, .. } = status_at(&daemon.socket_path).await?
        else { anyhow::bail!("server cannot prepare maintenance") };
    record.phase = HandoverPhase::Preparing;
    save(daemon, record)?;
    let mut socket = client::connect_at(&daemon.socket_path, "ws://localhost/daemon/maintenance").await?;
    socket.send(Message::Text(serde_json::to_string(&MaintenanceCommand::Prepare {
        operation_id: record.operation_id.clone(), pid,
    })?.into())).await?;
    let cancel_path = daemon.pid_file.with_file_name("handover.cancel");
    let operation = record.operation_id.clone();
    let cancellation = async {
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        loop {
            interval.tick().await;
            if tokio::fs::read_to_string(&cancel_path).await.ok().as_deref() == Some(operation.as_str()) { break; }
        }
    };
    let frame = tokio::select! {
        _ = cancellation => {
            socket.close(None).await?;
            record.phase = HandoverPhase::Cancelled;
            return Ok(());
        }
        frame = tokio::time::timeout(Duration::from_secs(75), socket.next()) => frame?.context("maintenance owner disconnected")??,
    };
    let Message::Text(text) = frame else { anyhow::bail!("invalid checkpoint receipt") };
    ensure!(text.len() < 4096, "invalid checkpoint receipt size");
    ensure!(serde_json::from_str::<MaintenanceResponse>(&text)? == MaintenanceResponse::Ready { operation_id: record.operation_id.clone(), pid },
        "server did not checkpoint all work");
    ensure!(resolved_managed_codex_bin(&daemon.current_managed_codex_bin()?).await? == record.target,
        "selected package changed during preparation");
    if tokio::fs::read_to_string(&cancel_path).await.ok().as_deref() == Some(record.operation_id.as_str()) {
        socket.close(None).await?;
        record.phase = HandoverPhase::Cancelled;
        return Ok(());
    }
    record.phase = HandoverPhase::Committing;
    save(daemon, record)?;
    socket.send(Message::Text(serde_json::to_string(&MaintenanceCommand::Commit {
        operation_id: record.operation_id.clone(), pid,
    })?.into())).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while backend.is_starting_or_running().await? {
        ensure!(tokio::time::Instant::now() < deadline, "old server has not committed its exit; no process was killed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(socket);
    record.phase = HandoverPhase::Starting;
    save(daemon, record)?;
    let started = daemon.start_managed_backend_with_bin(&settings, &record.target).await;
    if started.is_ok() && wait_ready(daemon, &record.target).await.is_ok() {
        record.phase = HandoverPhase::Succeeded;
        return Ok(());
    }
    // Never launch another writer while an unhealthy replacement remains alive.
    if daemon.running_backend_instance(&settings).await?.is_some() {
        record.phase = HandoverPhase::NeedsAttention;
        anyhow::bail!("replacement is alive but not ready; preserved for diagnosis");
    }
    daemon.start_managed_backend_with_bin(&settings, &record.previous).await?;
    wait_ready(daemon, &record.previous).await?;
    record.phase = HandoverPhase::RolledBack;
    anyhow::bail!("replacement failed; previous compatible release restored")
}
