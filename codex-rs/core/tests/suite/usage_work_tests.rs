//! Sparse job declarations through real model dispatch, persistence, and restart.

use anyhow::Context;
use anyhow::Result;
use codex_core::UsageWorkAction;
use codex_protocol::ThreadId;
use codex_usage::UsageJournal;
use codex_usage::read_work_binding_transitions;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn output(request: &ResponsesRequest, id: &str) -> Result<Value> {
    let (content, _) = request
        .function_call_output_content_and_success(id)
        .context("tool receipt")?;
    Ok(serde_json::from_str(&content.context("JSON receipt")?)?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sparse_switches_follow_model_tools_without_per_operation_metadata() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let set = |revision, outcome| {
        json!({"action":"set", "expected_revision":revision,
        "native_project_id":"project-private-id", "workstream_id":"workstream-private-id", "outcome_id":outcome})
    };
    let plan = json!({"plan":[{"step":"private plan", "status":"in_progress"}]}).to_string();
    let mut payloads = Vec::new();
    for (index, (name, args)) in [
        ("update_plan", plan.clone()),
        ("usage_work", set(0, "outcome-private-a").to_string()),
        ("update_plan", plan.clone()),
        ("usage_work", set(1, "outcome-private-b").to_string()),
        ("update_plan", plan.clone()),
    ]
    .into_iter()
    .enumerate()
    {
        payloads.push(responses::sse(vec![
            responses::ev_response_created(&format!("response-{index}")),
            responses::ev_function_call(&format!("call-{index}"), name, &args),
            responses::ev_completed(&format!("response-{index}")),
        ]));
    }
    payloads.push(responses::sse(vec![
        responses::ev_response_created("bound-final"),
        responses::ev_assistant_message("bound-message", "done"),
        responses::ev_completed("bound-final"),
    ]));
    let first = responses::mount_sse_sequence(&server, payloads).await;
    let test = test_codex().build_with_auto_env(&server).await?;
    test.submit_turn("work on two outcomes").await?;
    let thread_id = test.session_configured.thread_id;
    let a = output(&first.requests()[2], "call-1")?;
    let b = output(&first.requests()[4], "call-3")?;
    assert_eq!((&a["revision"], &b["revision"]), (&json!(1), &json!(2)));
    let current = test
        .thread_manager
        .usage_work(thread_id, UsageWorkAction::Status)
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(serde_json::to_value(&current)?, b);
    // A lost reply and an unchanged declaration both recover the same receipt.
    for expected_revision in [1, 2] {
        assert_eq!(
            test.thread_manager
                .usage_work(
                    thread_id,
                    UsageWorkAction::Set {
                        native_project_id: "project-private-id".into(),
                        workstream_id: Some("workstream-private-id".into()),
                        outcome_id: Some("outcome-private-b".into()),
                        expected_revision,
                    }
                )
                .await
                .map_err(anyhow::Error::msg)?,
            current
        );
    }
    assert!(
        test.thread_manager
            .usage_work(
                thread_id,
                UsageWorkAction::Clear {
                    expected_revision: 1
                }
            )
            .await
            .is_err()
    );
    assert!(
        test.thread_manager
            .usage_work(ThreadId::new(), UsageWorkAction::Status)
            .await
            .is_err()
    );
    let child = test
        .thread_manager
        .start_thread(test.start_thread_options().await)
        .await?;
    let unbound = test
        .thread_manager
        .usage_work(child.thread_id, UsageWorkAction::Status)
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        serde_json::to_value(unbound)?,
        json!({"revision":0,"transition":null})
    );
    child.thread.shutdown_and_wait().await?;

    let second = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("clear-response"),
                responses::ev_function_call(
                    "clear",
                    "usage_work",
                    &json!({"action":"clear","expected_revision":2}).to_string(),
                ),
                responses::ev_completed("clear-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("cleared-plan"),
                responses::ev_function_call("cleared", "update_plan", &plan),
                responses::ev_completed("cleared-plan"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("review-response"),
                responses::ev_function_call(
                    "review",
                    "usage_stats",
                    &json!({"action":"performance_review"}).to_string(),
                ),
                responses::ev_completed("review-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("unbound-final"),
                responses::ev_assistant_message("unbound-message", "done"),
                responses::ev_completed("unbound-final"),
            ]),
        ],
    )
    .await;
    let restarted = test_codex().restart(&server, &test).await?;
    assert_eq!(
        restarted
            .thread_manager
            .usage_work(thread_id, UsageWorkAction::Status)
            .await
            .map_err(anyhow::Error::msg)?,
        current
    );
    restarted.submit_turn("clear the completed job").await?;
    restarted.codex.shutdown_and_wait().await?;
    let cleared = output(&second.requests()[1], "clear")?;
    assert_eq!(cleared["revision"], json!(3));
    assert_eq!(
        cleared["transition"]["update"]["nativeProjectId"],
        Value::Null
    );
    let switches = read_work_binding_transitions(test.home.path())?;
    assert_eq!(switches.len(), 3, "only A, B and clear create transitions");
    assert!(
        switches
            .iter()
            .all(|entry| entry.update.thread_id == thread_id.to_string()
                && entry.update.source == "agent_declared"
                && entry.update.authority == "agent_declared")
    );
    let report = output(&second.requests()[3], "review")?;
    let rows = report["outcomes"]["rows"]
        .as_array()
        .context("outcome rows")?;
    assert_eq!(
        rows.iter()
            .map(|row| (&row["outcomeId"], &row["effort"]["operations"]))
            .collect::<Vec<_>>(),
        vec![
            (&json!("outcome-private-a"), &json!(4)),
            (&json!("outcome-private-b"), &json!(5))
        ]
    );
    assert!(
        report["outcomes"]["unattributed"]["operations"]
            .as_u64()
            .context("unattributed")?
            > 0
    );
    let facts = UsageJournal::new(
        test.home
            .path()
            .join(codex_usage::USAGE_DIRECTORY_NAME)
            .join("events.jsonl"),
    )
    .records()?;
    assert!(
        facts
            .iter()
            .any(|record| record.event.as_deref() == Some("model_request_finished"))
    );
    let encoded_facts = serde_json::to_string(&facts)?;
    for id in [
        "project-private-id",
        "workstream-private-id",
        "outcome-private-a",
        "outcome-private-b",
    ] {
        assert!(
            !encoded_facts.contains(id),
            "bindings are sparse, never copied onto each fact"
        );
    }
    for request in first.requests().iter().chain(second.requests().iter()) {
        for field in ["instructions", "metadata"] {
            let encoded = request.body_json()[field].to_string();
            for id in [
                "project-private-id",
                "workstream-private-id",
                "outcome-private-a",
                "outcome-private-b",
            ] {
                assert!(!encoded.contains(id), "no work ID injected into {field}");
            }
        }
        for name in [
            "x-codex-turn-metadata",
            "x-codex-work-context",
            "x-codex-outcome-id",
            "x-codex-project-id",
        ] {
            let header = request.header(name).unwrap_or_default();
            assert!(
                !header.contains("private-"),
                "no work ID injected into headers"
            );
        }
    }
    Ok(())
}
