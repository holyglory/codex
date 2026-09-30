//! Cooperative upgrades owned by the existing detached daemon lifecycle.
pub(crate) mod recovery;
use crate::Daemon;
use crate::backend::PidBackend;
use crate::client;
use crate::managed_install::executable_identity;
use crate::managed_install::resolved_managed_codex_bin;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_transport::maintenance::MaintenanceCommand;
use codex_app_server_transport::maintenance::MaintenanceResponse;
use futures::SinkExt;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
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
    pub compatibility: String,
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
        socket
            .send(Message::Text(
                serde_json::to_string(&MaintenanceCommand::Status)?.into(),
            ))
            .await?;
        let frame = socket.next().await.context("maintenance status closed")??;
        let Message::Text(text) = frame else {
            anyhow::bail!("invalid maintenance status")
        };
        ensure!(text.len() < 16384, "invalid maintenance status size");
        let response = serde_json::from_str(&text)?;
        socket.close(None).await?;
        Ok(response)
    })
    .await
    .context("maintenance status timed out")?
}

/// Requests a detached operation and returns before any participating agent pauses.
pub async fn request_handover() -> Result<HandoverStatus> {
    crate::ensure_supported_platform()?;
    #[cfg(windows)]
    crate::backend::windows::ensure_not_elevated()?;
    let daemon = Daemon::from_environment()?;
    let selected = resolved_managed_codex_bin(&daemon.current_managed_codex_bin()?).await?;
    let worker = PidBackend::new_handover(
        selected.clone(),
        daemon.pid_file.with_file_name("handover.pid"),
    );
    if worker.is_starting_or_running().await? {
        let record = read(&daemon)?;
        ensure!(
            record.target == selected,
            "another handover target is already owned"
        );
        return Ok(record);
    }
    let _lock = daemon.acquire_operation_lock().await?;
    request_locked(&daemon).await
}

