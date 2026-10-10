use super::*;
use codex_app_server_protocol as api;
use pretty_assertions::assert_eq;

async fn policy(
    app: &mut TestAppServer,
    thread: &str,
    command: api::EventWakePolicyCommand,
) -> Result<api::EventSubscriptionWakePolicyResponse> {
    app.request(|request_id| ClientRequest::EventSubscriptionWakePolicy {
        request_id,
        params: api::EventSubscriptionWakePolicyParams {
            thread_id: thread.into(),
            command: Some(command),
            ..Default::default()
        },
    })
    .await
}

pub(super) async fn allow_subscription(
    app: &mut TestAppServer,
    thread: &str,
    subscription: &str,
) -> Result<()> {
    let current = policy(app, thread, api::EventWakePolicyCommand::Read).await?;
    policy(
        app,
        thread,
        api::EventWakePolicyCommand::Set {
            scope: api::EventWakeScope::Subscription {
                subscription_id: subscription.into(),
            },
            policy: api::EventWakePolicy::AllowBackground,
            expected_revision: current.revision,
            authorization_ref: "explicit test user request".into(),
        },
    )
    .await?;
    Ok(())
}

async fn blocked_thread(app: &mut TestAppServer) -> Result<String> {
    Ok(app.start_thread(ThreadStartParams {
        dynamic_tools: Some(vec![DynamicToolSpec::Function(DynamicToolFunctionSpec { name: "wait_for_test".into(), description: "Wait for the test client".into(), input_schema: serde_json::json!({"type":"object","properties":{},"additionalProperties":false}), defer_loading: false })]),
        ..Default::default()
    }).await?.thread.id)
}

fn blocked_response(id: &str) -> String {
    responses::sse(vec![
        responses::ev_response_created(id),
        responses::ev_function_call(id, "wait_for_test", "{}"),
        responses::ev_completed(id),
    ])
}

async fn stop(app: &mut TestAppServer, thread: &str, turn: &str) -> Result<()> {
    let _: api::TurnInterruptResponse = app
        .request(|request_id| ClientRequest::TurnInterrupt {
            request_id,
            params: api::TurnInterruptParams {
                thread_id: thread.into(),
                turn_id: turn.into(),
            },
        })
        .await?;
    let _: serde_json::Value = app.read_notification("turn/completed").await?;
    Ok(())
}

async fn resume_work(app: &mut TestAppServer, thread: &str) -> Result<()> {
    let _: TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.into(),
                input: vec![UserInput::Text {
                    text: "resume the user work".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    let _: serde_json::Value = app.read_notification("turn/completed").await?;
    Ok(())
}

#[tokio::test]
async fn stopped_alarm_waits_through_passive_resume_then_joins_user_request_once() -> Result<()> {
    let (mut app, _home, server) = event_app(vec![
        blocked_response("blocked"),
        create_final_assistant_message_sse_response("resumed")?,
        create_final_assistant_message_sse_response("resumed again")?,
    ])
    .await?;
    let thread = blocked_thread(&mut app).await?;
    let subscription = create_subscription(&mut app, &thread).await?.subscription;
    let (_, turn) = start_blocked_turn(&mut app, &thread).await?;
    stop(&mut app, &thread, &turn).await?;
    publish(&mut app, /*sequence*/ 1).await?;
    let _: api::ThreadResumeResponse = app
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: api::ThreadResumeParams {
                thread_id: thread.clone(),
                ..Default::default()
            },
        })
        .await?;
    tokio::time::sleep(Duration::from_millis(/*millis*/ 150)).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let deferred = policy(&mut app, &thread, api::EventWakePolicyCommand::Read).await?;
    assert_eq!(
        (
            deferred.running,
            deferred.pending_alarm_count,
            deferred.data
        ),
        (false, 1, Vec::new())
    );
    resume_work(&mut app, &thread).await?;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let body = String::from_utf8_lossy(&requests[1].body);
    assert!(body.contains("resume the user work"));
    assert!(body.contains(&subscription.id));
    assert_eq!(body.matches("<event_subscription_wake>").count(), 1);
    assert_eq!(
        policy(&mut app, &thread, api::EventWakePolicyCommand::Read)
            .await?
            .pending_alarm_count,
        0
    );

    // Normal completion is also quiet, and repeated observations coalesce.
    publish(&mut app, /*sequence*/ 2).await?;
    publish(&mut app, /*sequence*/ 3).await?;
    tokio::time::sleep(Duration::from_millis(/*millis*/ 150)).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_eq!(
        policy(&mut app, &thread, api::EventWakePolicyCommand::Read)
            .await?
            .pending_alarm_count,
        1
    );
    resume_work(&mut app, &thread).await?;
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    Ok(())
}

