//! Complete detached handover through the public CLI and real server process.
use super::*;

#[test]
fn cooperative_handover_owner_replaces_server_and_verifies_admission() -> Result<()> {
    let daemon = TestDaemon::new()?;
    let state = daemon.home.path().join("app-server-daemon");
    std::fs::write(
        state.join("settings.json"),
        br#"{"updater":{"autoUpdateEnabled":false}}"#,
    )?;
    daemon.lifecycle("start")?;
    let original_pid = daemon.pid("app-server.pid")?;
    let requested = daemon.lifecycle("handover")?;
    assert_eq!(requested["phase"], "queued");
    let operation = requested["operationId"]
        .as_str()
        .context("operation identity")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let completed = loop {
        let current: Value = serde_json::from_slice(&std::fs::read(state.join("handover.json"))?)?;
        assert_eq!(current["operationId"], operation);
        match current["phase"].as_str().context("phase")? {
            "succeeded" => break current,
            "failed" | "needsAttention" | "rolledBack" | "cancelled" => {
                let log =
                    std::fs::read_to_string(state.join("handover.stderr.log")).unwrap_or_default();
                anyhow::bail!(
                    "handover failed: {current}; {}",
                    log.chars().take(4000).collect::<String>()
                );
            }
            _ => {}
        }
        ensure!(
            Instant::now() < deadline,
            "handover exceeded its acceptance deadline"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(completed["reason"].is_null());
    assert_ne!(daemon.pid("app-server.pid")?, original_pid);
    assert_eq!(daemon.lifecycle("version")?["status"], "running");
    assert!(!codex_app_server_transport::daemon_recovery_file_path(daemon.home.path()).exists());
    Ok(())
}

#[test]
fn cooperative_handover_failed_start_restores_previous_server() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let daemon = TestDaemon::new()?;
    let state = daemon.home.path().join("app-server-daemon");
    std::fs::write(state.join("settings.json"), br#"{"updater":{"autoUpdateEnabled":false}}"#)?;
    daemon.lifecycle("start")?;
    let standalone = daemon.home.path().join("packages/standalone");
    let candidate = standalone.join("releases/failing-candidate/bin/codex");
    std::fs::create_dir_all(candidate.parent().context("candidate parent")?)?;
    let executable = format!("'{}'", daemon.codex.display().to_string().replace('\'', "'\\''"));
    let script = format!(r#"#!/bin/sh
case "$*" in
  "--version"|"app-server daemon handover-compatibility"|"app-server daemon handover-worker"|"app-server --managed-daemon --help") exec {executable} "$@" ;;
  *) exit 73 ;;
esac
"#);
    std::fs::write(&candidate, script)?;
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755))?;
    let selected = standalone.join("selected-next");
    std::os::unix::fs::symlink("releases/failing-candidate", &selected)?;
    std::fs::rename(selected, standalone.join("current"))?;
    daemon.lifecycle("handover")?;
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let current: Value = serde_json::from_slice(&std::fs::read(state.join("handover.json"))?)?;
        match current["phase"].as_str().context("phase")? {
            "rolledBack" => break,
            "failed" | "needsAttention" | "succeeded" => anyhow::bail!("unexpected replacement outcome: {current}"),
            _ => {}
        }
        ensure!(Instant::now() < deadline, "rollback exceeded its acceptance deadline");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(daemon.lifecycle("version")?["status"], "running");
    Ok(())
}
