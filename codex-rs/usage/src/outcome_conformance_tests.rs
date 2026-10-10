use super::*;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;

// The identical fixture is also exercised by Coordinator's read-only reader.
#[tokio::test]
async fn canonical_outcome_conformance_cases() {
    let cases: Value = serde_json::from_str(include_str!("../tests/fixtures/outcomes-v1.json"))
        .expect("shared fixture");
    for case in cases["fixtures"].as_array().expect("cases") {
        let fixture = Fixture::new().await;
        let agent = AgentId::new("agent").expect("agent");
        fixture
            .store
            .ensure_agent(&NewAgent {
                id: agent.clone(),
                thread_id: ThreadId::new("spec").expect("thread"),
                parent_agent_id: None,
                role_kind: AgentRoleKind::new("test").expect("role"),
                created_at_ms: 1_000_000,
            })
            .await
            .expect("agent");
        let mut requests = HashMap::new();
        let event = FactEventId::from_stable_key(b"conformance-source");
        for input in case["operations"].as_array().expect("operations") {
            let mut operation = fixture.operation(input["start"].as_i64().expect("start"));
            operation.kind = OperationKind::ModelRequest;
            operation.activity_state = ActivityState::ModelActive;
            operation.agent_id = Some(agent.clone());
            operation.work_context =
                input["snapshot"]
                    .as_bool()
                    .expect("snapshot")
                    .then(|| OperationWorkContext {
                        native_project_id: Some("project-accounting".into()),
                        workstream_id: Some("accounting".into()),
                        outcome_id: input["outcome"].as_str().map(str::to_string),
                        experiment_ref: None,
                    });
            let request = fixture.model(&operation).await;
            let duration = input["duration"].as_u64().expect("duration");
            if input["recovered"].as_bool().unwrap_or(false) {
                fixture
                    .store
                    .recover_after_write_failure(
                        fixture.process,
                        &[operation.id],
                        operation.started_at_ms + duration as i64,
                    )
                    .await
                    .expect("recovered terminal timestamp is not measured duration");
            } else if input["terminal"].as_str() == Some("interrupted") {
                fixture
                    .store
                    .finish_operation(&TerminalOperation {
                        operation_id: operation.id,
                        status: TerminalStatus::Interrupted,
                        occurred_at_ms: operation.started_at_ms + duration as i64,
                        duration_ns: duration * 1_000_000,
                        error_category: None,
                    })
                    .await
                    .expect("measured interruption");
            } else {
                fixture.finish(&operation, duration).await;
            }
            for category in [
                "total_tokens",
                "input_tokens",
                "input_tokens_details.cached_tokens",
            ] {
                fixture
                    .store
                    .record_token_observation(&NewTokenObservation {
                        id: FactEventId::new(),
                        source_event_id: event,
                        source: TokenObservationSource::ModelRequest(request),
                        category_path: TokenCategoryPath::new(category).expect("category"),
                        token_count: input["tokens"].as_u64(),
                        unit: TokenUnit::Tokens,
                        measurement_provenance: MeasurementProvenance::ProviderReported,
                        coverage_state: CoverageState::Complete,
                        repository_bucket: RepositoryBucket::Single(fixture.repository.clone()),
                        observed_at_ms: operation.started_at_ms + 20,
                    })
                    .await
                    .expect("provider fact");
            }
            requests.insert(input["id"].as_str().expect("alias"), (operation, request));
        }
        if let Some(covered) = case.get("covered") {
            let (parent, request) = &requests[covered["owner"].as_str().expect("owner")];
            let mut operation = fixture.operation(covered["start"].as_i64().expect("start"));
            operation.kind = OperationKind::HostedTool;
            operation.agent_id = Some(agent);
            operation.work_context = parent.work_context.clone();
            let tool = fixture
                .tool(
                    &operation,
                    ToolExecutionRole::Standalone,
                    /*group*/ None,
                    Some(*request),
                )
                .await;
            fixture.finish(&operation, /*duration_ms*/ 0).await;
            fixture
                .store
                .record_token_observation(&NewTokenObservation {
                    id: FactEventId::new(),
                    source_event_id: event,
                    source: TokenObservationSource::ToolInvocation(tool),
                    category_path: TokenCategoryPath::new("total_tokens").expect("category"),
                    token_count: covered["count"].as_u64(),
                    unit: TokenUnit::Tokens,
                    measurement_provenance: MeasurementProvenance::ProviderReported,
                    coverage_state: CoverageState::Complete,
                    repository_bucket: RepositoryBucket::Single(fixture.repository.clone()),
                    observed_at_ms: operation.started_at_ms,
                })
                .await
                .expect("covered observation");
        }
        if let Some(wait) = case.get("wait") {
            let span = ActivitySpanId::new();
            fixture
                .store
                .begin_activity_span(&NewActivitySpan {
                    id: span,
                    operation_id: requests[wait["owner"].as_str().expect("owner")].0.id,
                    activity_state: ActivityState::ExternalWait,
                    started_at_ms: wait["start"].as_i64().expect("start"),
                })
                .await
                .expect("wait");
            if let Some(end) = wait["end"].as_i64() {
                fixture
                    .store
                    .record_activity_span_event(&NewActivitySpanEvent {
                        event_id: FactEventId::new(),
                        activity_span_id: span,
                        kind: ActivitySpanEventKind::Ended,
                        occurred_at_ms: end,
                    })
                    .await
                    .expect("wait ended");
            }
        }
        let report = fixture
            .store
            .performance_review_packet(PerformanceReviewQuery {
                time_range: Some(
                    UtcTimeRange::new(
                        case["window"][0].as_i64().expect("lower"),
                        case["window"][1].as_i64().expect("upper"),
                    )
                    .expect("window"),
                ),
                ..Default::default()
            })
            .await
            .expect("report")
            .outcomes;
        assert_eq!(
            json!({"coverage": report.coverage, "totals": report.totals, "attributed": report.attributed,
            "unattributed": report.unattributed, "unattributedReasons": report.unattributed_reasons, "rows": report.rows}),
            case["expected"],
            "{}",
            case["name"]
        );
    }
}