#[tokio::test]
async fn stop_suspension_survives_restart_and_a_fresh_alarm_grant_stays_scoped() -> Result<()> {
    let (mut app, home, server) = event_app(vec![
        blocked_response("initial"),
        blocked_response("scoped-alarm"),
        create_final_assistant_message_sse_response("alarm finished")?,
        create_final_assistant_message_sse_response("remaining alarm")?,
    ])
    .await?;
    let thread = blocked_thread(&mut app).await?;
    let first = create_subscription(&mut app, &thread).await?.subscription;
    let second = create_subscription(&mut app, &thread).await?.subscription;
    let current = policy(&mut app, &thread, api::EventWakePolicyCommand::Read).await?;
    policy(
        &mut app,
        &thread,
        api::EventWakePolicyCommand::Set {
            scope: api::EventWakeScope::Thread,
            policy: api::EventWakePolicy::AllowBackground,
            expected_revision: current.revision,
            authorization_ref: "user allows task wakes".into(),
        },
    )
    .await?;
    let (_, turn) = start_blocked_turn(&mut app, &thread).await?;
    stop(&mut app, &thread, &turn).await?;
    drop(app);
    let mut app = build_event_app(home.path()).await?;
    let _: api::ThreadResumeResponse = app
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: api::ThreadResumeParams {
                thread_id: thread.clone(),
                ..Default::default()
            },
        })
        .await?;
    publish(&mut app, /*sequence*/ 1).await?;
    tokio::time::sleep(Duration::from_millis(/*millis*/ 150)).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let suspended = policy(&mut app, &thread, api::EventWakePolicyCommand::Read).await?;
    assert!(suspended.data.iter().all(|entry| entry.suspended));
    allow_subscription(&mut app, &thread, &first.id).await?;
    let request = timeout(READ_TIMEOUT, app.read_stream_until_request_message()).await??;
    let ServerRequest::DynamicToolCall { request_id, .. } = request else {
        anyhow::bail!("the permitted alarm did not reach its tool");
    };
    tokio::time::sleep(Duration::from_millis(/*millis*/ 150)).await;
    let scoped = policy(&mut app, &thread, api::EventWakePolicyCommand::Read).await?;
    assert_eq!((scoped.running, scoped.pending_alarm_count), (false, 1));
    app.send_response(
        request_id,
        serde_json::to_value(DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: "release the permitted alarm".into(),
            }],
            success: true,
        })?,
    )
    .await?;
    let _: serde_json::Value = app.read_notification("turn/completed").await?;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    assert!(String::from_utf8_lossy(&requests[2].body).contains(&first.id));
    assert!(!String::from_utf8_lossy(&requests[2].body).contains(&second.id));
    let current = policy(&mut app, &thread, api::EventWakePolicyCommand::Read).await?;
    policy(
        &mut app,
        &thread,
        api::EventWakePolicyCommand::Set {
            scope: api::EventWakeScope::Thread,
            policy: api::EventWakePolicy::AllowBackground,
            expected_revision: current.revision,
            authorization_ref: "new whole-task permission".into(),
        },
    )
    .await?;
    wait_for_requests(&server, /*expected*/ 4).await?;
    assert!(
        String::from_utf8_lossy(&server.received_requests().await.unwrap()[3].body)
            .contains(&second.id)
    );
    Ok(())
}

