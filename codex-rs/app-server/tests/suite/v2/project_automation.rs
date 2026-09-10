use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol as api;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

async fn command(
    app: &mut TestAppServer,
    params: Value,
) -> Result<api::ProjectAutomationCommandResponse> {
    let request_id = app
        .send_raw_request("projectAutomation/command", Some(params))
        .await?;
    let response = app
        .read_stream_until_response_message(api::RequestId::Integer(request_id))
        .await?;
    Ok(serde_json::from_value(response.result)?)
}

async fn rejected(app: &mut TestAppServer, params: Value) -> Result<api::JSONRPCErrorError> {
    let request_id = app
        .send_raw_request("projectAutomation/command", Some(params))
        .await?;
    Ok(app
        .read_stream_until_error_message(api::RequestId::Integer(request_id))
        .await?
        .error)
}

#[tokio::test]
async fn project_automation_public_rpc_preserves_scope_and_revision() -> Result<()> {
    let server =
        create_mock_responses_server_sequence(vec![create_final_assistant_message_sse_response(
            "Saved task ready.",
        )?])
        .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut setup = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = setup
        .start_thread(api::ThreadStartParams {
            cwd: Some(home.path().to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?
        .thread
        .id;
    let completed = setup
        .start_turn_and_wait_for_completion(api::TurnStartParams {
            thread_id: thread.clone(),
            input: vec![api::UserInput::Text {
                text: "Save this task.".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    assert_eq!(completed.turn.status, api::TurnStatus::Completed);
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build()
        .await?;
    let initialized = app
        .initialize_with_capabilities(
            api::ClientInfo {
                name: "project-automation-test".into(),
                title: None,
                version: "0.1.0".into(),
            },
            Some(api::InitializeCapabilities::default()),
        )
        .await?;
    let api::JSONRPCMessage::Response(initialized) = initialized else {
        anyhow::bail!("initialize failed")
    };
    let initialized: api::InitializeResponse = serde_json::from_value(initialized.result)?;
    assert_eq!(initialized.event_subscriptions, None);
    assert_eq!(
        command(&mut app, json!({})).await?,
        api::ProjectAutomationCommandResponse {
            capability: Some(api::ProjectAutomationCapability { version: 1 }),
            project: None,
        }
    );
    let bound = command(&mut app, json!({
        "threadId": thread, "command": {"action": "bind", "purpose": "implementation", "workstream": "adapter"}
    })).await?.project.context("bound project")?;
    assert_eq!(bound.owner_thread_id, thread);
    assert_eq!(
        bound.thread_workstreams.get(&thread).map(String::as_str),
        Some("adapter")
    );
    assert_eq!(
        command(&mut app, json!({"projectId": bound.project_id}))
            .await?
            .project,
        Some(bound.clone())
    );
    let linked = command(&mut app, json!({"threadId": thread, "expectedRevision": bound.revision,
        "command": {"action": "linkWork", "outcomeId": "outcome-context", "experimentRef": "review-context@1"}
    })).await?.project.context("linked project")?;
    let updated = command(
        &mut app,
        json!({"threadId": thread, "expectedRevision": linked.revision,
            "command": {"action": "linkWork", "experimentRef": "review-context@2"}
        }),
    )
    .await?
    .project
    .context("updated link")?;
    assert_eq!(
        (
            updated.thread_outcomes.get(&thread).map(String::as_str),
            updated.thread_experiments.get(&thread).map(String::as_str)
        ),
        (Some("outcome-context"), Some("review-context@2"))
    );
    let cleared = command(
        &mut app,
        json!({"threadId": thread, "expectedRevision": updated.revision,
            "command": {"action": "linkWork", "clearOutcome": true}
        }),
    )
    .await?
    .project
    .context("cleared outcome")?;
    assert!(cleared.thread_outcomes.is_empty());
    assert_eq!(cleared.thread_experiments, updated.thread_experiments);
    for invalid in [
        json!({"action": "linkWork"}),
        json!({"action": "linkWork", "clearOutcome": false, "clearExperiment": false}),
        json!({"action": "linkWork", "outcomeId": "outcome-context", "clearOutcome": true}),
        json!({"action": "linkWork", "experimentRef": "review-context@3", "clearExperiment": true}),
    ] {
        let error = rejected(
            &mut app,
            json!({"threadId": thread, "expectedRevision": cleared.revision, "command": invalid}),
        )
        .await?;
        assert_eq!(error.code, -32602);
    }
    assert_eq!(
        command(&mut app, json!({"threadId": thread}))
            .await?
            .project,
        Some(cleared.clone())
    );
    let activated = command(&mut app, json!({
        "threadId": thread, "expectedRevision": cleared.revision,
        "command": {"action": "activateDelivery", "target": "cli", "surface": "local executable", "acceptance": "project status responds"}
    })).await?.project.context("activated project")?;
    let obligation = activated.delivery.get("cli").context("delivery target")?;
    assert_eq!(
        (
            obligation.delivery_due_at_ms - obligation.started_at_ms,
            obligation.hard_stop_at_ms - obligation.started_at_ms
        ),
        (86_400_000, 129_600_000),
    );
    let postponed = command(&mut app, json!({
        "threadId": thread, "expectedRevision": activated.revision,
        "command": {"action": "postpone", "target": "cli", "deliveryDueAtMs": obligation.delivery_due_at_ms + 1000,
            "hardStopAtMs": obligation.hard_stop_at_ms + 1000, "authorizationRef": "test-owner-postponement"}
    })).await?.project.context("postponed project")?;
    let mut expected_obligation = obligation.clone();
    expected_obligation.delivery_due_at_ms += 1000;
    expected_obligation.hard_stop_at_ms += 1000;
    expected_obligation.revision += 1;
    assert_eq!(postponed.delivery.get("cli"), Some(&expected_obligation));
    assert_eq!(postponed.started_at_ms, bound.started_at_ms);
    let stale = json!({"threadId": thread, "expectedRevision": activated.revision,
        "command": {"action": "pause", "authorizationRef": "test-owner-pause"}});
    let conflict = rejected(&mut app, stale).await?;
    assert_eq!(conflict.code, -32602);
    assert!(conflict.message.contains("revision changed"));
    let missing_revision = rejected(
        &mut app,
        json!({"threadId": thread, "command": {"action": "resume"}}),
    )
    .await?;
    assert_eq!(missing_revision.code, -32602);
    assert_eq!(
        command(&mut app, json!({"threadId": thread}))
            .await?
            .project,
        Some(postponed.clone())
    );
    let unknown_scope = rejected(
        &mut app,
        json!({"projectId": "unknown-scope", "threadId": thread,
        "command": {"action": "bind", "purpose": "analysis"}}),
    )
    .await?;
    assert_eq!(unknown_scope.code, -32602);
    let unknown_thread = rejected(
        &mut app,
        json!({"threadId": "00000000-0000-0000-0000-000000000001",
        "command": {"action": "bind", "purpose": "analysis"}}),
    )
    .await?;
    assert_eq!(unknown_thread.code, -32602);
    assert_eq!(
        command(&mut app, json!({"projectId": "unknown-scope"}))
            .await?
            .project,
        None
    );
    let unverified = rejected(&mut app, json!({"threadId": thread, "expectedRevision": postponed.revision,
        "command": {"action": "recordDelivery", "target": "cli", "deliveredAtMs": obligation.started_at_ms,
            "evidenceRef": "not-a-coordinator-receipt"}})).await?;
    assert_eq!(unverified.code, -32602);
    assert_eq!(
        command(&mut app, json!({"threadId": thread}))
            .await?
            .project,
        Some(postponed)
    );
    let unknown_field = rejected(
        &mut app,
        json!({"threadId": thread,
        "command": {"action": "bind", "purpose": "analysis", "verified": true}}),
    )
    .await?;
    assert_eq!(unknown_field.code, -32600);
    Ok(())
}

#[tokio::test]
async fn project_automation_capability_requires_local_controls() -> Result<()> {
    let server = create_mock_responses_server_sequence(Vec::new()).await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_root_config("[tools.local_controls]\nenabled = false")
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    assert_eq!(
        command(&mut app, json!({})).await?,
        api::ProjectAutomationCommandResponse {
            capability: None,
            project: None
        }
    );
    let unavailable = rejected(&mut app, json!({"projectId": "project"})).await?;
    assert_eq!(unavailable.code, -32600);
    Ok(())
}
