//! Preparation refuses process-local work in active and idle conversations.
use super::*;
use core_test_support::responses;
use pretty_assertions::assert_eq;

#[derive(Clone, Copy)]
enum BackgroundKind {
    EphemeralTerminal,
    StopHook,
    CodeModeCell,
}

#[cfg(unix)]
#[test_case::test_case(BackgroundKind::EphemeralTerminal; "idle_ephemeral_terminal")]
#[test_case::test_case(BackgroundKind::StopHook; "idle_async_stop_hook")]
#[test_case::test_case(BackgroundKind::CodeModeCell; "active_ephemeral_code_mode_cell")]
#[tokio::test]
async fn maintenance_rejects_process_local_background_work(kind: BackgroundKind) -> Result<()> {
    use codex_app_server_transport::maintenance::MaintenanceCommand;
    use codex_app_server_transport::maintenance::MaintenanceResponse;
    let home = TempDir::new()?;
    let entered = home.path().join("background-entered");
    let release = home.path().join("release-background");
    let effect = home.path().join("background-survived");
    let command = format!(
        "echo ready > '{}'; while [ ! -f '{}' ]; do sleep 0.01; done; echo survived > '{}'",
        entered.display(),
        release.display(),
        effect.display()
    );
    let terminal = matches!(kind, BackgroundKind::EphemeralTerminal);
    let code_cell = matches!(kind, BackgroundKind::CodeModeCell);
    let (release_cell, cell_gate) = oneshot::channel();
    let (release_turn, turn_gate) = oneshot::channel();
    let mut chunks = Vec::new();
    if terminal {
        chunks.push(vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("background"),
                responses::ev_function_call(
                    "background",
                    "exec_command",
                    &json!({"cmd":command,"yield_time_ms":1,"max_output_tokens":200}).to_string(),
                ),
                responses::ev_completed("background"),
            ]),
        }]);
    } else if code_cell {
        chunks.push(vec![StreamingSseChunk {
            gate: Some(cell_gate),
            body: responses::sse(vec![
                responses::ev_response_created("background"),
                responses::ev_custom_tool_call(
                    "background",
                    "exec",
                    "// @exec: {\"yield_time_ms\": 1}\nconst value = await tools.cell_probe({phase: 'hold'}); await tools.cell_probe({phase: 'done', value}); text(value);",
                ),
                responses::ev_completed("background"),
            ]),
        }]);
    } else {
        std::fs::write(home.path().join("hooks.json"), json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":command,"async":true}]}]}}).to_string())?;
    }
    if !code_cell {
        chunks.push(vec![stream_chunk(
            /*gate*/ None,
            "Turn completed while work remains",
        )?]);
    }
    let (mock, _) = start_streaming_sse_server(chunks).await;
    let wait_response = if code_cell {
        let wait_response = mock.defer_next_response().await;
        mock.defer_next_response()
            .await
            .send(vec![stream_chunk(
                Some(turn_gate),
                "Cell output collected",
            )?])
            .map_err(|_| anyhow::anyhow!("final cell response receiver closed"))?;
        Some(wait_response)
    } else {
        None
    };
    create_config_toml(home.path(), mock.uri(), "never")?;
    let config_path = home.path().join("config.toml");
    let mut config = std::fs::read_to_string(&config_path)?.replace(
        "sandbox_mode = \"read-only\"",
        "sandbox_mode = \"danger-full-access\"",
    );
    config.push_str(&format!(
        "\n[features]\nhooks = true\ncode_mode = {code_cell}\ncode_mode_only = {code_cell}\n"
    ));
    std::fs::write(config_path, config)?;
    let socket_path = home.path().join("control/server.sock");
    let mut server = spawn_server(home.path(), &socket_path)?;
    let mut client = connect_daemon_client(
        &socket_path,
        InitializeCapabilities {
            experimental_api: true,
            ..Default::default()
        },
    )
    .await?;
    if matches!(kind, BackgroundKind::StopHook) {
        trust_fixture_hooks(&mut client, home.path()).await?;
    }
    let mut params = json!({"ephemeral":terminal || code_cell,"cwd":home.path()});
    if code_cell {
        params["dynamicTools"] = json!([{
            "type": "function",
            "name": "cell_probe",
            "description": "Hold a background cell and report its result",
            "inputSchema": {
                "type": "object",
                "properties": {"phase": {"type": "string"}, "value": {}},
                "required": ["phase"]
            }
        }]);
    }
    let thread = start_thread(&mut client, /*id*/ 2, params).await?;
    start_turn(&mut client, /*id*/ 3, &thread.thread.id).await?;
    let pending_cell = if code_cell {
        release_cell.send(()).expect("cell request gate");
        let (id, arguments) = read_cell_probe(&mut client)
            .await
            .context("first cell probe")?;
        assert_eq!(arguments, json!({"phase": "hold"}));
        Some(id)
    } else {
        None
    };
    timeout(DEFAULT_READ_TIMEOUT, async {
        if code_cell {
            return Ok::<_, anyhow::Error>(());
        }
        loop {
            let read = request(
                &mut client,
                /*id*/ 4,
                "thread/read",
                json!({"threadId":thread.thread.id,"includeTurns":false}),
            )
            .await;
            if (pending_cell.is_some() || entered.exists())
                && read.is_ok_and(|read| read["thread"]["status"]["type"] == "idle")
            {
                break;
            }
            sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let (mut control, _) = client_async(
        "ws://localhost/daemon/maintenance",
        UnixStream::connect(&socket_path).await?,
    )
    .await?;
    control
        .send(Message::Text(
            serde_json::to_string(&MaintenanceCommand::Prepare {
                operation_id: "background-owner".into(),
                pid: server.id().context("server pid")?,
            })?
            .into(),
        ))
        .await?;
    let frame = timeout(DEFAULT_READ_TIMEOUT, control.next())
        .await?
        .context("prepare response")??;
    let Message::Text(text) = frame else {
        anyhow::bail!("expected prepare response");
    };
    assert_eq!(
        serde_json::from_str::<MaintenanceResponse>(&text)?,
        MaintenanceResponse::Failed {
            reason: "backgroundWork".into()
        }
    );
    assert!(server.try_wait()?.is_none());
    start_thread(&mut client, /*id*/ 5, json!({})).await?;
    if let Some(id) = pending_cell {
        client.send(Message::Text(json!({
            "id": id,
            "result": {"contentItems": [{"type": "inputText", "text": "survived"}], "success": true}
        }).to_string().into())).await?;
        let (id, arguments) = read_cell_probe(&mut client)
            .await
            .context("released cell result")?;
        assert_eq!(arguments["phase"], "done");
        assert!(arguments["value"].to_string().contains("survived"));
        client
            .send(Message::Text(
                json!({
                    "id": id,
                    "result": {"contentItems": [], "success": true}
                })
                .to_string()
                .into(),
            ))
            .await?;
        wait_for_requests(&mock, /*count*/ 2).await?;
        let requests = mock.requests().await;
        let request: serde_json::Value = serde_json::from_slice(&requests[1])?;
        let output = request["input"]
            .as_array()
            .context("cell input")?
            .iter()
            .find(|item| {
                item["call_id"] == "background" && item["type"] == "custom_tool_call_output"
            })
            .context("yielded cell output")?;
        let body: codex_protocol::models::FunctionCallOutputBody =
            serde_json::from_value(output["output"].clone())?;
        let text = body.to_text().context("cell output text")?;
        let cell_id = text
            .split_once("Script running with cell ID ")
            .and_then(|(_, tail)| tail.split_whitespace().next())
            .context("returned cell identity")?;
        wait_response
            .context("deferred wait response")?
            .send(vec![StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![
                    responses::ev_response_created("collect-cell"),
                    responses::ev_function_call(
                        "collect-cell",
                        "wait",
                        &json!({"cell_id":cell_id,"yield_time_ms":10000}).to_string(),
                    ),
                    responses::ev_completed("collect-cell"),
                ]),
            }])
            .map_err(|_| anyhow::anyhow!("wait response receiver closed"))?;
        wait_for_requests(&mock, /*count*/ 3).await?;
        let requests = mock.requests().await;
        let request: serde_json::Value = serde_json::from_slice(&requests[2])?;
        let result = request["input"]
            .as_array()
            .context("collected cell input")?
            .iter()
            .find(|item| {
                item["call_id"] == "collect-cell" && item["type"] == "function_call_output"
            })
            .context("collected cell output")?;
        assert!(result["output"].to_string().contains("survived"));
        // Keep the owning turn alive until its cell has consumed the reply and
        // finished. The ordinary ephemeral-turn rejection then replaces the
        // background-work rejection, proving the cell guard clears correctly.
        timeout(DEFAULT_READ_TIMEOUT, async {
            loop {
                let (mut completed_control, _) = client_async(
                    "ws://localhost/daemon/maintenance",
                    UnixStream::connect(&socket_path).await?,
                )
                .await?;
                completed_control
                    .send(Message::Text(
                        serde_json::to_string(&MaintenanceCommand::Prepare {
                            operation_id: "completed-cell".into(),
                            pid: server.id().context("server pid")?,
                        })?
                        .into(),
                    ))
                    .await?;
                let frame = completed_control
                    .next()
                    .await
                    .context("cell completion receipt")??;
                let Message::Text(text) = frame else {
                    anyhow::bail!("expected cell completion receipt");
                };
                let response = serde_json::from_str::<MaintenanceResponse>(&text)?;
                if response
                    != (MaintenanceResponse::Failed {
                        reason: "backgroundWork".into(),
                    })
                {
                    assert_eq!(
                        response,
                        MaintenanceResponse::Failed {
                            reason: "nonpersistentWork".into()
                        }
                    );
                    break;
                }
                sleep(Duration::from_millis(/*millis*/ 10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("completed cell still blocks maintenance")??;
        release_turn.send(()).expect("final response gate");
        timeout(DEFAULT_READ_TIMEOUT, async {
            loop {
                let frame = client
                    .next()
                    .await
                    .context("cell turn connection closed")??;
                let Message::Text(text) = frame else { continue };
                if let JSONRPCMessage::Notification(notification) = serde_json::from_str(&text)?
                    && notification.method == "turn/completed"
                    && let Some(params) = notification.params
                    && params["threadId"] == thread.thread.id
                {
                    assert_eq!(params["turn"]["status"], "completed");
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("owning turn completion")??;
    } else {
        std::fs::write(release, "continue")?;
        timeout(DEFAULT_READ_TIMEOUT, async {
            while !effect.exists() {
                sleep(Duration::from_millis(/*millis*/ 10)).await;
            }
        })
        .await?;
        assert_eq!(std::fs::read_to_string(effect)?.trim(), "survived");
    }
    request_shutdown(&server, &socket_path).await?;
    wait_success(&mut server).await?;
    Ok(())
}

async fn read_cell_probe(
    client: &mut WebSocketStream<UnixStream>,
) -> Result<(RequestId, serde_json::Value)> {
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let frame = client.next().await.context("cell connection closed")??;
            let Message::Text(text) = frame else { continue };
            if let JSONRPCMessage::Request(request) = serde_json::from_str(&text)?
                && request.method == "item/tool/call"
            {
                let params = request.params.context("dynamic tool parameters")?;
                assert_eq!(params["tool"], "cell_probe");
                return Ok((request.id, params["arguments"].clone()));
            }
        }
    })
    .await?
}

pub(super) async fn trust_fixture_hooks(
    client: &mut WebSocketStream<UnixStream>,
    home: &Path,
) -> Result<()> {
    let listed = request(client, /*id*/ 90, "hooks/list", json!({"cwds":[home]})).await?;
    let mut trusted = serde_json::Map::new();
    for entry in listed["data"].as_array().context("hook entries")? {
        for hook in entry["hooks"].as_array().context("configured hooks")? {
            let key = hook["key"].as_str().context("hook key")?;
            if key.starts_with(home.to_str().context("fixture path")?) {
                trusted.insert(key.to_string(), json!({"trusted_hash":hook["currentHash"]}));
            }
        }
    }
    anyhow::ensure!(!trusted.is_empty(), "fixture hook was not discovered");
    request(client, /*id*/ 91, "config/batchWrite", json!({"edits":[{"keyPath":"hooks.state","value":trusted,"mergeStrategy":"upsert"}],"reloadUserConfig":true})).await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn maintenance_rejects_sealing_while_interrupt_cleanup_is_running() -> Result<()> {
    use codex_app_server_transport::maintenance::MaintenanceCommand;
    let home = TempDir::new()?;
    let entered = home.path().join("interrupt-entered");
    let release = home.path().join("release-interrupt");
    let finished = home.path().join("interrupt-finished");
    let command = format!(
        "echo ready > '{}'; while [ ! -f '{}' ]; do sleep 0.01; done; echo survived > '{}'",
        entered.display(),
        release.display(),
        finished.display()
    );
    std::fs::write(
        home.path().join("hooks.json"),
        json!({"hooks":{"Interrupt":[{"hooks":[{"type":"command","command":command}]}]}})
            .to_string(),
    )?;
    let (_release_model, gate) = oneshot::channel();
    let (mock, _) =
        start_streaming_sse_server(vec![vec![stream_chunk(Some(gate), "not reached")?]]).await;
    create_config_toml(home.path(), mock.uri(), "never")?;
    let config_path = home.path().join("config.toml");
    let mut config = std::fs::read_to_string(&config_path)?;
    config.push_str("\n[features]\nhooks = true\n");
    std::fs::write(config_path, config)?;
    let socket_path = home.path().join("control/server.sock");
    let mut server = spawn_server(home.path(), &socket_path)?;
    let mut client = connect_daemon_client(
        &socket_path,
        InitializeCapabilities {
            experimental_api: true,
            ..Default::default()
        },
    )
    .await?;
    trust_fixture_hooks(&mut client, home.path()).await?;
    let thread = start_thread(&mut client, /*id*/ 2, json!({"cwd":home.path()})).await?;
    let turn = request(
        &mut client,
        /*id*/ 3,
        "turn/start",
        json!({"threadId":thread.thread.id,"input":[{"type":"text","text":"hold this turn"}]}),
    )
    .await?;
    wait_for_requests(&mock, /*count*/ 1).await?;
    let mut stop = connect_default_daemon_client(&socket_path).await?;
    stop.send(Message::Text(json!({"id":44,"method":"turn/interrupt","params":{"threadId":thread.thread.id,"turnId":turn["turn"]["id"]}}).to_string().into())).await?;
    timeout(DEFAULT_READ_TIMEOUT, async {
        while !entered.exists() {
            sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await?;
    let (mut control, _) = client_async(
        "ws://localhost/daemon/maintenance",
        UnixStream::connect(&socket_path).await?,
    )
    .await?;
    control
        .send(Message::Text(
            serde_json::to_string(&MaintenanceCommand::Prepare {
                operation_id: "interrupt-cleanup".into(),
                pid: server.id().context("server pid")?,
            })?
            .into(),
        ))
        .await?;
    assert!(
        timeout(Duration::from_millis(/*millis*/ 200), control.next())
            .await
            .is_err(),
        "a running Interrupt hook cannot receive Ready"
    );
    start_thread(&mut client, /*id*/ 5, json!({})).await?;
    control.close(None).await?;
    std::fs::write(release, "continue")?;
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let frame = stop.next().await.context("Stop connection closed")??;
            let Message::Text(text) = frame else {
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&text)?;
            if value["id"] == 44 {
                anyhow::ensure!(value.get("error").is_none(), "Stop failed: {value}");
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert_eq!(std::fs::read_to_string(finished)?.trim(), "survived");
    assert!(server.try_wait()?.is_none());
    assert!(!daemon_recovery_file_path(home.path()).exists());
    request_shutdown(&server, &socket_path).await?;
    wait_success(&mut server).await?;
    Ok(())
}
