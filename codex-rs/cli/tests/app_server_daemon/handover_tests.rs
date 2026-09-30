//! Complete detached handover through the public CLI and real server process.
use super::*;
use pretty_assertions::assert_eq;

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
    assert!(!state.join("loaded-threads.json").exists());
    Ok(())
}

#[test]
fn cooperative_handover_failed_start_restores_previous_server() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let daemon = TestDaemon::new()?;
    let state = daemon.home.path().join("app-server-daemon");
    std::fs::write(
        state.join("settings.json"),
        br#"{"updater":{"autoUpdateEnabled":false}}"#,
    )?;
    daemon.lifecycle("start")?;
    let standalone = daemon.home.path().join("packages/standalone");
    let candidate = standalone.join("releases/failing-candidate/bin/codex");
    std::fs::create_dir_all(candidate.parent().context("candidate parent")?)?;
    let executable = format!(
        "'{}'",
        daemon.codex.display().to_string().replace('\'', "'\\''")
    );
    let script = format!(
        r#"#!/bin/sh
case "$*" in
  "--version"|"app-server daemon handover-compatibility"|"app-server daemon handover-worker"|"app-server --managed-daemon --help") exec {executable} "$@" ;;
  *) exit 73 ;;
esac
"#
    );
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
            "failed" | "needsAttention" | "succeeded" => {
                anyhow::bail!("unexpected replacement outcome: {current}")
            }
            _ => {}
        }
        ensure!(
            Instant::now() < deadline,
            "rollback exceeded its acceptance deadline"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(daemon.lifecycle("version")?["status"], "running");
    Ok(())
}

struct DaemonProxy {
    child: Child,
    output: std::io::BufReader<std::process::ChildStdout>,
}

impl DaemonProxy {
    fn connect(daemon: &TestDaemon) -> Result<Self> {
        use std::io::Write;
        let socket =
            codex_app_server_transport::app_server_control_socket_path(daemon.home.path())?;
        let mut child = daemon
            .command()
            .args(["app-server", "proxy", "--sock"])
            .arg(socket.as_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let output = std::io::BufReader::new(child.stdout.take().context("proxy output")?);
        let mut proxy = Self { child, output };
        proxy.request(
            "initialize",
            serde_json::json!({"clientInfo":{"name":"handover-test","version":"1"}}),
        )?;
        writeln!(
            proxy.child.stdin.as_mut().context("proxy input")?,
            "{}",
            serde_json::json!({"method":"initialized"})
        )?;
        Ok(proxy)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        use std::io::BufRead;
        use std::io::Write;
        let input = self.child.stdin.as_mut().context("proxy input")?;
        writeln!(
            input,
            "{}",
            serde_json::json!({"id":2,"method":method,"params":params})
        )?;
        input.flush()?;
        loop {
            let mut line = String::new();
            ensure!(
                self.output.read_line(&mut line)? > 0,
                "proxy closed before its response"
            );
            let row: Value = serde_json::from_str(&line)?;
            if row["id"] == 2 && row.get("method").is_none() {
                ensure!(row.get("error").is_none(), "request rejected: {row}");
                return Ok(row["result"].clone());
            }
        }
    }
}

impl Drop for DaemonProxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn cooperative_handover_fresh_start_recovers_after_owner_exit() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let daemon = TestDaemon::new()?;
    let state = daemon.home.path().join("app-server-daemon");
    std::fs::write(
        state.join("settings.json"),
        br#"{"updater":{"autoUpdateEnabled":false}}"#,
    )?;
    daemon.lifecycle("start")?;
    let mut proxy = DaemonProxy::connect(&daemon)?;
    let created = proxy.request("thread/start", serde_json::json!({}))?;
    let thread_id = created["thread"]["id"]
        .as_str()
        .context("created thread")?
        .to_string();
    let standalone = daemon.home.path().join("packages/standalone");
    let candidate = standalone.join("releases/owner-exit/bin/codex");
    std::fs::create_dir_all(candidate.parent().context("candidate parent")?)?;
    let executable = format!(
        "'{}'",
        daemon.codex.display().to_string().replace('\'', "'\\''")
    );
    let script = format!(
        r#"#!/bin/sh
if [ "$1 $2" = "app-server --listen" ] && [ ! -f "$CODEX_HOME/candidate-entered" ]; then
  echo $$ > "$CODEX_HOME/candidate-entered"
  while [ ! -f "$CODEX_HOME/release-candidate" ]; do sleep 0.05; done
fi
exec {executable} "$@"
"#
    );
    std::fs::write(&candidate, script)?;
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755))?;
    let selected = standalone.join("selected-next");
    std::os::unix::fs::symlink("releases/owner-exit", &selected)?;
    std::fs::rename(selected, standalone.join("current"))?;
    daemon.lifecycle("handover")?;
    let deadline = Instant::now() + Duration::from_secs(45);
    let candidate_pid = loop {
        if let Ok(pid) = std::fs::read_to_string(daemon.home.path().join("candidate-entered")) {
            break pid.trim().parse::<u32>()?;
        }
        ensure!(Instant::now() < deadline, "candidate was not launched");
        std::thread::sleep(Duration::from_millis(25));
    };
    let owner = daemon.pid("handover.pid")?;
    signal(owner, libc::SIGKILL)?;
    wait_for_exit(owner)?;
    signal(candidate_pid, libc::SIGTERM)?;
    wait_for_exit(candidate_pid)?;
    let snapshot = state.join("loaded-threads.json");
    assert!(
        snapshot.exists(),
        "committed checkpoint must survive the owner"
    );
    daemon.lifecycle("start")?;
    let mut proxy = DaemonProxy::connect(&daemon)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let loaded = proxy.request("thread/loaded/list", serde_json::json!({}))?;
        if loaded["data"]
            .as_array()
            .context("loaded threads")?
            .iter()
            .any(|id| id == &thread_id)
        {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "fresh start lost the committed thread"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}
