use super::*;

#[tokio::test]
async fn managed_maintenance_cancel_reopens_admission_and_commit_restores_threads() -> Result<()> {
    use codex_app_server_transport::maintenance::MaintenanceCommand;
    use codex_app_server_transport::maintenance::MaintenanceResponse;
    let home = TempDir::new()?;
    let (mock, _) = start_streaming_sse_server(vec![]).await;
    create_config_toml(home.path(), mock.uri(), "never")?;
    let socket_path = home.path().join("control/server.sock");
    let mut server = spawn_server(home.path(), &socket_path)?;
    let mut client = connect_default_daemon_client(&socket_path).await?;
    let thread = start_thread(&mut client, /*id*/ 2, json!({})).await?;
    for (operation_id, commit) in [("cancelled", false), ("committed", true)] {
        let pid = server.id().context("server pid")?;
        let stream = UnixStream::connect(&socket_path).await?;
        let (mut maintenance, _) =
            client_async("ws://localhost/daemon/maintenance", stream).await?;
        maintenance
            .send(Message::Text(
                serde_json::to_string(&MaintenanceCommand::Prepare {
                    operation_id: operation_id.into(),
                    pid,
                })?
                .into(),
            ))
            .await?;
        let frame = timeout(DEFAULT_READ_TIMEOUT, maintenance.next())
            .await?
            .context("maintenance reply")??;
        let Message::Text(text) = frame else {
            anyhow::bail!("expected readiness receipt")
        };
        assert_eq!(
            serde_json::from_str::<MaintenanceResponse>(&text)?,
            MaintenanceResponse::Ready {
                operation_id: operation_id.into(),
                pid
            }
        );
        // Preparation keeps fresh sessions available; commit must also capture
        // runtimes created after the initial readiness receipt.
        let added = start_thread(&mut client, /*id*/ 20, json!({})).await?;
        assert!(!added.thread.id.is_empty());
        let command = if commit {
            MaintenanceCommand::Commit {
                operation_id: operation_id.into(),
                pid,
            }
        } else {
            MaintenanceCommand::Cancel
        };
        maintenance
            .send(Message::Text(serde_json::to_string(&command)?.into()))
            .await?;
        if commit {
            wait_success(&mut server).await?;
        } else {
            maintenance.close(None).await?;
            timeout(DEFAULT_READ_TIMEOUT, async {
                loop {
                    if start_thread(&mut client, /*id*/ 3, json!({})).await.is_ok() {
                        break;
                    }
                    sleep(Duration::from_millis(25)).await;
                }
            })
            .await?;
            assert!(server.try_wait()?.is_none());
            assert!(!daemon_recovery_file_path(home.path()).exists());
        }
    }
    let saved = daemon_recovery::read_snapshot(&daemon_recovery_file_path(home.path()))?;
    assert!(saved.loaded.contains(&thread.thread.id));
    assert_eq!(
        saved
            .maintenance
            .context("maintenance metadata")?
            .operation_id,
        "committed"
    );
    let mut successor = spawn_server(home.path(), &socket_path)?;
    let mut client = connect_default_daemon_client(&socket_path).await?;
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            if request(
                &mut client,
                /*id*/ 4,
                "thread/resume",
                json!({"threadId":thread.thread.id}),
            )
            .await
            .is_ok()
            {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await?;
    start_thread(&mut client, /*id*/ 5, json!({})).await?;
    request_shutdown(&successor, &socket_path).await?;
    wait_success(&mut successor).await?;
    Ok(())
}

async fn maintenance_status(socket_path: &Path) -> Result<serde_json::Value> {
    use codex_app_server_transport::maintenance::MaintenanceCommand;
    let stream = UnixStream::connect(socket_path).await?;
    let (mut socket, _) = client_async("ws://localhost/daemon/maintenance", stream).await?;
    socket.send(Message::Text(serde_json::to_string(&MaintenanceCommand::Status)?.into())).await?;
    let frame = timeout(DEFAULT_READ_TIMEOUT, socket.next()).await?.context("status reply")??;
    let Message::Text(text) = frame else { anyhow::bail!("expected status") };
    socket.close(None).await?;
    Ok(serde_json::from_str(&text)?)
}

#[tokio::test]
async fn maintenance_restores_active_parent_and_child_without_replaying_tools() -> Result<()> {
    use codex_app_server_transport::maintenance::{MaintenanceCommand, MaintenanceResponse};
    use core_test_support::responses;
    let home = TempDir::new()?;
    let (release_a, gate_a) = oneshot::channel();
    let (release_b, gate_b) = oneshot::channel();
    let (release_c, gate_c) = oneshot::channel();
    let (release_d, gate_d) = oneshot::channel();
    let tool_step = |call: &str, gate| StreamingSseChunk {
        gate: Some(gate),
        body: responses::sse(vec![
            responses::ev_response_created(call),
            responses::ev_function_call(call, "exec_command", &json!({"cmd":"echo checkpoint-result","yield_time_ms":5000,"max_output_tokens":200}).to_string()),
            responses::ev_completed(call),
        ]),
    };
    let (mock, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk { gate: None, body: responses::sse(vec![
            responses::ev_response_created("spawn"),
            responses::ev_function_call_with_namespace("spawn-worker", "collaboration", "spawn_agent",
                &json!({"task_name":"worker","message":"Do the child work","fork_turns":"none"}).to_string()),
            responses::ev_completed("spawn"),
        ]) }],
        vec![tool_step("checkpoint-a", gate_a)],
        vec![tool_step("checkpoint-b", gate_b)],
        vec![stream_chunk(Some(gate_c), "Restored first")?],
        vec![stream_chunk(Some(gate_d), "Restored second")?],
    ]).await;
    create_config_toml(home.path(), mock.uri(), "never")?;
    let mut config = std::fs::read_to_string(home.path().join("config.toml"))?;
    config.push_str("\n[features]\nmulti_agent = true\ncode_mode = false\ncode_mode_only = false\n[features.multi_agent_v2]\nenabled = true\n");
    std::fs::write(home.path().join("config.toml"), config)?;
    let socket_path = home.path().join("control/server.sock");
    let mut server = spawn_server(home.path(), &socket_path)?;
    let mut client = connect_default_daemon_client(&socket_path).await?;
    let parent = start_thread(&mut client, /*id*/ 2, json!({})).await?;
    start_turn(&mut client, /*id*/ 3, &parent.thread.id).await?;
    wait_for_requests(&mock, /*count*/ 3).await?;
    let stream = UnixStream::connect(&socket_path).await?;
    let (mut control, _) = client_async("ws://localhost/daemon/maintenance", stream).await?;
    let pid = server.id().context("pid")?;
    control.send(Message::Text(serde_json::to_string(&MaintenanceCommand::Prepare { operation_id:"agent-tree".into(), pid })?.into())).await?;
    timeout(DEFAULT_READ_TIMEOUT, async {
        while maintenance_status(&socket_path).await?["preparing"] != true {
            sleep(Duration::from_millis(10)).await;
        }
        Ok::<(), anyhow::Error>(())
    }).await??;
    release_a.send(()).unwrap();
    release_b.send(()).unwrap();
    let frame = timeout(Duration::from_secs(30), control.next()).await?.context("prepare reply")??;
    let Message::Text(text) = frame else { anyhow::bail!("expected checkpoint receipt") };
    assert_eq!(serde_json::from_str::<MaintenanceResponse>(&text)?, MaintenanceResponse::Ready { operation_id:"agent-tree".into(), pid });
    assert_eq!(mock.requests().await.len(), 3);
    control.send(Message::Text(serde_json::to_string(&MaintenanceCommand::Commit { operation_id:"agent-tree".into(), pid })?.into())).await?;
    wait_success(&mut server).await?;
    let saved = daemon_recovery::read_snapshot(&daemon_recovery_file_path(home.path()))?;
    assert_eq!(saved.interrupted.len(), 2);
    let parents = &saved.maintenance.as_ref().context("tree metadata")?.parents;
    assert_eq!(parents.get(&parent.thread.id), Some(&None));
    assert_eq!(parents.values().filter(|parent_id| parent_id.as_deref() == Some(parent.thread.id.as_str())).count(), 1);
    let mut successor = spawn_server(home.path(), &socket_path)?;
    wait_for_requests(&mock, /*count*/ 5).await?;
    let mut client = connect_default_daemon_client(&socket_path).await?;
    for (id, interrupted) in &saved.interrupted {
        let read = request(&mut client, /*id*/ 4, "thread/read", json!({"threadId":id,"includeTurns":true})).await?;
        let turns = read["thread"]["turns"].as_array().context("restored turns")?;
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["id"], interrupted.turn_id);
        assert_eq!(turns[0]["status"], "inProgress");
    }
    let requests = mock.requests().await;
    let mut restored_calls = std::collections::BTreeSet::new();
    for body in &requests[3..] {
        let body: serde_json::Value = serde_json::from_slice(body)?;
        let outputs: Vec<_> = body["input"].as_array().context("input")?.iter()
            .filter(|item| item["type"] == "function_call_output" && item["call_id"].as_str().is_some_and(|id| id.starts_with("checkpoint-")))
            .collect();
        assert_eq!(outputs.len(), 1);
        let output = outputs[0]["output"].to_string();
        assert!(output.contains("checkpoint-result") && output.contains("Process exited with code 0"), "{output}");
        restored_calls.insert(outputs[0]["call_id"].as_str().unwrap().to_string());
    }
    assert_eq!(restored_calls, ["checkpoint-a".to_string(), "checkpoint-b".to_string()].into());
    let first: serde_json::Value = serde_json::from_slice(&requests[3])?;
    let parent_first = first["input"].as_array().context("input")?.iter().any(|item| item["call_id"] == "spawn-worker");
    let (release_parent, release_child) = if parent_first { (release_c, release_d) } else { (release_d, release_c) };
    release_parent.send(()).unwrap();
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let read = request(&mut client, /*id*/ 8, "thread/read", json!({"threadId":parent.thread.id,"includeTurns":true})).await?;
            if read["thread"]["turns"][0]["status"] == "completed" { break; }
            sleep(Duration::from_millis(10)).await;
        }
        Ok::<(), anyhow::Error>(())
    }).await??;
    release_child.send(()).unwrap();
    request_shutdown(&successor, &socket_path).await?;
    wait_success(&mut successor).await?;
    assert_eq!(mock.requests().await.len(), 5);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn maintenance_blocked_snapshot_reopens_server_without_killing_work() -> Result<()> {
    use codex_app_server_transport::maintenance::{MaintenanceCommand, MaintenanceResponse};
    let home = TempDir::new()?;
    let (mock, _) = start_streaming_sse_server(vec![]).await;
    create_config_toml(home.path(), mock.uri(), "never")?;
    let socket_path = home.path().join("control/server.sock");
    let mut server = spawn_server(home.path(), &socket_path)?;
    let mut client = connect_default_daemon_client(&socket_path).await?;
    let thread = start_thread(&mut client, /*id*/ 2, json!({})).await?;
    let rollout = thread.thread.path.context("deferred rollout")?;
    assert!(!rollout.exists());
    let compressed = rollout.with_extension("jsonl.zst");
    std::fs::create_dir_all(compressed.parent().context("rollout parent")?)?;
    assert!(StdCommand::new("mkfifo").arg(&compressed).status()?.success());
    let stream = UnixStream::connect(&socket_path).await?;
    let (mut control, _) = client_async("ws://localhost/daemon/maintenance", stream).await?;
    let pid = server.id().context("pid")?;
    control.send(Message::Text(serde_json::to_string(&MaintenanceCommand::Prepare { operation_id:"blocked-writer".into(), pid })?.into())).await?;
    let frame = timeout(DEFAULT_READ_TIMEOUT, control.next()).await?.context("prepare reply")??;
    let Message::Text(text) = frame else { anyhow::bail!("expected readiness") };
    assert_eq!(serde_json::from_str::<MaintenanceResponse>(&text)?, MaintenanceResponse::Ready { operation_id:"blocked-writer".into(), pid });
    control.send(Message::Text(serde_json::to_string(&MaintenanceCommand::Commit { operation_id:"blocked-writer".into(), pid })?.into())).await?;
    let writer = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            if let Ok(writer) = tokio::net::unix::pipe::OpenOptions::new().open_sender(&compressed) { break writer; }
            sleep(Duration::from_millis(25)).await;
        }
    }).await.context("checkpoint did not reach the real blocked writer")?;
    timeout(Duration::from_secs(30), async {
        loop {
            let state = maintenance_status(&socket_path).await?;
            if state["preparing"] == false && state["accepting"] == true { break; }
            sleep(Duration::from_millis(25)).await;
        }
        Ok::<(), anyhow::Error>(())
    }).await??;
    assert!(server.try_wait()?.is_none());
    start_thread(&mut client, /*id*/ 3, json!({})).await?;
    assert!(!daemon_recovery_file_path(home.path()).exists());
    drop(writer);
    std::fs::remove_file(compressed)?;
    request_shutdown(&server, &socket_path).await?;
    wait_success(&mut server).await?;
    Ok(())
}
