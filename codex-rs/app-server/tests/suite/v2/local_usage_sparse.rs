//! Exercise sparse control, durable capture and the public usage reader together.
use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server::INVALID_PARAMS_ERROR_CODE;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::LocalUsageBindingTransitionParams;
use codex_app_server_protocol::LocalUsageBindingTransitionResponse;
use codex_app_server_protocol::LocalUsageSummaryParams;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use codex_app_server_protocol::RequestId;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

use super::local_usage::fixture_fact;

#[tokio::test]
async fn local_usage_sparse_switches_survive_restart_and_do_not_scale_with_operations() -> Result<()>
{
    let home = TempDir::new()?;
    let thread = "00000000-0000-4000-8000-000000000001";
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let params = |sequence, outcome: Option<&str>| LocalUsageBindingTransitionParams {
        version: 1,
        native_project_id: outcome.map(|_| "project".into()),
        thread_id: thread.into(),
        workstream_id: outcome.map(|_| "workstream".into()),
        outcome_id: outcome.map(str::to_owned),
        transition_sequence: sequence,
        source: "project_automation".into(),
        authority: "explicit_assignment".into(),
    };
    codex_usage::capture_record(
        home.path(),
        fixture_fact("before", thread, "turn", "repo", 0, 5),
    )?;
    let first: LocalUsageBindingTransitionResponse = server
        .request(|request_id| ClientRequest::LocalUsageBindingTransition {
            request_id,
            params: params(1, Some("job-a")),
        })
        .await?;
    let encoded_before = std::fs::metadata(home.path().join("usage-v2/work-bindings.jsonl"))?.len();
    let mut records = (0..300)
        .map(|index| fixture_fact(&format!("a-{index}"), thread, "turn", "repo", 0, 1))
        .collect::<Vec<_>>();
    let mut retry = fixture_fact("retry", thread, "turn", "repo", 0, 10);
    retry.account_id = Some("failover-account".into());
    retry.attempt_number = Some(2);
    retry.retry_of_operation_id = Some("a-0".into());
    records.push(retry);
    let mut compaction = fixture_fact("compaction", thread, "compact-turn", "repo", 0, 11);
    compaction.model = Some("compaction-model".into());
    records.push(compaction);
    codex_usage::capture_records(home.path(), records)?;
    assert_eq!(
        std::fs::metadata(home.path().join("usage-v2/work-bindings.jsonl"))?.len(),
        encoded_before
    );
    // A different scope in the same repository cannot inherit the assignment.
    codex_usage::capture_record(
        home.path(),
        fixture_fact("other", "other-thread", "turn", "repo", 0, 7),
    )?;
    drop(server);
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    codex_usage::capture_record(
        home.path(),
        fixture_fact("resumed", thread, "resumed-turn", "repo", 0, 12),
    )?;
    let second: LocalUsageBindingTransitionResponse = server
        .request(|request_id| ClientRequest::LocalUsageBindingTransition {
            request_id,
            params: params(2, Some("job-b")),
        })
        .await?;
    assert!(second.effective_capture_sequence > first.effective_capture_sequence);
    codex_usage::capture_records(
        home.path(),
        vec![
            fixture_fact("b", thread, "turn-b", "repo", 0, 20),
            fixture_fact("exception", thread, "turn-b", "repo", 0, 30),
        ],
    )?;
    // A delayed retry belongs to its original operation even after a real switch.
    let mut late_retry = fixture_fact("late-retry", thread, "turn-b", "repo", 0, 13);
    late_retry.retry_of_operation_id = Some("a-0".into());
    late_retry.attempt_number = Some(2);
    let mut unknown_retry = fixture_fact("unknown-retry", thread, "turn-b", "repo", 0, 17);
    unknown_retry.retry_of_operation_id = Some("missing-predecessor".into());
    codex_usage::capture_records(home.path(), vec![late_retry, unknown_retry])?;
    let cleared: LocalUsageBindingTransitionResponse = server
        .request(|request_id| ClientRequest::LocalUsageBindingTransition {
            request_id,
            params: params(3, None),
        })
        .await?;
    let repeated: LocalUsageBindingTransitionResponse = server
        .request(|request_id| ClientRequest::LocalUsageBindingTransition {
            request_id,
            params: params(3, None),
        })
        .await?;
    assert_eq!(cleared, repeated);
    codex_usage::capture_record(
        home.path(),
        fixture_fact("cleared", thread, "turn-c", "repo", 0, 40),
    )?;
    let stale_id = server
        .send_request(
            "localUsage/bindingTransition",
            Some(serde_json::to_value(params(2, Some("stale")))?),
        )
        .await?;
    let stale = server
        .read_stream_until_error_message(RequestId::Integer(stale_id))
        .await?;
    assert_eq!(stale.error.code, INVALID_PARAMS_ERROR_CODE);
    let declaration = serde_json::from_value(
        json!({"schemaVersion":1,"operationId":"exception","capturedAtMs":0,
        "workContext":{"schemaVersion":1,"id":"exception-context","repositoryId":null,"nativeProjectId":"project",
            "threadId":thread,"turnId":"turn-b","workstreamId":"workstream","outcomeId":"exception-job","experimentRef":null}}),
    )?;
    let response: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: LocalUsageSummaryParams {
                from_at_ms: Some(0),
                to_at_ms: Some(3_000),
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
        codex_usage::TimeRange::from_millis(0, 3_000)?,
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
