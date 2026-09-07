use std::collections::BTreeMap;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_escalated_command_execution_sse_response;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::EventPublishParams;
use codex_app_server_protocol::EventPublishResponse;
use codex_app_server_protocol::EventSourceCursor;
use codex_app_server_protocol::EventSubscriptionCancelParams;
use codex_app_server_protocol::EventSubscriptionCancelResponse;
use codex_app_server_protocol::EventSubscriptionCreateParams;
use codex_app_server_protocol::EventSubscriptionCreateResponse;
use codex_app_server_protocol::EventSubscriptionFilter;
use codex_app_server_protocol::EventSubscriptionHeartbeat;
use codex_app_server_protocol::EventSubscriptionListParams;
use codex_app_server_protocol::EventSubscriptionListResponse;
use codex_app_server_protocol::EventSubscriptionTriggerParams;
use codex_app_server_protocol::EventSubscriptionTriggerResponse;
use codex_app_server_protocol::IngressEvent;
use codex_app_server_protocol::InitializeResponse;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::MockServer;

const READ_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 10);

#[tokio::test]
async fn capability_and_crud_are_feature_gated() -> Result<()> {
    let server = create_mock_responses_server_sequence(Vec::new()).await;
    let disabled_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(disabled_home.path())?;
    let mut disabled = TestAppServer::builder()
        .with_codex_home(disabled_home.path())
        .without_managed_config()
        .build()
        .await?;
    let initialize = disabled
        .initialize_with_client_info(ClientInfo {
            name: "event-subscription-disabled".to_string(),
            title: None,
            version: "0.1.0".to_string(),
        })
        .await?;
    let JSONRPCMessage::Response(initialize) = initialize else {
        anyhow::bail!("initialize failed")
    };
    let initialize: InitializeResponse = serde_json::from_value(initialize.result)?;
    assert_eq!(initialize.event_subscriptions, None);
    let request_id = disabled
        .send_raw_request(
            "eventSubscription/list",
            Some(serde_json::to_value(EventSubscriptionListParams {
                thread_id: None,
                cursor: None,
                limit: None,
            })?),
        )
        .await?;
    let error: JSONRPCError = disabled
        .read_stream_until_error_message(RequestId::Integer(request_id))
        .await?;
    assert!(error.error.message.contains("unavailable"));

    let (mut enabled, _home, _server) = event_app(Vec::new()).await?;
    let thread_id = enabled
        .start_thread(ThreadStartParams::default())
        .await?
        .thread
        .id;
    let created = create_subscription(&mut enabled, &thread_id).await?;
    let listed: EventSubscriptionListResponse = enabled
        .request(|request_id| ClientRequest::EventSubscriptionList {
            request_id,
            params: EventSubscriptionListParams {
                thread_id: Some(thread_id),
                cursor: None,
                limit: Some(20),
            },
        })
        .await?;
    assert_eq!(listed.data, vec![created.subscription.clone()]);
    let cancelled: EventSubscriptionCancelResponse = enabled
        .request(|request_id| ClientRequest::EventSubscriptionCancel {
            request_id,
            params: EventSubscriptionCancelParams {
                subscription_id: created.subscription.id,
            },
        })
        .await?;
    assert!(cancelled.cancelled);
    Ok(())
}

