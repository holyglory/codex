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
    let test = test_codex().build_with_auto_env(&server).await?;
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
async fn alarm_result_trigger_observes_the_real_tool_terminal_without_output() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
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
    let test = test_codex().build_with_auto_env(&server).await?;
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
