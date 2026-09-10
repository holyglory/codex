use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol as api;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
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

async fn start_project_server(
    home: &Path,
) -> Result<(tokio::process::Child, tokio::task::JoinHandle<()>, String)> {
    let mut process = tokio::process::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .args(["app-server", "--listen", "ws://127.0.0.1:0"])
        .env("CODEX_HOME", home)
        .env("CODEX_APP_SERVER_DISABLE_MANAGED_CONFIG", "1")
        .env("RUST_LOG", "warn")
        .env("NO_COLOR", "1")
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stderr = BufReader::new(process.stderr.take().context("server stderr")?).lines();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(/*secs*/ 30);
    let address = loop {
        let line = tokio::time::timeout_at(deadline, stderr.next_line())
            .await??
            .context("server exited before listening")?;
        eprintln!("[project daemon] {line}");
        if let Some(address) = line
            .split_whitespace()
            .find_map(|token| token.strip_prefix("ws://"))
            .and_then(|address| address.parse::<SocketAddr>().ok())
        {
            break address;
        }
    };
    let logs = tokio::spawn(async move {
        while let Ok(Some(line)) = stderr.next_line().await {
            eprintln!("[project daemon] {line}");
        }
    });
    Ok((process, logs, format!("ws://{address}")))
}

async fn run_project_cli(home: &Path, endpoint: &str, args: &[&str]) -> Result<Output> {
    Ok(
        tokio::process::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
            .env("CODEX_HOME", home)
            .current_dir(home)
            .args(["--remote", endpoint, "project"])
            .args(args)
            .output()
            .await?,
    )
}

fn project_state(output: Output) -> Result<api::ProjectAutomation> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.len() <= 16 * 1024);
    let response: api::ProjectAutomationCommandResponse = serde_json::from_slice(&output.stdout)?;
    response.project.context("project state missing")
}