#[tokio::test]
async fn user_steering_a_scoped_alarm_releases_other_due_work() -> Result<()> {
    let (mut app, _home, server) = event_app(vec![
        blocked_response("scoped"),
        create_final_assistant_message_sse_response("user work resumed")?,
    ])
    .await?;
    let thread = blocked_thread(&mut app).await?;
    let first = create_subscription(&mut app, &thread).await?.subscription;
    let second = create_subscription(&mut app, &thread).await?.subscription;
    allow_subscription(&mut app, &thread, &first.id).await?;
    publish(&mut app, /*sequence*/ 1).await?;
    let request = timeout(READ_TIMEOUT, app.read_stream_until_request_message()).await??;
    let ServerRequest::DynamicToolCall { request_id, params } = request else {
        anyhow::bail!("permitted alarm did not start");
    };
    let _: api::TurnSteerResponse = app
        .request(|id| ClientRequest::TurnSteer {
            request_id: id,
            params: api::TurnSteerParams {
                thread_id: thread.clone(),
                expected_turn_id: params.turn_id,
                input: vec![UserInput::Text {
                    text: "resume all my user work".into(),
                    text_elements: Vec::new(),
                }],
                client_user_message_id: None,
                responsesapi_client_metadata: None,
                additional_context: None,
            },
        })
        .await?;
    tokio::time::sleep(Duration::from_millis(/*millis*/ 150)).await;
    let running = policy(&mut app, &thread, api::EventWakePolicyCommand::Read).await?;
    assert_eq!((running.running, running.pending_alarm_count), (true, 1));
    app.send_response(
        request_id,
        serde_json::to_value(DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: "release".into(),
            }],
            success: true,
        })?,
    )
    .await?;
    let _: serde_json::Value = app.read_notification("turn/completed").await?;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let body = String::from_utf8_lossy(&requests[1].body);
    assert!(body.contains("resume all my user work"));
    assert!(body.contains(&second.id));
    Ok(())
}

#[tokio::test]
async fn alarm_queued_before_stop_survives_restart_until_user_consumes_it() -> Result<()> {
    let (mut app, home, server) = event_app(vec![
        blocked_response("blocked"),
        create_final_assistant_message_sse_response("resumed")?,
    ])
    .await?;
    let thread = blocked_thread(&mut app).await?;
    let subscription = create_subscription(&mut app, &thread).await?.subscription;
    let (_, turn) = start_blocked_turn(&mut app, &thread).await?;
    publish(&mut app, /*sequence*/ 1).await?;
    tokio::time::sleep(Duration::from_millis(/*millis*/ 250)).await;
    assert_eq!(
        policy(&mut app, &thread, api::EventWakePolicyCommand::Read)
            .await?
            .pending_alarm_count,
        1
    );
    stop(&mut app, &thread, &turn).await?;
    drop(app);
    let mut app = build_event_app(home.path()).await?;
    let _: api::ThreadResumeResponse = app
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: api::ThreadResumeParams {
                thread_id: thread.clone(),
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    resume_work(&mut app, &thread).await?;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let body = String::from_utf8_lossy(&requests[1].body);
    assert!(body.contains(&subscription.id));
    assert_eq!(body.matches("<event_subscription_wake>").count(), 1);
    assert_eq!(
        policy(&mut app, &thread, api::EventWakePolicyCommand::Read)
            .await?
            .pending_alarm_count,
        0
    );
    Ok(())
}

#[tokio::test]
async fn running_user_turn_accepts_events_after_an_earlier_alarm_was_queued() -> Result<()> {
    let (mut app, _home, server) = event_app(vec![
        blocked_response("blocked"),
        create_final_assistant_message_sse_response("both handled")?,
    ])
    .await?;
    let thread = blocked_thread(&mut app).await?;
    create_subscription(&mut app, &thread).await?;
    let (request_id, _) = start_blocked_turn(&mut app, &thread).await?;
    publish(&mut app, /*sequence*/ 1).await?;
    tokio::time::sleep(Duration::from_millis(/*millis*/ 250)).await;
    publish(&mut app, /*sequence*/ 2).await?;
    tokio::time::sleep(Duration::from_millis(/*millis*/ 250)).await;
    app.send_response(
        request_id,
        serde_json::to_value(DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: "release".into(),
            }],
            success: true,
        })?,
    )
    .await?;
    let _: serde_json::Value = app.read_notification("turn/completed").await?;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(String::from_utf8_lossy(&requests[1].body).contains(r#"\"sequence\":2"#));
    assert_eq!(
        policy(&mut app, &thread, api::EventWakePolicyCommand::Read)
            .await?
            .pending_alarm_count,
        0
    );
    Ok(())
}

#[tokio::test]
async fn refused_plan_mode_alarm_is_delivered_on_actual_user_start() -> Result<()> {
    use codex_protocol::config_types::CollaborationMode;
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::config_types::Settings;
    let (mut app, _home, server) = event_app(vec![
        create_final_assistant_message_sse_response("plan ready")?,
        create_final_assistant_message_sse_response("alarm handled")?,
    ])
    .await?;
    let thread = blocked_thread(&mut app).await?;
    let subscription = create_subscription(&mut app, &thread).await?.subscription;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.clone(),
        input: vec![UserInput::Text {
            text: "prepare a plan".into(),
            text_elements: Vec::new(),
        }],
        collaboration_mode: Some(CollaborationMode {
            mode: ModeKind::Plan,
            settings: Settings {
                model: "mock-model".into(),
                reasoning_effort: None,
                developer_instructions: None,
            },
        }),
        ..Default::default()
    })
    .await?;
    allow_subscription(&mut app, &thread, &subscription.id).await?;
    publish(&mut app, /*sequence*/ 1).await?;
    tokio::time::sleep(Duration::from_millis(/*millis*/ 250)).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        policy(&mut app, &thread, api::EventWakePolicyCommand::Read)
            .await?
            .pending_alarm_count,
        1
    );
    resume_work(&mut app, &thread).await?;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        String::from_utf8_lossy(&requests[1].body)
            .matches("<event_subscription_wake>")
            .count(),
        1
    );
    assert_eq!(
        policy(&mut app, &thread, api::EventWakePolicyCommand::Read)
            .await?
            .pending_alarm_count,
        0
    );
    Ok(())
}

