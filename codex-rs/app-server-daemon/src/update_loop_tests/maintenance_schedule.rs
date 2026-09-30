//! Manual installation reports pending while the detached owner holds activation.
use super::*;
use anyhow::Context;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use std::os::unix::fs::PermissionsExt;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn manual_update_schedules_cooperative_activation_without_claiming_serving_version()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let (daemon, _) = manual_update_daemon(&home);
    let old_binary = std::fs::canonicalize(&daemon.managed_codex_bin)?;
    let script = |version| {
        format!(
            "#!/bin/sh\ncase \"$*\" in\n--version) echo codex {version};;\n'app-server daemon handover-compatibility') echo maintenance-test-v1;;\n'app-server --managed-daemon --help') exit 0;;\n*) exec sleep 30;;\nesac\n"
        )
    };
    std::fs::write(&old_binary, script("1.0.0"))?;
    std::fs::write(
        &daemon.settings_file,
        r#"{"updater":{"autoUpdateEnabled":false}}"#,
    )?;
    let settings = daemon.load_settings().await?;
    let backend = crate::backend::pid_backend(daemon.backend_paths(&settings));
    backend.start().await?;
    let original_record = std::fs::read(&daemon.pid_file)?;
    let pid = serde_json::from_slice::<serde_json::Value>(&original_record)?["pid"]
        .as_u64()
        .context("serving pid")?;
    let root = home.path().join("packages/standalone");
    let target = if cfg!(target_os = "macos") {
        format!("{}-apple-darwin", std::env::consts::ARCH)
    } else {
        format!("{}-unknown-linux-musl", std::env::consts::ARCH)
    };
    let release = format!("1.1.0-{target}");
    let selected = root.join("releases").join(&release).join("bin/codex");
    std::fs::create_dir_all(selected.parent().context("new binary parent")?)?;
    std::fs::write(&selected, script("1.1.0"))?;
    std::fs::set_permissions(&selected, std::fs::Permissions::from_mode(0o755))?;
    std::fs::remove_file(root.join("current"))?;
    std::os::unix::fs::symlink(format!("releases/{release}"), root.join("current"))?;
    std::fs::write(root.join("auto-update-version"), release)?;
    std::fs::create_dir_all(daemon.socket_path.parent().context("socket parent")?)?;
    let mut listener = codex_uds::UnixListener::bind(&daemon.socket_path).await?;
    let server = tokio::spawn(async move {
        loop {
            let connection = listener.accept().await?;
            let mut socket = tokio_tungstenite::accept_async(connection).await?;
            let Some(Ok(Message::Text(text))) = socket.next().await else {
                continue;
            };
            let request: serde_json::Value = serde_json::from_str(&text)?;
            let response = if request["type"] == "status" {
                serde_json::json!({"type":"status","pid":pid,"accepting":true,"preparing":false,"restored":true,"executable":old_binary})
            } else {
                serde_json::json!({"id":request["id"],"result":{"userAgent":"codex_app_server_daemon/1.0.0"}})
            };
            socket
                .send(Message::Text(response.to_string().into()))
                .await?;
            let _ = socket.next().await;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });
    let no_op = FakeInstallerHttp::new(InstallerResponse::Success(
        b"#!/bin/sh\n# CODEX_INSTALL_IF_LATEST CODEX_INSTALL_DAEMON_ONLY\nexit 0\n".to_vec(),
    ));
    let result = manual_update_once(
        &no_op,
        &daemon,
        &executable_identity_from_bytes(b"updater"),
        &mut test_terminate(),
        super::super::UpdateTrigger::Manual,
    )
    .await;
    let worker = crate::backend::PidBackend::new_handover(
        selected,
        daemon.pid_file.with_file_name("handover.pid"),
    );
    let owned = worker.is_starting_or_running().await?;
    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(
        daemon.pid_file.with_file_name("handover.json"),
    )?)?;
    let still_running = backend.is_starting_or_running().await?;
    let after_record = std::fs::read(&daemon.pid_file)?;
    worker.stop().await?;
    backend.stop().await?;
    server.abort();
    let output = result?;
    assert_eq!(output.status, UpdateStatus::Pending);
    assert_eq!(output.installed_version.as_deref(), Some("1.1.0"));
    assert_eq!(output.running_version.as_deref(), Some("1.0.0"));
    assert_eq!(record["phase"], "queued");
    assert_eq!(after_record, original_record);
    assert!(owned && still_running);
    Ok(())
}