#[tokio::test]
async fn project_automation_cli_link_work_mutates_and_survives_server_restart() -> Result<()> {
    let model =
        create_mock_responses_server_sequence(vec![create_final_assistant_message_sse_response(
            "Specification ready.",
        )?])
        .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&model.uri()).write(home.path())?;
    let (mut process, logs, endpoint) = start_project_server(home.path()).await?;
    let mut client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: endpoint.clone(),
            auth_token: None,
        },
        client_name: "project-cli-integration".into(),
        client_version: "0.1.0".into(),
        experimental_api: false,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 32,
    })
    .await?;
    let started: api::ThreadStartResponse = client
        .request_typed(api::ClientRequest::ThreadStart {
            request_id: api::RequestId::Integer(1),
            params: api::ThreadStartParams {
                cwd: Some(home.path().to_string_lossy().into_owned()),
                ..Default::default()
            },
        })
        .await?;
    let thread = started.thread.id;
    let _: api::TurnStartResponse = client
        .request_typed(api::ClientRequest::TurnStart {
            request_id: api::RequestId::Integer(2),
            params: api::TurnStartParams {
                thread_id: thread.clone(),
                input: vec![api::UserInput::Text {
                    text: "Prepare a specification for this fixture.".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(/*secs*/ 30), async {
        loop {
            match client
                .next_event()
                .await
                .context("server disconnected during setup")?
            {
                AppServerEvent::ServerNotification(notification) => {
                    if let api::ServerNotification::TurnCompleted(completed) = *notification
                        && completed.thread_id == thread
                    {
                        assert_eq!(completed.turn.status, api::TurnStatus::Completed);
                        return Ok::<_, anyhow::Error>(());
                    }
                }
                AppServerEvent::Disconnected { message } => anyhow::bail!("{message}"),
                AppServerEvent::Lagged { .. } | AppServerEvent::ServerRequest(_) => {
                    anyhow::bail!("unexpected setup event")
                }
            }
        }
    })
    .await??;
    client.shutdown().await?;
    let mut state = project_state(
        run_project_cli(
            home.path(),
            &endpoint,
            &[
                "bind",
                "analysis",
                "--thread",
                &thread,
                "--workstream",
                "cli",
                "--json",
            ],
        )
        .await?,
    )?;
    state = project_state(
        run_project_cli(
            home.path(),
            &endpoint,
            &[
                "link-work",
                "--thread",
                &thread,
                "--expected-revision",
                &state.revision.to_string(),
                "--outcome-id",
                "outcome-context",
                "--experiment-ref",
                "review-context@1",
                "--json",
            ],
        )
        .await?,
    )?;
    assert_eq!(
        (
            state.thread_outcomes.get(&thread).map(String::as_str),
            state.thread_experiments.get(&thread).map(String::as_str)
        ),
        (Some("outcome-context"), Some("review-context@1"))
    );
    let output = run_project_cli(
        home.path(),
        &endpoint,
        &[
            "link-work",
            "--thread",
            &thread,
            "--expected-revision",
            &state.revision.to_string(),
            "--experiment-ref",
            "review-context@2",
        ],
    )
    .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout)?;
    assert!(text.contains("outcome: \"outcome-context\" | experiment: \"review-context@2\""));
    state = project_state(
        run_project_cli(
            home.path(),
            &endpoint,
            &["status", "--thread", &thread, "--json"],
        )
        .await?,
    )?;
    assert_eq!(
        (
            state.thread_outcomes.get(&thread).map(String::as_str),
            state.thread_experiments.get(&thread).map(String::as_str)
        ),
        (Some("outcome-context"), Some("review-context@2"))
    );
    process.kill().await?;
    logs.await?;
    let (mut process, logs, endpoint) = start_project_server(home.path()).await?;
    assert_eq!(
        project_state(
            run_project_cli(
                home.path(),
                &endpoint,
                &["status", "--thread", &thread, "--json"]
            )
            .await?
        )?,
        state
    );
    state = project_state(
        run_project_cli(
            home.path(),
            &endpoint,
            &[
                "link-work",
                "--thread",
                &thread,
                "--expected-revision",
                &state.revision.to_string(),
                "--clear-outcome",
                "--json",
            ],
        )
        .await?,
    )?;
    assert_eq!(
        (
            state.thread_outcomes.get(&thread).map(String::as_str),
            state.thread_experiments.get(&thread).map(String::as_str)
        ),
        (None, Some("review-context@2"))
    );
    state = project_state(
        run_project_cli(
            home.path(),
            &endpoint,
            &[
                "link-work",
                "--thread",
                &thread,
                "--expected-revision",
                &state.revision.to_string(),
                "--outcome-id",
                "outcome-next",
                "--clear-experiment",
                "--json",
            ],
        )
        .await?,
    )?;
    assert_eq!(
        (
            state.thread_outcomes.get(&thread).map(String::as_str),
            state.thread_experiments.get(&thread).map(String::as_str)
        ),
        (Some("outcome-next"), None)
    );
    for flags in [
        vec![],
        vec!["--outcome-id", "outcome-next", "--clear-outcome"],
        vec!["--experiment-ref", "review-context@3", "--clear-experiment"],
    ] {
        let revision = state.revision.to_string();
        let mut args = vec![
            "link-work",
            "--thread",
            &thread,
            "--expected-revision",
            &revision,
            "--json",
        ];
        args.extend(flags);
        let output = run_project_cli(home.path(), &endpoint, &args).await?;
        assert_eq!(output.status.code(), Some(2));
    }
    assert_eq!(
        project_state(
            run_project_cli(
                home.path(),
                &endpoint,
                &["status", "--thread", &thread, "--json"]
            )
            .await?
        )?,
        state
    );
    state = project_state(
        run_project_cli(
            home.path(),
            &endpoint,
            &[
                "link-work",
                "--thread",
                &thread,
                "--expected-revision",
                &state.revision.to_string(),
                "--clear-outcome",
                "--clear-experiment",
                "--json",
            ],
        )
        .await?,
    )?;
    assert!(state.thread_outcomes.is_empty() && state.thread_experiments.is_empty());
    process.kill().await?;
    logs.await?;
    Ok(())
}