#[tokio::test]
async fn permitted_alarm_does_not_resume_a_stopped_goal_but_user_work_does() -> Result<()> {
    let server = create_mock_responses_server_sequence(vec![
        blocked_response("initial"),
        create_final_assistant_message_sse_response("alarm only")?,
        create_final_assistant_message_sse_response("user resumed")?,
        blocked_response("goal-continues"),
    ])
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).with_root_config("approvals_reviewer = \"user\"\nfeatures = { event_subscriptions = true, goals = true }").write(home.path())?;
    let mut app = build_event_app(home.path()).await?;
    let thread = blocked_thread(&mut app).await?;
    let subscription = create_subscription(&mut app, &thread).await?.subscription;
    let (_, turn) = start_blocked_turn(&mut app, &thread).await?;
    let request = app.send_raw_request("thread/goal/set", Some(serde_json::json!({"threadId":thread,"objective":"finish the original user work","status":"active"}))).await?;
    let _: api::ThreadGoalSetResponse = app.read_response(request).await?;
    stop(&mut app, &thread, &turn).await?;
    drop(app);
    let mut app = build_event_app(home.path()).await?;
    let _: api::ThreadResumeResponse = app
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: api::ThreadResumeParams {
                thread_id: thread.clone(),
                ..Default::default()
            },
        })
        .await?;
    allow_subscription(&mut app, &thread, &subscription.id).await?;
    publish(&mut app, /*sequence*/ 1).await?;
    let _: serde_json::Value = app.read_notification("turn/completed").await?;
    tokio::time::sleep(Duration::from_millis(/*millis*/ 250)).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    resume_work(&mut app, &thread).await?;
    let request = timeout(READ_TIMEOUT, app.read_stream_until_request_message()).await??;
    let ServerRequest::DynamicToolCall { params, .. } = request else {
        anyhow::bail!("goal did not resume after user work");
    };
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    stop(&mut app, &thread, &params.turn_id).await?;
    Ok(())
}
