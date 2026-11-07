//! Exercises declared activity through real tool dispatch and persisted observations.

use anyhow::Context;
use anyhow::Result;
use codex_usage::UsageJournal;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn activity_call(id: &str, action: Value) -> Value {
    responses::ev_function_call(id, "usage_activity", &action.to_string())
}

fn plan_call(id: &str) -> Value {
    responses::ev_function_call(
        id,
        "update_plan",
        &json!({"plan": [{"step": "private plan text", "status": "in_progress"}]}).to_string(),
    )
}

fn receipt(request: &ResponsesRequest, id: &str) -> Result<Value> {
    let (content, _) = request
        .function_call_output_content_and_success(id)
        .context("activity output")?;
    Ok(serde_json::from_str(&content.context("activity JSON")?)?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declarations_activate_on_next_request_and_corrections_preserve_current_activity()
-> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let first = responses::mount_sse_sequence(&server, vec![
        responses::sse(vec![responses::ev_response_created("a"),
            activity_call("set", json!({"action": "set", "phase": "implementation", "activity": "coding"})),
            activity_call("invalid", json!({"action": "heartbeat", "phase": "testing", "note": "private activity note"})),
            plan_call("before"), responses::ev_completed("a")]),
        responses::sse(vec![responses::ev_response_created("b"), plan_call("active"),
            activity_call("heartbeat", json!({"action": "heartbeat"})), responses::ev_completed("b")]),
        responses::sse(vec![responses::ev_response_created("c"), responses::ev_assistant_message("done", "done"), responses::ev_completed("c")]),
    ]).await;
    let test = test_codex().build_with_auto_env(&server).await?;
    test.submit_turn("work").await?;
    let journal = UsageJournal::new(
        test.home
            .path()
            .join(codex_usage::USAGE_DIRECTORY_NAME)
            .join("events.jsonl"),
    );
    let initial = journal.records()?;
    let original = initial
        .iter()
        .find(|record| {
            record.event.as_deref() == Some("model_request_finished")
                && record.response_id.as_deref() == Some("a")
        })
        .context("original request")?
        .clone();
    let target = original.operation_id.clone().context("operation ID")?;
    let staged = receipt(&first.requests()[1], "set")?;
    assert_eq!(
        (&staged["status"], &staged["persistence"]),
        (&json!("staged"), &json!("recorded"))
    );
    assert_eq!(
        first.requests()[1]
            .function_call_output_content_and_success("invalid")
            .context("invalid declaration result")?
            .0
            .as_deref(),
        Some("usage_activity arguments must match its enum schema")
    );

    let second = responses::mount_sse_sequence(&server, vec![
        responses::sse(vec![responses::ev_response_created("d"),
            activity_call("redo", json!({"action": "set", "phase": "planning", "activity": "diagnosis", "relation": "rework_previous"})), responses::ev_completed("d")]),
        responses::sse(vec![responses::ev_response_created("e"), plan_call("redo-plan"),
            activity_call("correct", json!({"action": "correct_classification", "target_id": format!("{target}:finished"), "phase": "reporting", "activity": "verification_review"})), responses::ev_completed("e")]),
        responses::sse(vec![responses::ev_response_created("f"),
            activity_call("keep-alive", json!({"action": "heartbeat"})),
            activity_call("end", json!({"action": "end"})), responses::ev_completed("f")]),
        responses::sse(vec![responses::ev_response_created("g"), plan_call("ended"), responses::ev_completed("g")]),
        responses::sse(vec![responses::ev_response_created("h"), responses::ev_assistant_message("final", "done"), responses::ev_completed("h")]),
    ]).await;
    test.submit_turn("explicitly redo prior work").await?;
    test.codex.shutdown_and_wait().await?;
    let facts = journal.records()?;
    let requests = facts
        .iter()
        .filter(|record| record.event.as_deref() == Some("model_request_finished"))
        .collect::<Vec<_>>();
    assert_eq!(
        requests
            .iter()
            .map(|record| (
                record.response_id.as_deref(),
                record.phase.as_deref(),
                record.activity.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![
            (Some("a"), None, None),
            (Some("b"), Some("implementation"), Some("coding")),
            (Some("c"), Some("implementation"), Some("coding")),
            (Some("d"), Some("implementation"), Some("coding")),
            (Some("e"), Some("planning"), Some("diagnosis")),
            (Some("f"), Some("planning"), Some("diagnosis")),
            (Some("g"), None, None),
            (Some("h"), None, None),
        ]
    );
    assert_eq!(requests[4].rework_of_operation_id, requests[3].operation_id);
    assert!(
        requests
            .iter()
            .enumerate()
            .all(|(index, record)| index == 4 || record.rework_of_operation_id.is_none())
    );
    assert_eq!(requests[0], &original);
    let process = facts
        .iter()
        .find(|fact| fact.event.as_deref() == Some("process_observed"))
        .context("observed runtime process")?;
    let process_id = process.process_id.as_deref().context("opaque process ID")?;
    uuid::Uuid::parse_str(process_id)?;
    assert!(process.started_at_ms.is_none() && process.ended_at_ms.is_none());
    assert!(
        requests
            .iter()
            .all(|fact| fact.process_id.as_deref() == Some(process_id)
                && fact.agent_id == fact.thread_id
                && fact.taxonomy_version == Some(1))
    );
    let agent = facts
        .iter()
        .find(|fact| fact.event.as_deref() == Some("agent_started"))
        .context("observed agent session")?;
    assert_eq!(
        (&agent.agent_id, &agent.thread_id),
        (&requests[0].thread_id, &requests[0].thread_id)
    );
    assert!(
        facts
            .iter()
            .any(|fact| fact.event.as_deref() == Some("agent_stopped")
                && fact.agent_id == agent.agent_id
                && fact.started_at_ms == agent.started_at_ms
                && fact.ended_at_ms.is_some())
    );
    assert!(
        facts
            .iter()
            .any(|fact| fact.event.as_deref() == Some("taxonomy_registered")
                && fact.taxonomy_version == Some(1))
    );

    let plans = facts
        .iter()
        .filter(|record| {
            record.event.as_deref() == Some("tool_finished")
                && record.tool.as_deref() == Some("update_plan")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        plans
            .iter()
            .map(|record| record.activity.as_deref())
            .collect::<Vec<_>>(),
        vec![None, Some("coding"), Some("diagnosis"), None]
    );
    assert_eq!(plans[2].rework_of_operation_id, requests[3].operation_id);
    assert!(
        facts
            .iter()
            .any(|fact| fact.event.as_deref() == Some("activity_replaced")
                && fact.activity.as_deref() == Some("coding"))
    );
    let correction = facts
        .iter()
        .find(|record| record.event.as_deref() == Some("classification_correction"))
        .context("append-only correction")?;
    assert_eq!(
        (
            &correction.operation_id,
            correction.phase.as_deref(),
            correction.activity.as_deref(),
            &correction.thread_id
        ),
        (
            &Some(target.clone()),
            Some("reporting"),
            Some("verification_review"),
            &None
        )
    );
    let corrected = receipt(&second.requests()[2], "correct")?;
    assert_eq!(
        (
            &corrected["targetId"],
            &corrected["targetResolution"],
            &corrected["persistence"]
        ),
        (&json!(target), &json!("pending"), &json!("recorded"))
    );
    let ended = receipt(&second.requests()[3], "end")?;
    assert_eq!(
        (&ended["status"], &ended["persistence"]),
        (&json!("ended"), &json!("recorded"))
    );
    assert!(
        facts
            .iter()
            .filter(|record| record.event.as_deref().is_some_and(|event| event
                .starts_with("activity_")
                || event == "classification_correction"))
            .all(|record| record.started_at.is_none()
                && record.ended_at.is_none()
                && record.duration_ms.is_none()
                && record.input_tokens == 0
                && record.output_tokens == 0)
    );
    assert!(!serde_json::to_string(&facts)?.contains("private plan text"));
    assert!(!serde_json::to_string(&facts)?.contains("private activity note"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_declarations_do_not_stop_inference_and_replay_after_recovery() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("outage"),
                activity_call(
                    "declare",
                    json!({"action": "set", "phase": "testing", "activity": "integration_testing"}),
                ),
                responses::ev_completed("outage"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("continued"),
                responses::ev_assistant_message("continued-message", "continued"),
                responses::ev_completed("continued"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("recovered"),
                responses::ev_assistant_message("recovered-message", "recovered"),
                responses::ev_completed("recovered"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            std::fs::write(
                config.codex_home.join(codex_usage::USAGE_DIRECTORY_NAME),
                b"intentional fixture outage",
            )
            .expect("block private usage directory");
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("continue during accounting outage")
        .await?;
    let deferred = receipt(&mock.requests()[1], "declare")?;
    assert_eq!(
        (&deferred["status"], &deferred["persistence"]),
        (&json!("deferred"), &json!("pending"))
    );
    let storage = test.home.path().join(codex_usage::USAGE_DIRECTORY_NAME);
    std::fs::remove_file(&storage)?;
    test.submit_turn("continue after recovery").await?;
    test.codex.shutdown_and_wait().await?;
    let facts = UsageJournal::new(storage.join("events.jsonl")).records()?;
    assert_eq!(mock.requests().len(), 3);
    assert!(facts.iter().any(|fact| {
        fact.id
            == deferred["eventId"]
                .as_str()
                .expect("deferred observation ID")
    }));
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.event.as_deref() == Some("model_request_finished"))
            .map(|fact| (
                fact.response_id.as_deref(),
                fact.phase.as_deref(),
                fact.activity.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![
            (Some("outage"), None, None),
            (
                Some("continued"),
                Some("testing"),
                Some("integration_testing")
            ),
            (
                Some("recovered"),
                Some("testing"),
                Some("integration_testing")
            ),
        ]
    );
    assert!(facts.iter().any(
        |fact| fact.event.as_deref() == Some("activity_stopped") && fact.duration_ms.is_none()
    ));
    Ok(())
}
