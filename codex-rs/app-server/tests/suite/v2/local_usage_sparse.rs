//! Exercise sparse control, durable capture and the public usage reader together.
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server::INVALID_PARAMS_ERROR_CODE;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::LocalUsageBindingStatusParams;
use codex_app_server_protocol::LocalUsageBindingStatusResponse;
use codex_app_server_protocol::LocalUsageBindingTransitionParams;
use codex_app_server_protocol::LocalUsageBindingTransitionResponse;
use codex_app_server_protocol::LocalUsageSummaryParams;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_usage::UsageRecord;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

use super::local_usage::fixture_fact;

fn captured_pair(finish: UsageRecord) -> [UsageRecord; 2] {
    let mut start = finish.clone();
    start.id = format!("{}-started", start.operation_id.as_deref().unwrap());
    start.event = Some("model_request_started".into());
    start.observed_at_ms = start.started_at_ms;
    start.observed_at = start.started_at_ms.unwrap().div_euclid(1_000);
    start.ended_at_ms = None;
    start.ended_at = None;
    start.duration_ms = None;
    start.outcome = None;
    start.input_tokens = 0;
    start.output_tokens = 0;
    start.provider_total_tokens = None;
    [start, finish]
}

async fn invalid_request(
    server: &mut TestAppServer,
    method: &str,
    params: Value,
) -> Result<JSONRPCErrorError> {
    let request_id = server.send_request(method, Some(params)).await?;
    let result = server
        .read_stream_until_error_message(RequestId::Integer(request_id))
        .await?;
    assert_eq!(result.error.code, INVALID_PARAMS_ERROR_CODE);
    Ok(result.error)
}

