use anyhow::Context;
use anyhow::Result;
use codex_event_subscriptions::EventSubscriptionStore;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::mount_function_call_agent_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::MockServer;

async fn tool_turn(
    test: &TestCodex,
    server: &MockServer,
    call_id: &str,
    tool_name: &str,
    arguments: Value,
) -> Result<[ResponsesRequest; 2]> {
    let mocks =
        mount_function_call_agent_response(server, call_id, &arguments.to_string(), tool_name)
            .await;
    test.submit_turn(&format!("Run the {call_id} check."))
        .await?;
    Ok([
        mocks.function_call.single_request(),
        mocks.completion.single_request(),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alarms_tools_persist_nudge_and_ack_only_delivery() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| config.local_control_tools_enabled = true)
        .build_with_auto_env(&server)
        .await?;
    let [_,response]=tool_turn(&test,&server,"set-alarm","alarm_set",json!({"dedupe_key":"review-window","subject":"Performance review due","summary":"Use Coordinator review.prepare and review.record; acknowledgement only handles delivery.","absolute_at_ms":1})).await?;
    let alarm: Value = serde_json::from_str(
        &response
            .function_call_output_text("set-alarm")
            .context("alarm result")?,
    )?;
    let id = alarm["id"].as_str().context("alarm id")?;
    let state = test.codex.state_db().context("persistent state")?;
    let store = state.event_subscriptions();
    let thread = test.session_configured.thread_id;
    store
        .collect_due_heartbeats(codex_core::project_automation_now_ms())
        .await?;
    let pending = store
        .pending_wake(thread)
        .await?
        .context("queued reminder")?;
    assert_eq!(pending.wake.items.len(), 1);
    let response = core_test_support::responses::mount_sse_once(
        &server,
        core_test_support::responses::sse(vec![core_test_support::responses::ev_completed(
            "reminded",
        )]),
    )
    .await;
    test.codex
        .start_event_subscription_wake_if_idle(pending.wake)
        .await?;
    core_test_support::wait_for_event(&test.codex, |event| {
        matches!(event, codex_protocol::protocol::EventMsg::TurnComplete(_))
    })
    .await;
    let request = response.single_request().body_json().to_string();
    assert!(request.contains("Performance review due"));
    assert!(request.contains("review.record"));
    assert!(request.contains(id));
    let [_, response] = tool_turn(
        &test,
        &server,
        "ack-alarm",
        "alarm_ack",
        json!({"alarm_id":id}),
    )
    .await?;
    let acknowledged: Value = serde_json::from_str(
        &response
            .function_call_output_text("ack-alarm")
            .context("ack result")?,
    )?;
    assert_eq!(acknowledged["state"], "acknowledged");
    let projects = store
        .project_status(&codex_core::project_automation_id(
            test.config.cwd.as_path(),
        ))
        .await?;
    assert!(projects.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_project_store_does_not_abort_model_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response = core_test_support::responses::mount_sse_once(
        &server,
        core_test_support::responses::sse(vec![
            core_test_support::responses::ev_response_created("response-1"),
            core_test_support::responses::ev_completed("response-1"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| config.local_control_tools_enabled = true)
        .build_with_auto_env(&server)
        .await?;
    let state = test.codex.state_db().context("persistent test state")?;
    state.close().await;

    test.submit_turn("continue even when project scheduling storage is unavailable")
        .await?;
    response.single_request();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alarm_result_trigger_observes_the_real_tool_terminal_without_output() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| config.local_control_tools_enabled = true)
        .build_with_auto_env(&server)
        .await?;
    let [_,response]=tool_turn(&test,&server,"watch-result","alarm_set",json!({"dedupe_key":"command-result","subject":"Command finished","summary":"Check the command result.","operation_result":{"operation_id":"execute-observed","tool_name":"exec_command","outcome_class":"completed"}})).await?;
    let alarm: Value = serde_json::from_str(
        &response
            .function_call_output_text("watch-result")
            .context("alarm result")?,
    )?;
    let id = alarm["id"].as_str().context("alarm id")?;
    let command = if test
        .executor_environment()
        .selection()
        .cwd
        .infer_path_convention()
        == Some(codex_utils_path_uri::PathConvention::Windows)
    {
        "Write-Output 'private-output-canary'"
    } else {
        "printf private-output-canary"
    };
    let _ = tool_turn(
        &test,
        &server,
        "execute-observed",
        "exec_command",
        json!({"cmd":command,"login":false,"yield_time_ms":10000,"max_output_tokens":100}),
    )
    .await?;
    let state = test.codex.state_db().context("persistent state")?;
    let store = state.event_subscriptions();
    let thread = test.session_configured.thread_id;
    store
        .collect_due_heartbeats(codex_core::project_automation_now_ms())
        .await?;
    let pending = store
        .pending_wake(thread)
        .await?
        .context("result triggered a reminder")?;
    assert_eq!(pending.wake.items.len(), 1);
    assert_eq!(pending.wake.items[0].subscription_id.to_string(), id);
    let encoded = serde_json::to_string(&pending.wake)?;
    assert!(!encoded.contains("private-output-canary"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alarm_pages_bound_escaped_reminders_without_losing_the_next_page() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| config.local_control_tools_enabled = true)
        .build_with_auto_env(&server)
        .await?;
    for index in 0..5 {
        let _=tool_turn(&test,&server,&format!("set-{index}"),"alarm_set",json!({"dedupe_key":format!("bounded-{index}"),"subject":"Bounded reminder","summary":"\u{0001}".repeat(512),"relative_ms":86400000})).await?;
    }
    let mut offset = 0;
    let mut ids = std::collections::BTreeSet::new();
    loop {
        let call = format!("list-{offset}");
        let [_, response] = tool_turn(
            &test,
            &server,
            &call,
            "alarm_list",
            json!({"offset":offset,"limit":5}),
        )
        .await?;
        let output = response
            .function_call_output_text(&call)
            .context("alarm page")?;
        assert!(output.len() <= 8192);
        let page: Value = serde_json::from_str(&output)?;
        for alarm in page["data"].as_array().context("alarm data")? {
            assert!(ids.insert(alarm["id"].as_str().context("alarm id")?.to_owned()));
        }
        let Some(next) = page["next_offset"].as_u64() else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(ids.len(), 5);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[test_case::test_case(true; "alarms_available")]
#[test_case::test_case(false; "alarms_unavailable")]
async fn shell_work_context_replaces_inherited_values_without_private_content(
    alarms_available: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            config.local_control_tools_enabled = true;
            for key in [
                "DEVCOORDINATOR_WORK_CONTEXT",
                "CODEX_ALARM_CONTEXT",
                "CODEX_ALARM_ACTIVATION",
            ] {
                config
                    .permissions
                    .shell_environment_policy
                    .r#set
                    .insert(key.into(), "stale-private-context".into());
            }
        })
        .build_with_auto_env(&server)
        .await?;
    let state = test.codex.state_db().context("persistent state")?;
    state
        .event_subscriptions()
        .set_alarm_delivery_available(alarms_available);
    let command = match core_test_support::test_target_os() {
        core_test_support::TestTargetOs::Windows => {
            "Write-Output $env:DEVCOORDINATOR_WORK_CONTEXT; Write-Output ('alarm=' + $env:CODEX_ALARM_CONTEXT); Write-Output ('activation=' + $env:CODEX_ALARM_ACTIVATION)"
        }
        core_test_support::TestTargetOs::Linux | core_test_support::TestTargetOs::MacOs => {
            "printf '%s\\nalarm=%s\\nactivation=%s\\n' \"$DEVCOORDINATOR_WORK_CONTEXT\" \"$CODEX_ALARM_CONTEXT\" \"$CODEX_ALARM_ACTIVATION\""
        }
    };
    let [_, response] = tool_turn(
        &test,
        &server,
        "work-context",
        "exec_command",
        json!({"cmd":command,"login":false,"yield_time_ms":10000,"max_output_tokens":2048}),
    )
    .await?;
    let output = response
        .function_call_output_text("work-context")
        .context("shell output")?;
    let output = output
        .split_once("Output:\n")
        .context("successful shell output")?
        .1;
    let mut lines = output.lines();
    let encoded = lines.next().context("work context")?;
    let mut work: Value = serde_json::from_str(encoded)?;
    assert!(encoded.len() <= 2048);
    assert!(
        work["operation_id"].is_string(),
        "current usage operation is linked"
    );
    assert!(work["turn_id"].is_string());
    work.as_object_mut()
        .context("work context object")?
        .remove("operation_id");
    work.as_object_mut()
        .context("work context object")?
        .remove("turn_id");
    assert_eq!(
        work,
        json!({
            "version":1,
            "native_project_id":codex_core::project_automation_id(&test.executor_environment().selection().cwd.to_path_buf()),
            "thread_id":test.session_configured.thread_id.to_string(),
            "workstream_id":null,"outcome_id":null,"experiment_ref":null,
        })
    );
    let alarm = lines
        .next()
        .context("alarm context")?
        .strip_prefix("alarm=")
        .context("alarm prefix")?;
    let activation = lines
        .next()
        .context("alarm activation")?
        .strip_prefix("activation=")
        .context("activation prefix")?;
    let local_alarm = alarms_available && !test.executor_environment().environment().is_remote();
    if local_alarm {
        let mut alarm: Value = serde_json::from_str(alarm)?;
        assert!(
            alarm["lease_expires_at"]
                .as_i64()
                .context("alarm lease expiry")?
                > codex_core::project_automation_now_ms()
        );
        alarm
            .as_object_mut()
            .context("alarm object")?
            .remove("lease_expires_at");
        assert_eq!(
            alarm,
            json!({"alarm_namespace":"codex.review.v1","capability_revision":1})
        );
        assert_eq!(
            std::path::Path::new(activation),
            test.config
                .codex_home
                .join("alarm-activations")
                .join(test.session_configured.thread_id.to_string())
                .as_path()
        );
    } else {
        assert_eq!((alarm, activation), ("", ""));
    }
    assert!(!output.contains("stale-private-context"));
    assert!(!encoded.contains(command));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[test_case::test_case("devcoordinator"; "legacy_name")]
#[test_case::test_case("devcoordinator2"; "current_name")]
#[test_case::test_case("third_party"; "unrelated_server")]
async fn mcp_work_context_is_scoped_and_activates_the_alarm_route(name: &str) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let mcp = start_mock_server().await;
    let endpoint = core_test_support::apps_test_server::AppsTestServer::mount(&mcp).await?;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::body_partial_json(
            json!({"method":"tools/list"}),
        ))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body)
                .expect("tools/list request body must be valid JSON");
            wiremock::ResponseTemplate::new(/*s*/ 200).set_body_json(json!({
                "jsonrpc":"2.0","id":body["id"],"result":{"tools":[{
                    "name":"inspect","description":"Inspect bounded metadata.",
                    "inputSchema":{"type":"object","properties":{}},
                    "annotations":{"readOnlyHint":true}
                }]}
            }))
        })
        .with_priority(/*priority*/ 1)
        .mount(&mcp)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::body_partial_json(
            json!({"method":"tools/call"}),
        ))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body)
                .expect("tools/call request body must be valid JSON");
            wiremock::ResponseTemplate::new(/*s*/ 200).set_body_json(json!({
                "jsonrpc":"2.0","id":body["id"],
                "result":{"content":[{"type":"text","text":"ok"}],"isError":false}
            }))
        })
        .with_priority(/*priority*/ 1)
        .mount(&mcp)
        .await;
    let mcp_config: codex_config::McpServerConfig = serde_json::from_value(json!({
        "url":format!("{}/api/codex/ps/mcp", endpoint.chatgpt_base_url),
    }))?;
    let name = name.to_owned();
    let configured_name = name.clone();
    let test = test_codex()
        .with_model_info_override("gpt-5.4", |model| model.supports_search_tool = false)
        .with_config(move |config| {
            config.local_control_tools_enabled = true;
            config
                .features
                .disable(codex_features::Feature::Mcp20260728)
                .expect("Mcp20260728 can be disabled in this fixture");
            config
                .mcp_servers
                .set(std::collections::HashMap::from([(
                    configured_name,
                    mcp_config,
                )]))
                .expect("MCP server fixture should be valid");
        })
        .build_with_auto_env(&server)
        .await?;
    core_test_support::wait_for_mcp_server(&test.codex, &name).await?;
    let state = test.codex.state_db().context("persistent state")?;
    let store = state.event_subscriptions();
    store.set_alarm_delivery_available(/*available*/ true);
    core_test_support::responses::mount_sse_sequence(
        &server,
        vec![
            core_test_support::responses::sse(vec![
                core_test_support::responses::ev_response_created("work-context-call"),
                core_test_support::responses::ev_function_call_with_namespace(
                    "coordinator-work-context",
                    &format!("mcp__{name}"),
                    "inspect",
                    "{}",
                ),
                core_test_support::responses::ev_completed("work-context-call"),
            ]),
            core_test_support::responses::sse(vec![core_test_support::responses::ev_completed(
                "work-context-complete",
            )]),
        ],
    )
    .await;
    test.submit_turn("Inspect Coordinator context.").await?;
    let calls: Vec<Value> = mcp
        .received_requests()
        .await
        .context("MCP requests")?
        .into_iter()
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter(|body| body["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 1);
    let meta = &calls[0]["params"]["_meta"]["devcoordinator/work_context"];
    let routes = store.coordinator_subscriptions().await?;
    if name == "third_party" {
        assert!(meta.is_null());
        assert!(routes.is_empty());
    } else {
        assert_eq!(
            meta["thread_id"],
            test.session_configured.thread_id.to_string()
        );
        assert!(meta["operation_id"].is_string());
        assert_eq!(meta["alarm"]["alarm_namespace"], "codex.review.v1");
        assert!(serde_json::to_string(meta)?.len() <= 2048);
        assert_eq!(routes.len(), 1);
        assert_eq!(
            routes[0]
                .filter
                .as_ref()
                .context("Coordinator route filter")?
                .labels
                .clone(),
            std::collections::BTreeMap::from([(
                "owner_thread_id".into(),
                test.session_configured.thread_id.to_string()
            )])
        );
        assert!(!serde_json::to_string(meta)?.contains("coordinator-work-context"));
    }
    Ok(())
}
