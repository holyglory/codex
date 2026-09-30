use super::*;
use pretty_assertions::assert_eq;

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
    let mut expected = std::collections::BTreeSet::from([thread.thread.id.clone()]);
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
        expected.insert(added.thread.id);
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
            let receipt = timeout(DEFAULT_READ_TIMEOUT, maintenance.next())
                .await?
                .context("commit receipt")??;
            let Message::Text(receipt) = receipt else {
                anyhow::bail!("expected commit receipt");
            };
            assert_eq!(
                serde_json::from_str::<MaintenanceResponse>(&receipt)?,
                MaintenanceResponse::Committed {
                    operation_id: operation_id.into(),
                    pid
                }
            );
            wait_success(&mut server).await?;
        } else {
            let _ = maintenance.close(None).await;
            timeout(DEFAULT_READ_TIMEOUT, async {
                loop {
                    if let Ok(added) = start_thread(&mut client, /*id*/ 3, json!({})).await {
                        expected.insert(added.thread.id);
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
    assert_eq!(saved.loaded, expected);
    assert_eq!(
        saved
            .maintenance
            .context("maintenance metadata")?
            .operation_id,
        "committed"
    );
    let mut successor = spawn_server(home.path(), &socket_path)?;
    let mut client = connect_default_daemon_client(&socket_path).await?;
    for replacement in 0..2 {
        timeout(DEFAULT_READ_TIMEOUT, async {
            loop {
                let loaded =
                    request(&mut client, /*id*/ 4, "thread/loaded/list", json!({})).await?;
                let ids: std::collections::BTreeSet<String> =
                    serde_json::from_value(loaded["data"].clone())?;
                if ids == expected && maintenance_status(&socket_path).await?["restored"] == true {
                    break;
                }
                sleep(Duration::from_millis(/*millis*/ 25)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        assert!(
            daemon_recovery_file_path(home.path()).exists(),
            "owner has not verified this generation yet"
        );
        if replacement == 0 {
            successor.kill().await?;
            successor.wait().await?;
            successor = spawn_server(home.path(), &socket_path)?;
            client = connect_default_daemon_client(&socket_path).await?;
        }
    }
    start_thread(&mut client, /*id*/ 5, json!({})).await?;
    request_shutdown(&successor, &socket_path).await?;
    wait_success(&mut successor).await?;
    Ok(())
}

async fn maintenance_status(socket_path: &Path) -> Result<serde_json::Value> {
    use codex_app_server_transport::maintenance::MaintenanceCommand;
    let stream = UnixStream::connect(socket_path).await?;
    let (mut socket, _) = client_async("ws://localhost/daemon/maintenance", stream).await?;
    socket
        .send(Message::Text(
            serde_json::to_string(&MaintenanceCommand::Status)?.into(),
        ))
        .await?;
    let frame = timeout(DEFAULT_READ_TIMEOUT, socket.next())
        .await?
        .context("status reply")??;
    let Message::Text(text) = frame else {
        anyhow::bail!("expected status")
    };
    socket.close(None).await?;
    Ok(serde_json::from_str(&text)?)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentTreeScenario {
    Continue,
    StopChild,
    OwnerDisconnect,
    StopDuringRestore,
    LegacyChild,
}

#[cfg(unix)]
#[test_case::test_case(AgentTreeScenario::Continue; "continue_both")]
#[test_case::test_case(AgentTreeScenario::LegacyChild; "legacy_child_keeps_its_model")]
#[test_case::test_case(AgentTreeScenario::StopChild; "stop_child_after_ready")]
#[test_case::test_case(AgentTreeScenario::OwnerDisconnect; "owner_exits_before_ready")]
#[test_case::test_case(AgentTreeScenario::StopDuringRestore; "stop_while_restoration_waits")]
#[tokio::test]
async fn maintenance_restores_active_parent_and_child_without_replaying_tools(
    scenario: AgentTreeScenario,
) -> Result<()> {
    let stop_child = scenario == AgentTreeScenario::StopChild;
    use codex_app_server_transport::maintenance::MaintenanceCommand;
    use codex_app_server_transport::maintenance::MaintenanceResponse;
    use core_test_support::responses;
    let home = TempDir::new()?;
    let (release_a, gate_a) = oneshot::channel();
    let (release_b, gate_b) = oneshot::channel();
    let (release_c, gate_c) = oneshot::channel();
    let (release_d, gate_d) = oneshot::channel();
    let effects = home.path().join("effects.txt");
    let tool_step = |call: &str, gate| {
        StreamingSseChunk {
        gate: Some(gate),
        body: responses::sse(vec![
            responses::ev_response_created(call),
            responses::ev_function_call(call, "exec_command", &json!({"cmd":format!("echo {call} >> '{}'; echo checkpoint-result", effects.display()),"yield_time_ms":5000,"max_output_tokens":200}).to_string()),
            responses::ev_completed(call),
        ]),
    }
    };
    let legacy_child = scenario == AgentTreeScenario::LegacyChild;
    let (namespace, spawn_args) = if legacy_child {
        (
            "multi_agent_v1",
            json!({"message":"Do the child work","model":"gpt-6-luna","reasoning_effort":"low","fork_context":false}),
        )
    } else {
        (
            "collaboration",
            json!({"task_name":"worker","message":"Do the child work","fork_turns":"none"}),
        )
    };
    let (mock, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("spawn"),
                responses::ev_function_call_with_namespace(
                    "spawn-worker",
                    namespace,
                    "spawn_agent",
                    &spawn_args.to_string(),
                ),
                responses::ev_completed("spawn"),
            ]),
        }],
        vec![tool_step("checkpoint-a", gate_a)],
        vec![tool_step("checkpoint-b", gate_b)],
        vec![stream_chunk(Some(gate_c), "Restored first")?],
        vec![stream_chunk(Some(gate_d), "Restored second")?],
    ])
    .await;
    create_config_toml(home.path(), mock.uri(), "never")?;
    let mut config = std::fs::read_to_string(home.path().join("config.toml"))?;
    config = config.replace(
        "sandbox_mode = \"read-only\"",
        "sandbox_mode = \"danger-full-access\"",
    );
    config = config.replace("model = \"mock-model\"", "model = \"gpt-6-sol\"");
    config.push_str("\n[features]\nhooks = true\nstep_model_switching = true\nmulti_agent = true\ncode_mode = false\ncode_mode_only = false\n[features.multi_agent_v2]\nenabled = true\n");
    if legacy_child {
        config = config.replace(
            "[features.multi_agent_v2]\nenabled = true",
            "[features.multi_agent_v2]\nenabled = false",
        );
    }
    std::fs::write(home.path().join("config.toml"), config)?;
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
    let parent = if legacy_child {
        let id = app_test_support::create_fake_rollout(
            home.path(),
            "2025-01-05T12-00-00",
            "2025-01-05T12:00:00Z",
            "Saved legacy work",
            Some("mock_provider"),
            /*git_info*/ None,
        )?;
        serde_json::from_value::<ThreadStartResponse>(
            request(
                &mut client,
                /*id*/ 2,
                "thread/resume",
                json!({"threadId":id,"cwd":home.path(),"model":"gpt-6-sol"}),
            )
            .await?,
        )?
    } else {
        start_thread(&mut client, /*id*/ 2, json!({"cwd":home.path()})).await?
    };
    start_turn(&mut client, /*id*/ 3, &parent.thread.id).await?;
    wait_for_requests(&mock, /*count*/ 3).await?;
    let stream = UnixStream::connect(&socket_path).await?;
    let (mut control, _) = client_async("ws://localhost/daemon/maintenance", stream).await?;
    let pid = server.id().context("pid")?;
    control
        .send(Message::Text(
            serde_json::to_string(&MaintenanceCommand::Prepare {
                operation_id: "agent-tree".into(),
                pid,
            })?
            .into(),
        ))
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, async {
        while maintenance_status(&socket_path).await?["preparing"] != true {
            sleep(Duration::from_millis(10)).await;
        }
        Ok::<(), anyhow::Error>(())
    })
    .await??;
    // Both model responses remain held: preparation must not block unrelated
    // fresh sessions or accepted steering while participants reach their boundary.
    let added = start_thread(&mut client, /*id*/ 30, json!({})).await?;
    let read = request(
        &mut client,
        /*id*/ 31,
        "thread/read",
        json!({"threadId":parent.thread.id,"includeTurns":true}),
    )
    .await?;
    let parent_turn = read["thread"]["turns"]
        .as_array()
        .and_then(|turns| turns.last())
        .context("parent turn history")?["id"]
        .as_str()
        .context("parent turn")?
        .to_string();
    request(&mut client, /*id*/ 32, "turn/steer", json!({"threadId":parent.thread.id,"expectedTurnId":parent_turn,"input":[{"type":"text","text":"preserved steering during preparation"}]})).await?;
    let settings = request(&mut client, /*id*/ 33, "turn/settings/update", json!({"threadId":parent.thread.id,"turnId":parent_turn,"model":"gpt-6-astra","effort":"high","summary":"detailed"})).await?;
    assert_eq!(settings["status"], "applied");
    assert_eq!(mock.requests().await.len(), 3);
    if scenario == AgentTreeScenario::OwnerDisconnect {
        control.close(None).await?;
    }
    release_a
        .send(())
        .expect("held response still has its receiver");
    release_b
        .send(())
        .expect("held response still has its receiver");
    if scenario == AgentTreeScenario::OwnerDisconnect {
        wait_for_requests(&mock, /*count*/ 5).await?;
        let status = maintenance_status(&socket_path).await?;
        assert_eq!(status["accepting"], true);
        assert_eq!(status["preparing"], false);
        start_thread(&mut client, /*id*/ 40, json!({})).await?;
        assert!(server.try_wait()?.is_none());
        assert!(!daemon_recovery_file_path(home.path()).exists());
        release_c
            .send(())
            .expect("held response still has its receiver");
        release_d
            .send(())
            .expect("held response still has its receiver");
        request_shutdown(&server, &socket_path).await?;
        wait_success(&mut server).await?;
        return Ok(());
    }
    let frame = timeout(Duration::from_secs(30), control.next())
        .await?
        .context("prepare reply")??;
    let Message::Text(text) = frame else {
        anyhow::bail!("expected checkpoint receipt")
    };
    assert_eq!(
        serde_json::from_str::<MaintenanceResponse>(&text)?,
        MaintenanceResponse::Ready {
            operation_id: "agent-tree".into(),
            pid
        }
    );
    assert_eq!(mock.requests().await.len(), 3);
    let loaded = request(&mut client, /*id*/ 34, "thread/loaded/list", json!({})).await?;
    let child_id = loaded["data"]
        .as_array()
        .context("loaded agent tree")?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .find(|id| *id != parent.thread.id && *id != added.thread.id)
        .context("child")?
        .to_string();
    let child_before = request(
        &mut client,
        /*id*/ 35,
        "thread/read",
        json!({"threadId":child_id,"includeTurns":true}),
    )
    .await?;
    let child_turn = child_before["thread"]["turns"][0]["id"]
        .as_str()
        .context("child turn")?
        .to_string();
    if stop_child {
        request(
            &mut client,
            /*id*/ 36,
            "turn/interrupt",
            json!({"threadId":child_id,"turnId":child_turn}),
        )
        .await?;
    }
    control
        .send(Message::Text(
            serde_json::to_string(&MaintenanceCommand::Commit {
                operation_id: "agent-tree".into(),
                pid,
            })?
            .into(),
        ))
        .await?;
    let receipt = timeout(DEFAULT_READ_TIMEOUT, control.next())
        .await?
        .context("agent-tree commit receipt")??;
    let Message::Text(receipt) = receipt else {
        anyhow::bail!("expected commit receipt");
    };
    assert_eq!(
        serde_json::from_str::<MaintenanceResponse>(&receipt)?,
        MaintenanceResponse::Committed {
            operation_id: "agent-tree".into(),
            pid
        }
    );
    wait_success(&mut server).await?;
    let saved = daemon_recovery::read_snapshot(&daemon_recovery_file_path(home.path()))?;
    assert_eq!(saved.interrupted.len(), if stop_child { 1 } else { 2 });
    assert!(saved.loaded.contains(&added.thread.id));
    let retained = saved.maintenance.as_ref().context("maintenance context")?;
    assert_eq!(
        retained.turn_contexts[&parent.thread.id]
            .collaboration_mode
            .settings
            .model,
        "gpt-6-astra"
    );
    if !stop_child && !legacy_child {
        assert_eq!(
            retained.turn_contexts[&child_id]
                .options
                .parent_turn_id
                .as_deref(),
            Some(parent_turn.as_str())
        );
    }
    let parents = &saved.maintenance.as_ref().context("tree metadata")?.parents;
    assert_eq!(parents.get(&parent.thread.id), Some(&None));
    assert_eq!(
        parents
            .values()
            .filter(|parent_id| parent_id.as_deref() == Some(parent.thread.id.as_str()))
            .count(),
        1
    );
    let held_history = if scenario == AgentTreeScenario::StopDuringRestore {
        let path = parent.thread.path.clone().context("parent history path")?;
        let held = path.with_extension("held-history");
        std::fs::rename(&path, &held)?;
        // A directory at the history-file path deterministically prevents cold
        // loading, including when the metadata projection is already cached.
        std::fs::create_dir(&path)?;
        Some((path, held))
    } else {
        None
    };
    let mut successor = spawn_server(home.path(), &socket_path)?;
    if scenario == AgentTreeScenario::StopDuringRestore {
        let mut client = connect_default_daemon_client(&socket_path).await?;
        let (path, held) = held_history.context("held history")?;
        timeout(DEFAULT_READ_TIMEOUT, async {
            loop {
                let state = maintenance_status(&socket_path).await?;
                if state["preparing"] == false
                    && state["restored"] == false
                    && state["accepting"] == true
                {
                    break;
                }
                sleep(Duration::from_millis(/*millis*/ 10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        assert_eq!(
            mock.requests().await.len(),
            3,
            "no saved turn may run before this failed restore is repaired"
        );
        #[cfg(target_os = "linux")]
        {
            // A per-process descriptor limit produces a real pre-publication I/O
            // failure even when the test namespace can bypass Unix mode bits.
            let checkpoint_path = daemon_recovery_file_path(home.path());
            let before_stop = std::fs::read(&checkpoint_path)?;
            let pid = successor.id().context("replacement pid")?.to_string();
            let limit = StdCommand::new("prlimit")
                .args([
                    "--pid",
                    &pid,
                    "--nofile",
                    "--noheadings",
                    "--output",
                    "SOFT",
                ])
                .output()?;
            anyhow::ensure!(
                limit.status.success(),
                "could not inspect owned test server limit"
            );
            let original = String::from_utf8(limit.stdout)?.trim().to_string();
            anyhow::ensure!(
                StdCommand::new("prlimit")
                    .args(["--pid", &pid, "--nofile=0:"])
                    .status()?
                    .success(),
                "could not inject checkpoint write failure"
            );
            let rejected = request(
                &mut client,
                /*id*/ 40,
                "turn/interrupt",
                json!({"threadId":parent.thread.id,"turnId":parent_turn}),
            )
            .await;
            anyhow::ensure!(
                StdCommand::new("prlimit")
                    .args(["--pid", &pid, &format!("--nofile={original}:")])
                    .status()?
                    .success(),
                "could not restore owned test server limit"
            );
            assert!(
                rejected.is_err(),
                "Stop must not claim persistence when its write fails"
            );
            assert_eq!(std::fs::read(&checkpoint_path)?, before_stop);
        }
        request(
            &mut client,
            /*id*/ 41,
            "turn/interrupt",
            json!({"threadId":parent.thread.id,"turnId":parent_turn}),
        )
        .await?;
        let stopped = daemon_recovery::read_snapshot(&daemon_recovery_file_path(home.path()))?;
        assert!(!stopped.interrupted.contains_key(&parent.thread.id));
        std::fs::remove_dir(&path)?;
        std::fs::rename(held, &path)?;
        successor.kill().await?;
        successor.wait().await?;
        successor = spawn_server(home.path(), &socket_path)?;
        client = connect_default_daemon_client(&socket_path).await?;
        wait_for_requests(&mock, /*count*/ 4).await?;
        release_c
            .send(())
            .expect("held response still has its receiver");
        timeout(DEFAULT_READ_TIMEOUT, async {
            loop {
                let read = request(
                    &mut client,
                    /*id*/ 42,
                    "thread/read",
                    json!({"threadId":parent.thread.id,"includeTurns":true}),
                )
                .await?;
                if read["thread"]["turns"][0]["status"] == "interrupted" {
                    break;
                }
                sleep(Duration::from_millis(/*millis*/ 10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        request_shutdown(&successor, &socket_path).await?;
        wait_success(&mut successor).await?;
        assert_eq!(mock.requests().await.len(), 4);
        return Ok(());
    }
    let expected_requests = if stop_child { 4 } else { 5 };
    wait_for_requests(&mock, expected_requests).await?;
    let mut client = connect_default_daemon_client(&socket_path).await?;
    let loaded = request(&mut client, /*id*/ 37, "thread/loaded/list", json!({})).await?;
    assert!(
        loaded["data"]
            .as_array()
            .context("restored census")?
            .contains(&json!(added.thread.id))
    );
    for (id, interrupted) in &saved.interrupted {
        let read = request(
            &mut client,
            /*id*/ 4,
            "thread/read",
            json!({"threadId":id,"includeTurns":true}),
        )
        .await?;
        let turns = read["thread"]["turns"]
            .as_array()
            .context("restored turns")?;
        let matching: Vec<_> = turns
            .iter()
            .filter(|turn| turn["id"] == interrupted.turn_id)
            .collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0]["status"], "inProgress");
        if id == &child_id {
            assert_eq!(read["thread"]["parentThreadId"], parent.thread.id);
            if legacy_child {
                assert_eq!(read["thread"]["model"], "gpt-6-luna");
                assert_eq!(read["thread"]["reasoningEffort"], "low");
            }
        }
    }
    let requests = mock.requests().await;
    let mut restored_calls = std::collections::BTreeSet::new();
    for body in &requests[3..] {
        let body: serde_json::Value = serde_json::from_slice(body)?;
        let outputs: Vec<_> = body["input"]
            .as_array()
            .context("input")?
            .iter()
            .filter(|item| {
                item["type"] == "function_call_output"
                    && item["call_id"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("checkpoint-"))
            })
            .collect();
        assert_eq!(outputs.len(), 1);
        let output = outputs[0]["output"].to_string();
        assert!(
            output.contains("checkpoint-result") && output.contains("Process exited with code 0"),
            "{output}"
        );
        restored_calls.insert(
            outputs[0]["call_id"]
                .as_str()
                .expect("tool output call identity")
                .to_string(),
        );
    }
    if stop_child {
        assert_eq!(restored_calls.len(), 1);
    } else {
        assert_eq!(
            restored_calls,
            ["checkpoint-a".to_string(), "checkpoint-b".to_string()].into()
        );
    }
    let parent_request = requests[3..]
        .iter()
        .map(|body| {
            serde_json::from_slice::<serde_json::Value>(body)
                .expect("captured response request JSON")
        })
        .find(|body| {
            body["input"]
                .as_array()
                .expect("captured response input array")
                .iter()
                .any(|item| item["call_id"] == "spawn-worker")
        })
        .context("parent continuation")?;
    assert_eq!(parent_request["model"], "gpt-6-astra");
    assert_eq!(parent_request["reasoning"]["effort"], "high");
    assert!(
        parent_request
            .to_string()
            .contains("preserved steering during preparation")
    );
    let first: serde_json::Value = serde_json::from_slice(&requests[3])?;
    let parent_first = first["input"]
        .as_array()
        .context("input")?
        .iter()
        .any(|item| item["call_id"] == "spawn-worker");
    let (release_parent, release_child) = if parent_first {
        (release_c, release_d)
    } else {
        (release_d, release_c)
    };
    release_parent
        .send(())
        .expect("held response still has its receiver");
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let read = request(
                &mut client,
                /*id*/ 8,
                "thread/read",
                json!({"threadId":parent.thread.id,"includeTurns":true}),
            )
            .await?;
            if read["thread"]["turns"].as_array().is_some_and(|turns| {
                turns
                    .iter()
                    .any(|turn| turn["id"] == parent_turn && turn["status"] == "completed")
            }) {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        Ok::<(), anyhow::Error>(())
    })
    .await??;
    if stop_child {
        let child = request(
            &mut client,
            /*id*/ 38,
            "thread/read",
            json!({"threadId":child_id,"includeTurns":true}),
        )
        .await?;
        assert_eq!(child["thread"]["turns"][0]["status"], "interrupted");
    } else {
        release_child
            .send(())
            .expect("held response still has its receiver");
    }
    let mut observed_effects: Vec<_> = std::fs::read_to_string(&effects)?
        .lines()
        .map(str::to_string)
        .collect();
    observed_effects.sort();
    assert_eq!(observed_effects, vec!["checkpoint-a", "checkpoint-b"]);
    request_shutdown(&successor, &socket_path).await?;
    wait_success(&mut successor).await?;
    assert_eq!(mock.requests().await.len(), expected_requests);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn maintenance_blocked_snapshot_reopens_server_without_killing_work() -> Result<()> {
    use codex_app_server_transport::maintenance::MaintenanceCommand;
    use codex_app_server_transport::maintenance::MaintenanceResponse;
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
    assert!(
        StdCommand::new("mkfifo")
            .arg(&compressed)
            .status()?
            .success()
    );
    let stream = UnixStream::connect(&socket_path).await?;
    let (mut control, _) = client_async("ws://localhost/daemon/maintenance", stream).await?;
    let pid = server.id().context("pid")?;
    control
        .send(Message::Text(
            serde_json::to_string(&MaintenanceCommand::Prepare {
                operation_id: "blocked-writer".into(),
                pid,
            })?
            .into(),
        ))
        .await?;
    let frame = timeout(DEFAULT_READ_TIMEOUT, control.next())
        .await?
        .context("prepare reply")??;
    let Message::Text(text) = frame else {
        anyhow::bail!("expected readiness")
    };
    assert_eq!(
        serde_json::from_str::<MaintenanceResponse>(&text)?,
        MaintenanceResponse::Ready {
            operation_id: "blocked-writer".into(),
            pid
        }
    );
    control
        .send(Message::Text(
            serde_json::to_string(&MaintenanceCommand::Commit {
                operation_id: "blocked-writer".into(),
                pid,
            })?
            .into(),
        ))
        .await?;
    let writer = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            if let Ok(writer) = tokio::net::unix::pipe::OpenOptions::new().open_sender(&compressed)
            {
                break writer;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("checkpoint did not reach the real blocked writer")?;
    timeout(Duration::from_secs(30), async {
        loop {
            let state = maintenance_status(&socket_path).await?;
            if state["preparing"] == false && state["accepting"] == true {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
        Ok::<(), anyhow::Error>(())
    })
    .await??;
    assert!(server.try_wait()?.is_none());
    start_thread(&mut client, /*id*/ 3, json!({})).await?;
    assert!(!daemon_recovery_file_path(home.path()).exists());
    drop(writer);
    std::fs::remove_file(compressed)?;
    request_shutdown(&server, &socket_path).await?;
    wait_success(&mut server).await?;
    Ok(())
}

#[tokio::test]
async fn maintenance_corrupt_checkpoint_is_retained_without_claiming_restoration() -> Result<()> {
    let home = TempDir::new()?;
    let (mock, _) = start_streaming_sse_server(vec![]).await;
    create_config_toml(home.path(), mock.uri(), "never")?;
    let path = daemon_recovery_file_path(home.path());
    std::fs::create_dir_all(path.parent().context("checkpoint parent")?)?;
    let corrupted = serde_json::to_vec(&vec!["codex-maintenance-v1:{broken"])?;
    std::fs::write(&path, &corrupted)?;
    let socket_path = home.path().join("control/server.sock");
    let mut server = spawn_server(home.path(), &socket_path)?;
    let mut client = connect_default_daemon_client(&socket_path).await?;
    let state = maintenance_status(&socket_path).await?;
    assert_eq!(state["accepting"], true);
    assert_eq!(state["restored"], false);
    assert_eq!(std::fs::read(&path)?, corrupted);
    start_thread(&mut client, /*id*/ 2, json!({})).await?;
    request_shutdown(&server, &socket_path).await?;
    wait_success(&mut server).await?;
    Ok(())
}
