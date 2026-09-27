use anyhow::Context;
use anyhow::Result;
use futures::SinkExt;
use futures::StreamExt;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn project_automation_cli_uses_persistent_remote_without_experimental_events() -> Result<()> {
    let home = TempDir::new()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _attempt in 0..2 {
            let (stream, _) = listener.accept().await?;
            let mut websocket = accept_async(stream).await?;
            let initialize = websocket.next().await.context("missing initialize")??;
            let initialize: Value = serde_json::from_str(initialize.to_text()?)?;
            assert_eq!(
                initialize["params"]["capabilities"]["experimentalApi"],
                false
            );
            websocket
                .send(Message::Text(
                    json!({"id": initialize["id"], "result": {
                        "userAgent": "codex_cli_rs/0.0.0-test"
                    }})
                    .to_string()
                    .into(),
                ))
                .await?;
            let initialized = websocket.next().await.context("missing initialized")??;
            let initialized: Value = serde_json::from_str(initialized.to_text()?)?;
            assert_eq!(initialized["method"], "initialized");
            let request = websocket
                .next()
                .await
                .context("missing project request")??;
            let request: Value = serde_json::from_str(request.to_text()?)?;
            assert_eq!(request["method"], "projectAutomation/command");
            websocket
                .send(Message::Text(
                    json!({"id": request["id"], "result": {
                        "capability": {"version": 1}, "project": null
                    }})
                    .to_string()
                    .into(),
                ))
                .await?;
            requests.push(request["params"].clone());
            let _closed = websocket.next().await;
        }
        Ok::<_, anyhow::Error>(requests)
    });
    for _attempt in 0..2 {
        let output = tokio::process::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
            .env("CODEX_HOME", home.path())
            .args(["--remote", &endpoint, "project", "status", "--json"])
            .output()
            .await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout)?,
            json!({"capability": {"version": 1}, "project": null})
        );
    }
    assert_eq!(
        server.await??,
        vec![
            json!({"projectId": null, "threadId": null, "expectedRevision": null, "command": null});
            2
        ]
    );
    Ok(())
}

#[tokio::test]
async fn project_automation_cli_never_starts_a_short_lived_local_server() -> Result<()> {
    let home = TempDir::new()?;
    let socket = home.path().join("missing.sock");
    let endpoint = format!("unix://{}", socket.display());
    let output = tokio::process::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .env("CODEX_HOME", home.path())
        .args(["--remote", &endpoint, "project", "status"])
        .output()
        .await?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("persistent app server"));
    assert!(!socket.exists());
    Ok(())
}

#[tokio::test]
async fn project_automation_cli_reports_retired_mutations() -> Result<()> {
    let home = TempDir::new()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut websocket = accept_async(stream).await?;
        let initialize = websocket.next().await.context("missing initialize")??;
        let initialize: Value = serde_json::from_str(initialize.to_text()?)?;
        websocket
            .send(Message::Text(
                json!({"id": initialize["id"], "result": {
                    "userAgent": "codex_cli_rs/0.0.0-test"
                }})
                .to_string()
                .into(),
            ))
            .await?;
        let _initialized = websocket.next().await.context("missing initialized")??;
        let request = websocket
            .next()
            .await
            .context("missing project request")??;
        let request: Value = serde_json::from_str(request.to_text()?)?;
        assert_eq!(request["method"], "projectAutomation/command");
        websocket
            .send(Message::Text(
                json!({"id": request["id"], "error": {
                    "code": -32600,
                    "message": "project clocks are retired; use generic alarms and Coordinator review operations"
                }})
                .to_string()
                .into(),
            ))
            .await?;
        Ok::<_, anyhow::Error>(())
    });
    let output = tokio::process::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .env("CODEX_HOME", home.path())
        .args([
            "--remote",
            &endpoint,
            "project",
            "bind",
            "analysis",
            "--thread",
            "thread-id",
            "--workstream",
            "cli",
        ])
        .output()
        .await?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("project clocks are retired"));
    server.await??;
    Ok(())
}