#[tokio::test]
async fn local_usage_sparse_switches_survive_restart_and_do_not_scale_with_operations() -> Result<()>
{
    let home = TempDir::new()?;
    let responses = create_mock_responses_server_repeating_assistant("fixture complete").await;
    MockResponsesConfig::new(&responses.uri()).write(home.path())?;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let started = server.start_thread(ThreadStartParams::default()).await?;
    let thread = started.thread.id.as_str();
    server
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.into(),
            input: vec![UserInput::Text {
                text: "persist the fixture thread".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        })
        .await?;
    let base = chrono::Utc::now().timestamp_millis();
    let initial: LocalUsageBindingStatusResponse = server
        .request(|request_id| ClientRequest::LocalUsageBindingStatus {
            request_id,
            params: LocalUsageBindingStatusParams {
                version: None,
                thread_id: thread.into(),
            },
        })
        .await?;
    assert_eq!(
        serde_json::to_value(&initial)?,
        json!({"version":1,"revision":0,"transition":null})
    );
    invalid_request(
        &mut server,
        "localUsage/bindingStatus",
        json!({"version":2,"threadId":thread}),
    )
    .await?;
    invalid_request(
        &mut server,
        "localUsage/bindingStatus",
        json!({"threadId":"00000000-0000-4000-8000-000000000001"}),
    )
    .await?;
    let invalid_id = invalid_request(
        &mut server,
        "localUsage/bindingStatus",
        json!({"threadId":"private invalid thread"}),
    )
    .await?;
    assert!(!invalid_id.message.contains("private invalid thread"));
    let params = |expected_revision, outcome: Option<&str>| LocalUsageBindingTransitionParams {
        version: 1,
        native_project_id: outcome.map(|_| "project".into()),
        thread_id: thread.into(),
        workstream_id: outcome.map(|_| "workstream".into()),
        outcome_id: outcome.map(str::to_owned),
        expected_revision,
    };
    let mut unloaded = params(/*expected_revision*/ 0, Some("unloaded"));
    unloaded.thread_id = "00000000-0000-4000-8000-000000000001".into();
    invalid_request(
        &mut server,
        "localUsage/bindingTransition",
        serde_json::to_value(unloaded)?,
    )
    .await?;
    let mut unsupported = params(/*expected_revision*/ 0, Some("unsupported"));
    unsupported.version = 2;
    invalid_request(
        &mut server,
        "localUsage/bindingTransition",
        serde_json::to_value(unsupported)?,
    )
    .await?;
    codex_usage::capture_records(
        home.path(),
        captured_pair(fixture_fact(
            "before", thread, "turn", "repo", base, /*tokens*/ 5,
        ))
        .into(),
    )?;
    let first: LocalUsageBindingTransitionResponse = server
        .request(|request_id| ClientRequest::LocalUsageBindingTransition {
            request_id,
            params: params(/*expected_revision*/ 0, Some("job-a")),
        })
        .await?;
    assert_eq!(first.revision, 1);
    for expected_revision in [0, 1] {
        let repeated: LocalUsageBindingTransitionResponse = server
            .request(|request_id| ClientRequest::LocalUsageBindingTransition {
                request_id,
                params: params(expected_revision, Some("job-a")),
            })
            .await?;
        assert_eq!(
            repeated, first,
            "lost receipt and unchanged binding return the actual existing revision"
        );
    }
    let mut forged = serde_json::to_value(params(/*expected_revision*/ 1, Some("forged")))?;
    forged["source"] = json!("verified_user");
    forged["authority"] = json!("verified_assignment");
    invalid_request(&mut server, "localUsage/bindingTransition", forged).await?;
    let encoded_before = std::fs::metadata(home.path().join("usage-v2/work-bindings.jsonl"))?.len();
    let mut records = (0..300)
        .map(|index| {
            fixture_fact(
                &format!("a-{index}"),
                thread,
                "turn",
                "repo",
                base,
                /*tokens*/ 1,
            )
        })
        .collect::<Vec<_>>();
    let mut retry = fixture_fact("retry", thread, "turn", "repo", base, /*tokens*/ 10);
    retry.account_id = Some("failover-account".into());
    retry.attempt_number = Some(2);
    retry.retry_of_operation_id = Some("a-0".into());
    records.push(retry);
    let mut compaction = fixture_fact(
        "compaction",
        thread,
        "compact-turn",
        "repo",
        base,
        /*tokens*/ 11,
    );
    compaction.model = Some("compaction-model".into());
    let [compaction_start, compaction_finish] = captured_pair(compaction);
    let mut records = records
        .into_iter()
        .flat_map(captured_pair)
        .collect::<Vec<_>>();
    records.push(compaction_start);
    codex_usage::capture_records(home.path(), records)?;
    assert_eq!(
        std::fs::metadata(home.path().join("usage-v2/work-bindings.jsonl"))?.len(),
        encoded_before
    );
    // A different scope in the same repository cannot inherit the assignment.
    codex_usage::capture_records(
        home.path(),
        captured_pair(fixture_fact(
            "other",
            "other-thread",
            "turn",
            "repo",
            base,
            /*tokens*/ 7,
        ))
        .into(),
    )?;
    assert!(server.shutdown_gracefully().await?.success());
    drop(server);
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let resume_id = server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.into(),
            ..Default::default()
        })
        .await?;
    let resumed_thread: ThreadResumeResponse = server.read_response(resume_id).await?;
    assert_eq!(resumed_thread.thread.id, thread);
    let resumed: LocalUsageBindingStatusResponse = server
        .request(|request_id| ClientRequest::LocalUsageBindingStatus {
            request_id,
            params: LocalUsageBindingStatusParams {
                version: Some(1),
                thread_id: thread.into(),
            },
        })
        .await?;
    assert_eq!(resumed.revision, first.revision);
    assert_eq!(
        resumed.transition.as_ref().unwrap().source,
        "agent_declared"
    );
    assert_eq!(
        resumed.transition.as_ref().unwrap().authority,
        "agent_declared"
    );
    codex_usage::capture_records(
        home.path(),
        captured_pair(fixture_fact(
            "resumed",
            thread,
            "resumed-turn",
            "repo",
            base,
            /*tokens*/ 12,
        ))
        .into(),
    )?;
    let second: LocalUsageBindingTransitionResponse = server
        .request(|request_id| ClientRequest::LocalUsageBindingTransition {
            request_id,
            params: params(/*expected_revision*/ 1, Some("job-b")),
        })
        .await?;
    assert!(second.effective_capture_sequence > first.effective_capture_sequence);
    codex_usage::capture_record(home.path(), compaction_finish)?;
    codex_usage::capture_records(
        home.path(),
        vec![
            fixture_fact("b", thread, "turn-b", "repo", base, /*tokens*/ 20),
            fixture_fact(
                "exception",
                thread,
                "turn-b",
                "repo",
                base,
                /*tokens*/ 30,
            ),
        ]
        .into_iter()
        .flat_map(captured_pair)
        .collect(),
    )?;
    // A delayed retry belongs to its original operation even after a real switch.
    let mut late_retry = fixture_fact(
        "late-retry",
        thread,
        "turn-b",
        "repo",
        base,
        /*tokens*/ 13,
    );
    late_retry.retry_of_operation_id = Some("a-0".into());
    late_retry.attempt_number = Some(2);
    let mut unknown_retry = fixture_fact(
        "unknown-retry",
        thread,
        "turn-b",
        "repo",
        base,
        /*tokens*/ 17,
    );
    unknown_retry.retry_of_operation_id = Some("missing-predecessor".into());
    let mut records = captured_pair(late_retry).to_vec();
    records.push(unknown_retry); // A terminal without an observed start stays unattributed.
    codex_usage::capture_records(home.path(), records)?;
    let cleared: LocalUsageBindingTransitionResponse = server
        .request(|request_id| ClientRequest::LocalUsageBindingTransition {
            request_id,
            params: params(/*expected_revision*/ 2, None),
        })
        .await?;
    let repeated: LocalUsageBindingTransitionResponse = server
        .request(|request_id| ClientRequest::LocalUsageBindingTransition {
            request_id,
            params: params(/*expected_revision*/ 2, None),
        })
        .await?;
    assert_eq!(cleared, repeated);
    codex_usage::capture_records(
        home.path(),
        captured_pair(fixture_fact(
            "cleared", thread, "turn-c", "repo", base, /*tokens*/ 40,
        ))
        .into(),
    )?;
    invalid_request(
        &mut server,
        "localUsage/bindingTransition",
        serde_json::to_value(params(/*expected_revision*/ 1, Some("stale")))?,
    )
    .await?;
    let status: LocalUsageBindingStatusResponse = server
        .request(|request_id| ClientRequest::LocalUsageBindingStatus {
            request_id,
            params: LocalUsageBindingStatusParams {
                version: None,
                thread_id: thread.into(),
            },
        })
        .await?;
    assert_eq!(status.revision, 3);
    assert_eq!(status.transition.unwrap().native_project_id, None);
    let declaration = serde_json::from_value(
        json!({"schemaVersion":1,"operationId":"exception","capturedAtMs":base,
        "workContext":{"schemaVersion":1,"id":"exception-context","repositoryId":null,"nativeProjectId":"project",
            "threadId":thread,"turnId":"turn-b","workstreamId":"workstream","outcomeId":"exception-job","experimentRef":null}}),
    )?;
    let response: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: LocalUsageSummaryParams {
                from_at_ms: Some(base),
                to_at_ms: Some(base + 3_000),
                repository_id: Some("repo".into()),
                include_review: true,
                work_context_bindings: Some(vec![declaration]),
                ..Default::default()
            },
        })
        .await?;
    let outcomes = &response.review.as_ref().unwrap()["outcomes"];
    assert_eq!(outcomes["totals"]["providerTotalTokens"]["measured"], 465);
    assert_eq!(
        outcomes["attributed"]["providerTotalTokens"]["measured"],
        396
    );
    assert_eq!(
        outcomes["unattributed"]["providerTotalTokens"]["measured"],
        69
    );
    let actual = outcomes["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["outcomeId"].as_str().unwrap(),
                row["effort"]["providerTotalTokens"]["measured"]
                    .as_u64()
                    .unwrap(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        actual,
        std::collections::BTreeMap::from([("job-a", 346), ("job-b", 20), ("exception-job", 30)])
    );
    assert_eq!(response.report["workBindings"]["boundOperations"], 306);
    assert_eq!(response.report["workBindings"]["unknownOperations"], 4);
    assert_eq!(
        response.operation_bindings.len(),
        1,
        "native assignment must remain sparse"
    );
    let transitions = codex_usage::read_work_binding_transitions(home.path())?;
    assert_eq!(
        transitions.len(),
        3,
        "one durable event per accepted job switch"
    );
    assert_eq!(transitions.last().unwrap().update.native_project_id, None);
    let facts =
        codex_usage::UsageJournal::new(home.path().join("usage-v2/events.jsonl")).records()?;
    assert!(
        facts
            .iter()
            .all(|fact| serde_json::to_string(fact).unwrap().find("job-a").is_none())
    );
    let native = codex_usage::UsageReader::open(home.path())?.performance_review(
        codex_usage::TimeRange::from_millis(base, base + 3_000)?,
        &codex_usage::UsageFilter::default().for_repository("repo"),
    )?;
    assert_eq!(native["workBindings"]["boundOperations"], 306);
    assert_eq!(
        native["outcomes"]["attributed"]["providerTotalTokens"]["measured"],
        396
    );
    assert_eq!(
        native["outcomes"]["unattributed"]["providerTotalTokens"]["measured"],
        69
    );
    eprintln!(
        "sparse attribution fixture: 310 operations, 3 transitions, 0 operation/model metadata additions; transition bytes unchanged across 302 operations"
    );
    Ok(())
}