#[tokio::test]
async fn typed_event_wakes_one_thread_and_duplicate_cursor_does_not_wake_again() -> Result<()> {
    let responses = vec![create_final_assistant_message_sse_response(
        "event handled",
    )?];
    let (mut app, _home, server) = event_app(responses).await?;
    let thread_id = app
        .start_thread(ThreadStartParams::default())
        .await?
        .thread
        .id;
    let created = create_subscription(&mut app, &thread_id).await?;

    let rejected_id = app
        .send_raw_request(
            "event/publish",
            Some(serde_json::json!({
                "event": {
                    "id": "raw-event",
                    "source": "synthetic",
                    "eventType": "ready",
                    "cursor": { "sequence": 1, "value": null },
                    "labels": { "channel": "test" },
                    "occurredAt": 1_700_000_000,
                    "externalPayload": "must not cross ingress"
                }
            })),
        )
        .await?;
    let rejected: JSONRPCError = app
        .read_stream_until_error_message(RequestId::Integer(rejected_id))
        .await?;
    assert!(rejected.error.message.contains("unknown field"));
    tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
    assert!(
        server
            .received_requests()
            .await
            .context("recorded model requests")?
            .is_empty()
    );
    let published = publish(&mut app, /*sequence*/ 1).await?;
    assert_eq!(
        published.accepted_subscription_ids,
        vec![created.subscription.id.clone()]
    );
    wait_for_requests(&server, /*expected*/ 1).await?;
    let requests = server
        .received_requests()
        .await
        .context("recorded model requests")?;
    let body = String::from_utf8_lossy(&requests[0].body);
    assert!(body.contains("<event_subscription_wake>"));
    assert!(body.contains(&created.subscription.id));
    assert!(!body.contains("externalPayload"));

    let duplicate = publish(&mut app, /*sequence*/ 1).await?;
    assert_eq!(
        duplicate.ignored_subscription_ids,
        vec![created.subscription.id]
    );
    tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
    assert_eq!(
        server
            .received_requests()
            .await
            .context("recorded model requests")?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn pending_subscription_resumes_the_correct_thread_after_process_restart() -> Result<()> {
    let server = create_mock_responses_server_sequence(vec![
        create_final_assistant_message_sse_response("seeded")?,
        create_final_assistant_message_sse_response("recovered")?,
    ])
    .await;
    let home = TempDir::new()?;
    write_event_config(home.path(), &server)?;
    let mut first = build_event_app(home.path()).await?;
    let thread_id = first
        .start_thread(ThreadStartParams::default())
        .await?
        .thread
        .id;
    let _: TurnStartResponse = first
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread_id.clone(),
                input: vec![UserInput::Text {
                    text: "seed durable history".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    let _: serde_json::Value = first.read_notification("turn/completed").await?;
    let subscription = create_subscription(&mut first, &thread_id)
        .await?
        .subscription;
    drop(first);

    let mut restarted = build_event_app(home.path()).await?;
    let triggered: EventSubscriptionTriggerResponse = restarted
        .request(|request_id| ClientRequest::EventSubscriptionTrigger {
            request_id,
            params: EventSubscriptionTriggerParams {
                subscription_ids: vec![subscription.id.clone()],
            },
        })
        .await?;
    assert_eq!(triggered.triggered_subscription_ids, vec![subscription.id]);
    wait_for_requests(&server, /*expected*/ 2).await?;
    let requests = server
        .received_requests()
        .await
        .context("recorded model requests")?;
    let request = &requests[1];
    let body = String::from_utf8_lossy(&request.body);
    assert!(body.contains(&thread_id));
    assert!(body.contains("<event_subscription_wake>"));
    Ok(())
}

#[tokio::test]
async fn event_arriving_during_an_active_turn_waits_and_starts_one_follow_up() -> Result<()> {
    let responses = vec![
        blocked_turn_response()?,
        create_final_assistant_message_sse_response("active turn done")?,
        create_final_assistant_message_sse_response("event follow-up done")?,
    ];
    let (mut app, _home, server) = event_app(responses).await?;
    let thread_id = app
        .start_thread(ThreadStartParams::default())
        .await?
        .thread
        .id;
    let subscription = create_subscription(&mut app, &thread_id)
        .await?
        .subscription;
    let approval_id = start_blocked_turn(&mut app, &thread_id).await?;

    let triggered: EventSubscriptionTriggerResponse = app
        .request(|request_id| ClientRequest::EventSubscriptionTrigger {
            request_id,
            params: EventSubscriptionTriggerParams {
                subscription_ids: vec![subscription.id.clone()],
            },
        })
        .await?;
    assert_eq!(
        triggered.triggered_subscription_ids,
        vec![subscription.id.clone()]
    );
    tokio::time::sleep(Duration::from_millis(/*millis*/ 50)).await;
    assert_eq!(
        server
            .received_requests()
            .await
            .context("recorded model requests")?
            .len(),
        1
    );

    app.send_response(
        approval_id,
        serde_json::to_value(CommandExecutionRequestApprovalResponse {
            decision: CommandExecutionApprovalDecision::Decline,
        })?,
    )
    .await?;
    wait_for_requests(&server, /*expected*/ 3).await?;
    let requests = server
        .received_requests()
        .await
        .context("recorded model requests")?;
    assert_eq!(requests.len(), 3);
    let follow_up = String::from_utf8_lossy(&requests[2].body);
    assert!(follow_up.contains("<event_subscription_wake>"));
    assert!(follow_up.contains(&subscription.id));
    Ok(())
}

#[tokio::test]
async fn clock_only_subscription_wakes_without_an_external_publisher() -> Result<()> {
    let responses = vec![create_final_assistant_message_sse_response(
        "heartbeat handled",
    )?];
    let (mut app, _home, server) = event_app(responses).await?;
    let thread_id = app
        .start_thread(ThreadStartParams::default())
        .await?
        .thread
        .id;
    let first_deadline_at = current_unix_seconds().saturating_add(2);
    let created: EventSubscriptionCreateResponse = app
        .request(|request_id| ClientRequest::EventSubscriptionCreate {
            request_id,
            params: EventSubscriptionCreateParams {
                thread_id,
                filter: None,
                source_cursor: None,
                heartbeat: Some(EventSubscriptionHeartbeat {
                    interval_seconds: 60,
                    first_deadline_at: Some(first_deadline_at),
                }),
            },
        })
        .await?;

    wait_for_requests(&server, /*expected*/ 1).await?;
    let requests = server
        .received_requests()
        .await
        .context("recorded model requests")?;
    let body = String::from_utf8_lossy(&requests[0].body);
    assert!(body.contains("heartbeat"));
    assert!(body.contains(&created.subscription.id));
    Ok(())
}

async fn event_app(responses: Vec<String>) -> Result<(TestAppServer, TempDir, MockServer)> {
    let server = create_mock_responses_server_sequence(responses).await;
    let home = TempDir::new()?;
    write_event_config(home.path(), &server)?;
    let app = build_event_app(home.path()).await?;
    Ok((app, home, server))
}

fn write_event_config(path: &std::path::Path, server: &MockServer) -> Result<()> {
    Ok(MockResponsesConfig::new(&server.uri())
        .with_approval_policy("on-request")
        .with_root_config(
            "approvals_reviewer = \"user\"\nfeatures = { event_subscriptions = true }",
        )
        .write(path)?)
}

async fn build_event_app(path: &std::path::Path) -> Result<TestAppServer> {
    TestAppServer::builder()
        .with_codex_home(path)
        .without_managed_config()
        .build_initialized()
        .await
}

async fn create_subscription(
    app: &mut TestAppServer,
    thread_id: &str,
) -> Result<EventSubscriptionCreateResponse> {
    app.request(|request_id| ClientRequest::EventSubscriptionCreate {
        request_id,
        params: EventSubscriptionCreateParams {
            thread_id: thread_id.to_string(),
            filter: Some(EventSubscriptionFilter {
                source: "synthetic".to_string(),
                event_types: vec!["ready".to_string()],
                labels: BTreeMap::from([("channel".to_string(), "test".to_string())]),
            }),
            source_cursor: None,
            heartbeat: Some(EventSubscriptionHeartbeat {
                interval_seconds: 60,
                first_deadline_at: None,
            }),
        },
    })
    .await
}

async fn publish(app: &mut TestAppServer, sequence: u64) -> Result<EventPublishResponse> {
    app.request(|request_id| ClientRequest::EventPublish {
        request_id,
        params: EventPublishParams {
            event: IngressEvent {
                id: format!("event-{sequence}"),
                source: "synthetic".to_string(),
                event_type: "ready".to_string(),
                cursor: EventSourceCursor {
                    sequence,
                    value: Some(format!("cursor-{sequence}")),
                },
                labels: BTreeMap::from([("channel".to_string(), "test".to_string())]),
                occurred_at: 1_700_000_000,
            },
        },
    })
    .await
}

async fn wait_for_requests(server: &MockServer, expected: usize) -> Result<()> {
    timeout(READ_TIMEOUT, async {
        loop {
            if server
                .received_requests()
                .await
                .is_some_and(|requests| requests.len() >= expected)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await?;
    Ok(())
}

fn current_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

fn blocked_turn_response() -> Result<String> {
    #[cfg(target_os = "windows")]
    let shell_command = vec![
        "powershell".to_string(),
        "-Command".to_string(),
        "Start-Sleep -Seconds 10".to_string(),
    ];
    #[cfg(not(target_os = "windows"))]
    let shell_command = vec![
        "python3".to_string(),
        "-c".to_string(),
        "import time; time.sleep(10)".to_string(),
    ];
    create_escalated_command_execution_sse_response(
        shell_command,
        /*workdir*/ None,
        /*timeout_ms*/ Some(10_000),
        "event-subscription-blocked-command",
    )
}

async fn start_blocked_turn(app: &mut TestAppServer, thread_id: &str) -> Result<RequestId> {
    let _: TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread_id.to_string(),
                input: vec![UserInput::Text {
                    text: "start an approval-blocked turn".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    let approval = timeout(READ_TIMEOUT, app.read_stream_until_request_message()).await??;
    let ServerRequest::CommandExecutionRequestApproval { request_id, .. } = approval else {
        anyhow::bail!("active turn did not request command approval")
    };
    Ok(request_id)
}