pub(crate) async fn request_locked(daemon: &Daemon) -> Result<HandoverStatus> {
    let target = resolved_managed_codex_bin(&daemon.current_managed_codex_bin()?).await?;
    let owner_binary = target.clone();
    let worker =
        PidBackend::new_handover(owner_binary, daemon.pid_file.with_file_name("handover.pid"));
    if worker.is_starting_or_running().await? {
        let record = read(&daemon)?;
        ensure!(
            record.target == target,
            "another handover target is already owned"
        );
        return Ok(record);
    }
    recovery::finish_orphaned(daemon).await?;
    let MaintenanceResponse::Status {
        executable: previous,
        accepting: true,
        restored: true,
        ..
    } = status_at(&daemon.socket_path).await?
    else {
        anyhow::bail!("server is not accepting cooperative maintenance")
    };
    let compatibility = compatibility(&target).await?;
    ensure!(
        compatibility == self::compatibility(&previous).await?,
        "selected package cannot perform a compatible cooperative handover"
    );
    let record = HandoverStatus {
        operation_id: format!(
            "handover-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ),
        phase: HandoverPhase::Queued,
        target,
        previous,
        reason: None,
        compatibility,
    };
    save(&daemon, &record)?;
    let _ = std::fs::remove_file(daemon.pid_file.with_file_name("handover.cancel"));
    worker.start().await?;
    Ok(record)
}

pub async fn handover_status() -> Result<HandoverStatus> {
    let daemon = Daemon::from_environment()?;
    let mut record = read(&daemon)?;
    if matches!(
        record.phase,
        HandoverPhase::Queued
            | HandoverPhase::Preparing
            | HandoverPhase::Committing
            | HandoverPhase::Starting
    ) {
        let worker = PidBackend::new_handover(
            record.target.clone(),
            daemon.pid_file.with_file_name("handover.pid"),
        );
        if !worker.is_starting_or_running().await? {
            // The requester owns this lock until PID publication, so an in-flight
            // launch cannot be mistaken for an exited owner. This observation
            // does not rewrite the last durable phase or claim a successful update.
            let lock = tokio::fs::OpenOptions::new()
                .read(true)
                .open(&daemon.operation_lock_file)
                .await?;
            if crate::try_lock_file(&lock)? {
                record.phase = HandoverPhase::NeedsAttention;
                record.reason = Some("activation owner exited before recording completion; daemon start preserves committed work".into());
            }
        }
    }
    Ok(record)
}

pub async fn cancel_handover() -> Result<HandoverStatus> {
    let daemon = Daemon::from_environment()?;
    let record = read(&daemon)?;
    ensure!(
        matches!(
            record.phase,
            HandoverPhase::Queued | HandoverPhase::Preparing
        ),
        "handover has reached its commit boundary; cancellation cannot interrupt recovery"
    );
    std::fs::write(
        daemon.pid_file.with_file_name("handover.cancel"),
        &record.operation_id,
    )?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(/*secs*/ 10);
    loop {
        let current = read(&daemon)?;
        ensure!(
            current.operation_id == record.operation_id,
            "handover operation changed while cancelling"
        );
        match current.phase {
            HandoverPhase::Cancelled => return Ok(current),
            HandoverPhase::Queued | HandoverPhase::Preparing => {}
            HandoverPhase::Committing
            | HandoverPhase::Starting
            | HandoverPhase::Succeeded
            | HandoverPhase::Failed
            | HandoverPhase::RolledBack
            | HandoverPhase::NeedsAttention => {
                anyhow::bail!(
                    "cancellation was not accepted before the commit boundary; inspect handover status"
                )
            }
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "cancellation has not been acknowledged; inspect handover status"
        );
        tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
    }
}

async fn compatibility(binary: &Path) -> Result<String> {
    let isolated = tempfile::tempdir()?;
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(binary)
            .env("CODEX_HOME", isolated.path())
            .env("CODEX_SQLITE_HOME", isolated.path())
            .args(["app-server", "daemon", "handover-compatibility"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    ensure!(
        output.status.success() && output.stdout.len() < 65536,
        "package does not support verified cooperative handover"
    );
    Ok(blake3::hash(&output.stdout).to_hex().to_string())
}

async fn wait_ready(daemon: &Daemon, expected: &Path) -> Result<()> {
    let identity = executable_identity(expected).await?;
    let settings = daemon.load_settings().await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(/*secs*/ 75);
    loop {
        if let Ok(MaintenanceResponse::Status {
            executable,
            accepting: true,
            restored: true,
            ..
        }) = status_at(&daemon.socket_path).await
            && executable_identity(&executable).await? == identity
        {
            client::probe(&daemon.socket_path).await?;
            client::verify_session_admission(&daemon.socket_path).await?;
            return Ok(());
        }
        ensure!(
            daemon.running_backend_instance(&settings).await?.is_some(),
            "replacement exited before readiness verification"
        );
        ensure!(
            tokio::time::Instant::now() < deadline,
            "replacement did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Runs only in the detached PID-backed handover owner, outside agent turns.
pub async fn run_handover() -> Result<()> {
    crate::ensure_supported_platform()?;
    #[cfg(windows)]
    crate::backend::windows::ensure_not_elevated()?;
    let daemon = Daemon::from_environment()?;
    let _lock = daemon.acquire_operation_lock().await?;
    let mut record = read(&daemon)?;
    let result = perform(&daemon, &mut record).await;
    if let Err(error) = result {
        if !matches!(
            record.phase,
            HandoverPhase::RolledBack | HandoverPhase::NeedsAttention
        ) {
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
    ensure!(
        resolved_managed_codex_bin(&daemon.current_managed_codex_bin()?).await? == record.target,
        "selected package changed before handover"
    );
    // Equality of embedded migrations is intentionally conservative. Schema
    // changes require an explicit migration/recovery plan, not binary rollback.
    ensure!(
        compatibility(&record.target).await? == record.compatibility
            && compatibility(&record.previous).await? == record.compatibility,
        "package schemas differ; cooperative rollback has not been established"
    );
    let settings = daemon.load_settings().await?;
    let backend = daemon
        .running_backend_instance(&settings)
        .await?
        .context("managed server is not running")?;
    ensure!(
        backend.running_executable_identity().await?
            == Some(executable_identity(&record.previous).await?),
        "serving package changed before handover"
    );
    #[cfg(windows)]
    crate::backend::windows::ensure_detached_launch(&record.target)?;
    let MaintenanceResponse::Status {
        pid,
        accepting: true,
        restored: true,
        ..
    } = status_at(&daemon.socket_path).await?
    else {
        anyhow::bail!("server cannot prepare maintenance")
    };
    record.phase = HandoverPhase::Preparing;
    save(daemon, record)?;
    let mut socket =
        client::connect_at(&daemon.socket_path, "ws://localhost/daemon/maintenance").await?;
    socket
        .send(Message::Text(
            serde_json::to_string(&MaintenanceCommand::Prepare {
                operation_id: record.operation_id.clone(),
                pid,
            })?
            .into(),
        ))
        .await?;
    let cancel_path = daemon.pid_file.with_file_name("handover.cancel");
    let operation = record.operation_id.clone();
    let cancellation = async {
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        loop {
            interval.tick().await;
            if tokio::fs::read_to_string(&cancel_path)
                .await
                .ok()
                .as_deref()
                == Some(operation.as_str())
            {
                break;
            }
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
    let Message::Text(text) = frame else {
        anyhow::bail!("invalid checkpoint receipt")
    };
    ensure!(text.len() < 4096, "invalid checkpoint receipt size");
    ensure!(
        serde_json::from_str::<MaintenanceResponse>(&text)?
            == MaintenanceResponse::Ready {
                operation_id: record.operation_id.clone(),
                pid
            },
        "server did not checkpoint all work"
    );
    let codex_home = daemon
        .settings_file
        .parent()
        .and_then(Path::parent)
        .context("daemon home")?;
    let _install_lock = crate::install_lock::acquire_install_lock(
        &crate::managed_install::package_root(codex_home),
    )
    .await?;
    ensure!(
        resolved_managed_codex_bin(&daemon.current_managed_codex_bin()?).await? == record.target,
        "selected package changed during preparation"
    );
    if tokio::fs::read_to_string(&cancel_path)
        .await
        .ok()
        .as_deref()
        == Some(record.operation_id.as_str())
    {
        socket.close(None).await?;
        record.phase = HandoverPhase::Cancelled;
        return Ok(());
    }
    record.phase = HandoverPhase::Committing;
    save(daemon, record)?;
    socket
        .send(Message::Text(
            serde_json::to_string(&MaintenanceCommand::Commit {
                operation_id: record.operation_id.clone(),
                pid,
            })?
            .into(),
        ))
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while backend.is_starting_or_running().await? {
        if tokio::time::Instant::now() >= deadline {
            let saved = codex_app_server_transport::daemon_recovery::read_snapshot(
                &daemon.recovery_file()?,
            )?;
            ensure!(
                saved
                    .maintenance
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.operation_id == record.operation_id
                        && checkpoint.source_pid == pid),
                "old server did not commit a checkpoint; no process was killed"
            );
            // Every participant is already parked and persisted. This bounds
            // server teardown without interrupting uncheckpointed agent work.
            backend.stop_with_grace(/*grace_seconds*/ 0).await?;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(socket);
    let checkpoint =
        codex_app_server_transport::daemon_recovery::read_snapshot(&daemon.recovery_file()?)?;
    ensure!(
        checkpoint
            .maintenance
            .as_ref()
            .is_some_and(
                |saved| saved.operation_id == record.operation_id && saved.source_pid == pid
            ),
        "old server exited without its committed checkpoint; activation is incomplete"
    );
    record.phase = HandoverPhase::Starting;
    save(daemon, record)?;
    let started = daemon
        .start_managed_backend_with_bin(&settings, &record.target)
        .await;
    if started.is_ok() && wait_ready(daemon, &record.target).await.is_ok() {
        recovery::acknowledge(daemon, record)?;
        record.phase = HandoverPhase::Succeeded;
        return Ok(());
    }
    // Never launch another writer while an unhealthy replacement remains alive.
    if daemon.running_backend_instance(&settings).await?.is_some() {
        record.phase = HandoverPhase::NeedsAttention;
        anyhow::bail!("replacement is alive but not ready; preserved for diagnosis");
    }
    daemon
        .start_managed_backend_with_bin(&settings, &record.previous)
        .await?;
    wait_ready(daemon, &record.previous).await?;
    recovery::acknowledge(daemon, record)?;
    record.phase = HandoverPhase::RolledBack;
    anyhow::bail!("replacement failed; previous compatible release restored")
}
