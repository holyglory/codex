//! Real queue-only agent mail crosses the daemon checkpoint and replacement.
use super::*;
use codex_app_server_transport::maintenance::MaintenanceCommand;
use codex_app_server_transport::maintenance::MaintenanceResponse;
use core_test_support::responses;
use pretty_assertions::assert_eq;

async fn wait_completed(client: &mut WebSocketStream<UnixStream>, thread_id: &str) -> Result<()> {
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let read = request(
                client,
                /*id*/ 80,
                "thread/read",
                json!({"threadId":thread_id,"includeTurns":true}),
            )
            .await?;
            if read["thread"]["turns"]
                .as_array()
                .and_then(|turns| turns.last())
                .is_some_and(|turn| turn["status"] == "completed")
            {
                return Ok::<_, anyhow::Error>(());
            }
            sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn maintenance_mailbox_routes_pending_agent_mail_through_process_replacement() -> Result<()> {
    let home = TempDir::new()?;
    let (release_a, gate_a) = oneshot::channel();
    let (release_b, gate_b) = oneshot::channel();
    let (release_mail, gate_mail) = oneshot::channel();
    let (release_c, gate_c) = oneshot::channel();
    let (release_d, gate_d) = oneshot::channel();
    let tool_step = |call: &str, name: &str, args: serde_json::Value, gate| StreamingSseChunk {
        gate,
        body: responses::sse(vec![
            responses::ev_response_created(call),
            responses::ev_function_call_with_namespace(
                call,
                "collaboration",
                name,
                &args.to_string(),
            ),
            responses::ev_completed(call),
        ]),
    };
    let (mock, _) = start_streaming_sse_server(vec![
        vec![tool_step(
            "spawn-worker",
            "spawn_agent",
            json!({"task_name":"worker","message":"Complete initial work","fork_turns":"none"}),
            /*gate*/ None,
        )],
        vec![stream_chunk(Some(gate_a), "Initial first")?],
        vec![stream_chunk(Some(gate_b), "Initial second")?],
        vec![tool_step(
            "queue-before-handover",
            "send_message",
            json!({"target":"worker","message":"maintenance-mail-proof"}),
            Some(gate_mail),
        )],
        vec![tool_step(
            "consume-after-handover",
            "followup_task",
            json!({"target":"worker","message":"consume-restored-maintenance-mail"}),
            /*gate*/ None,
        )],
        vec![stream_chunk(Some(gate_c), "Restored first")?],
        vec![stream_chunk(Some(gate_d), "Restored second")?],
    ])
    .await;
    create_config_toml(home.path(), mock.uri(), "never")?;
    let config = std::fs::read_to_string(home.path().join("config.toml"))?
        .replace("model = \"mock-model\"", "model = \"gpt-6-sol\"");
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "{config}\n[features]\nmulti_agent = true\ncode_mode = false\ncode_mode_only = false\n[features.multi_agent_v2]\nenabled = true\n"
        ),
    )?;
    let socket_path = home.path().join("control/server.sock");
    let mut server = spawn_server(home.path(), &socket_path)?;
    let mut client = connect_default_daemon_client(&socket_path).await?;
    let parent = start_thread(&mut client, /*id*/ 2, json!({"cwd":home.path()})).await?;
    start_turn(&mut client, /*id*/ 3, &parent.thread.id).await?;
    wait_for_requests(&mock, /*count*/ 3).await?;
    let loaded = request(&mut client, /*id*/ 4, "thread/loaded/list", json!({})).await?;
    let child_id = loaded["data"]
        .as_array()
        .context("agent tree")?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .find(|id| *id != parent.thread.id)
        .context("child")?
        .to_string();
    let requests = mock.requests().await;
    let first: serde_json::Value = serde_json::from_slice(&requests[1])?;
    let parent_first = first["input"]
        .as_array()
        .context("initial input")?
        .iter()
        .any(|item| item["call_id"] == "spawn-worker");
    let (release_parent, release_child) = if parent_first {
        (release_a, release_b)
    } else {
        (release_b, release_a)
    };
    release_parent.send(()).expect("initial parent response");
    wait_completed(&mut client, &parent.thread.id).await?;
    release_child.send(()).expect("initial child response");
    wait_completed(&mut client, &child_id).await?;
    start_turn(&mut client, /*id*/ 5, &parent.thread.id).await?;
    wait_for_requests(&mock, /*count*/ 4).await?;
    let pid = server.id().context("source pid")?;
    let (mut control, _) = client_async(
        "ws://localhost/daemon/maintenance",
        UnixStream::connect(&socket_path).await?,
    )
    .await?;
    control
        .send(Message::Text(
            serde_json::to_string(&MaintenanceCommand::Prepare {
                operation_id: "queued-mail".into(),
                pid,
            })?
            .into(),
        ))
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, async {
        while maintenance_tests::maintenance_status(&socket_path).await?["preparing"] != true {
            sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    release_mail.send(()).expect("queued mail response");
    for expected in [
        MaintenanceResponse::Ready {
            operation_id: "queued-mail".into(),
            pid,
        },
        MaintenanceResponse::Committed {
            operation_id: "queued-mail".into(),
            pid,
        },
    ] {
        let Message::Text(reply) = timeout(DEFAULT_READ_TIMEOUT, control.next())
            .await?
            .context("maintenance receipt")??
        else {
            anyhow::bail!("invalid maintenance receipt");
        };
        assert_eq!(
            serde_json::from_str::<MaintenanceResponse>(&reply)?,
            expected
        );
        if matches!(expected, MaintenanceResponse::Ready { .. }) {
            control
                .send(Message::Text(
                    serde_json::to_string(&MaintenanceCommand::Commit {
                        operation_id: "queued-mail".into(),
                        pid,
                    })?
                    .into(),
                ))
                .await?;
        }
    }
    wait_success(&mut server).await?;
    assert_eq!(mock.requests().await.len(), 4);
    let saved = daemon_recovery::read_snapshot(&daemon_recovery_file_path(home.path()))?;
    assert!(
        !saved.interrupted.contains_key(&child_id),
        "queue-only mail must leave an idle recipient idle"
    );
    let retained = saved.maintenance.context("maintenance snapshot")?;
    let mail: Vec<_> = retained.mailboxes[&child_id]
        .iter()
        .filter(|mail| {
            mail.communication.encrypted_content.as_deref() == Some("maintenance-mail-proof")
        })
        .collect();
    assert_eq!(mail.len(), 1);
    let communication = &mail[0].communication;
    assert_eq!(
        (
            communication.author.to_string(),
            communication.recipient.to_string(),
            communication.trigger_turn
        ),
        ("/root".into(), "/root/worker".into(), false)
    );
    let mail_id = communication.id.clone().context("stable mail identity")?;
    let mut successor = spawn_server(home.path(), &socket_path)?;
    let mut client = connect_default_daemon_client(&socket_path).await?;
    wait_for_requests(&mock, /*count*/ 7).await?;
    let requests = mock.requests().await;
    let resumed = requests[5..]
        .iter()
        .map(|body| serde_json::from_slice::<serde_json::Value>(body))
        .collect::<Result<Vec<_>, _>>()?;
    let child_index = resumed
        .iter()
        .position(|body| {
            body["input"].as_array().is_some_and(|input| {
                input.iter().any(|item| {
                    item["type"] == "agent_message"
                        && item["content"]
                            .to_string()
                            .contains("consume-restored-maintenance-mail")
                })
            })
        })
        .context("child followup request")?;
    let delivered: Vec<_> = resumed[child_index]["input"]
        .as_array()
        .context("child input")?
        .iter()
        .filter(|item| {
            item["type"] == "agent_message"
                && item["content"]
                    .to_string()
                    .contains("maintenance-mail-proof")
        })
        .collect();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["id"], json!(mail_id));
    assert_eq!(
        (&delivered[0]["author"], &delivered[0]["recipient"]),
        (&json!("/root"), &json!("/root/worker"))
    );
    let (release_parent, release_child) = if child_index == 0 {
        (release_d, release_c)
    } else {
        (release_c, release_d)
    };
    release_parent.send(()).expect("restored parent response");
    wait_completed(&mut client, &parent.thread.id).await?;
    release_child.send(()).expect("restored child response");
    wait_completed(&mut client, &child_id).await?;
    request_shutdown(&successor, &socket_path).await?;
    wait_success(&mut successor).await?;
    assert_eq!(mock.requests().await.len(), 7);
    Ok(())
}
