use crate::*;
use pretty_assertions::assert_eq;

fn detail_query(limit: u32) -> UsageDetailListQuery {
    UsageDetailListQuery {
        page: UsagePageRequest {
            cursor: None,
            limit,
        },
        time_range: None,
        thread_id: None,
        repository_id: None,
        account_profile_ref: None,
    }
}

#[test]
fn detail_inventory_names_round_trip_without_omissions() {
    assert_eq!(UsageDetailKind::ALL.len(), 15);
    for kind in UsageDetailKind::ALL {
        assert_eq!(UsageDetailKind::parse(kind.as_str()), Some(kind));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn every_approved_detail_family_is_queryable_and_operations_paginate() {
    let home = tempfile::tempdir().expect("home");
    let store = UsageStore::open(home.path()).await.expect("store");
    let process_id = ProcessId::new();
    const OS_PID_SENTINEL: u32 = 4_294_000_001;
    store
        .register_process(&process_id, OS_PID_SENTINEL, /*started_at_ms*/ 900)
        .await
        .expect("process");
    store
        .heartbeat_process(&process_id, /*occurred_at_ms*/ 950)
        .await
        .expect("process heartbeat");

    let thread_id = ThreadId::new("thread-current").expect("thread");
    let turn_id = TurnId::new("turn-current").expect("turn");
    let agent_id = AgentId::new("agent-current").expect("agent");
    let account_ref =
        AccountProfileRef::new("acct_018f47a6f2787c4aa130000000000001").expect("account ref");
    store
        .ensure_thread(&NewThread {
            id: thread_id.clone(),
            parent_thread_id: None,
            source_kind: ThreadSourceKind::new("cli").expect("source"),
            created_at_ms: 960,
        })
        .await
        .expect("thread");
    store
        .ensure_turn(&NewTurn {
            id: turn_id.clone(),
            thread_id: thread_id.clone(),
            account: AccountAttributionSnapshot::new(
                Some(account_ref.clone()),
                Some(AccountAuthMode::Chatgpt),
            ),
            created_at_ms: 970,
        })
        .await
        .expect("turn");
    store
        .ensure_agent(&NewAgent {
            id: agent_id.clone(),
            thread_id: thread_id.clone(),
            parent_agent_id: None,
            role_kind: AgentRoleKind::new("root").expect("role"),
            created_at_ms: 980,
        })
        .await
        .expect("agent");

    let repository = store
        .resolve_repository(
            &RepositoryIdentityInput::new(CanonicalRepositoryPath::new("/repo").expect("path")),
            &SafeRepositoryLabel::new("repo").expect("label"),
            /*observed_at_ms*/ 990,
        )
        .await
        .expect("repository");

    let model_operation = NewOperation {
        id: OperationId::new(),
        process_id,
        thread_id: Some(thread_id.clone()),
        turn_id: Some(turn_id.clone()),
        agent_id: Some(agent_id.clone()),
        parent_operation_id: None,
        retry_of_operation_id: None,
        rework_of_operation_id: None,
        kind: OperationKind::ModelRequest,
        started_at_ms: 1_000,
        phase: Phase::Implementation,
        activity: Activity::Coding,
        activity_state: ActivityState::ModelActive,
        attribution_provenance: AttributionProvenance::AgentDeclared,
    };
    store
        .begin_operation(&model_operation)
        .await
        .expect("model operation");
    let request_id = ModelRequestId::new();
    store
        .record_model_request(&NewModelRequest {
            id: request_id,
            operation_id: model_operation.id,
            provider_kind: ProviderKind::new("openai").expect("provider"),
            model: ModelName::new("gpt-test").expect("model"),
            transport_kind: TransportKind::new("sse").expect("transport"),
            attempt_number: 2,
            account: AccountAttributionSnapshot::new(
                Some(account_ref.clone()),
                Some(AccountAuthMode::Chatgpt),
            ),
            client_origin: ClientOrigin::new("root").expect("origin"),
        })
        .await
        .expect("model request");
    store
        .record_token_observation(&NewTokenObservation {
            id: FactEventId::new(),
            source_event_id: FactEventId::new(),
            source: TokenObservationSource::ModelRequest(request_id),
            category_path: TokenCategoryPath::new("input_tokens").expect("category"),
            token_count: Some(123),
            unit: TokenUnit::Tokens,
            measurement_provenance: MeasurementProvenance::ProviderReported,
            coverage_state: CoverageState::Complete,
            repository_bucket: RepositoryBucket::Single(repository.clone()),
            observed_at_ms: 1_040,
        })
        .await
        .expect("token");
    store
        .record_classification(&NewClassificationEvent {
            event_id: FactEventId::new(),
            operation_id: model_operation.id,
            phase: Phase::Implementation,
            activity: Activity::Coding,
            activity_state: ActivityState::ModelActive,
            provenance: AttributionProvenance::AgentDeclared,
            supersedes_event_id: None,
            occurred_at_ms: 1_010,
        })
        .await
        .expect("classification");
    store
        .record_coverage(&NewCoverageEvent {
            event_id: FactEventId::new(),
            operation_id: Some(model_operation.id),
            scope_kind: CoverageScopeKind::new("operation").expect("scope"),
            state: CoverageState::Complete,
            reason_code: None,
            occurred_at_ms: 1_045,
        })
        .await
        .expect("coverage");
    let span_id = ActivitySpanId::new();
    store
        .begin_activity_span(&NewActivitySpan {
            id: span_id,
            operation_id: model_operation.id,
            activity_state: ActivityState::ExternalWait,
            started_at_ms: 1_015,
        })
        .await
        .expect("span");
    store
        .record_activity_span_event(&NewActivitySpanEvent {
            event_id: FactEventId::new(),
            activity_span_id: span_id,
            kind: ActivitySpanEventKind::Ended,
            occurred_at_ms: 1_030,
        })
        .await
        .expect("span end");
    store
        .finish_operation(&TerminalOperation {
            operation_id: model_operation.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 1_050,
            duration_ns: 50,
            error_category: None,
        })
        .await
        .expect("model terminal");
    attribute(
        &store,
        model_operation.id,
        &repository,
        /*occurred_at_ms*/ 1_005,
    )
    .await;

    let tool_operation = NewOperation {
        id: OperationId::new(),
        started_at_ms: 1_100,
        kind: OperationKind::LocalTool,
        activity_state: ActivityState::ToolActive,
        ..model_operation.clone()
    };
    store
        .begin_operation(&tool_operation)
        .await
        .expect("tool operation");
    let tool_id = ToolInvocationId::new();
    store
        .record_tool_invocation(&NewToolInvocation {
            id: tool_id,
            operation_id: tool_operation.id,
            operation_kind: OperationKind::LocalTool,
            tool_kind: ToolKind::new("builtin").expect("kind"),
            safe_tool_name: ToolName::new("apply_patch").expect("name"),
            operation_family: OperationFamily::new("filesystem").expect("family"),
            observation_timing: ObservationTiming::new("runtime").expect("timing"),
            covering_model_request_id: None,
            execution_group_id: None,
            execution_role: ToolExecutionRole::Standalone,
        })
        .await
        .expect("tool");
    store
        .record_tool_approval(&NewToolApprovalEvent {
            event_id: FactEventId::new(),
            tool_invocation_id: tool_id,
            outcome: ApprovalOutcome::Approved,
            provenance: ApprovalProvenance::User,
            occurred_at_ms: 1_110,
        })
        .await
        .expect("approval");
    store
        .finish_operation(&TerminalOperation {
            operation_id: tool_operation.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 1_150,
            duration_ns: 50,
            error_category: None,
        })
        .await
        .expect("tool terminal");
    attribute(
        &store,
        tool_operation.id,
        &repository,
        /*occurred_at_ms*/ 1_105,
    )
    .await;

    let retry_operation = NewOperation {
        id: OperationId::new(),
        started_at_ms: 1_200,
        parent_operation_id: Some(model_operation.id),
        retry_of_operation_id: Some(model_operation.id),
        rework_of_operation_id: None,
        ..model_operation.clone()
    };
    store
        .begin_operation(&retry_operation)
        .await
        .expect("retry operation");
    let retry_request_id = ModelRequestId::new();
    store
        .record_model_request(&NewModelRequest {
            id: retry_request_id,
            operation_id: retry_operation.id,
            provider_kind: ProviderKind::new("openai").expect("provider"),
            model: ModelName::new("gpt-test").expect("model"),
            transport_kind: TransportKind::new("sse").expect("transport"),
            attempt_number: 3,
            account: AccountAttributionSnapshot::new(
                Some(account_ref.clone()),
                Some(AccountAuthMode::Chatgpt),
            ),
            client_origin: ClientOrigin::new("root").expect("origin"),
        })
        .await
        .expect("retry request");
    store
        .finish_operation(&TerminalOperation {
            operation_id: retry_operation.id,
            status: TerminalStatus::Failed,
            occurred_at_ms: 1_250,
            duration_ns: 50,
            error_category: Some(ErrorCategory::Transport),
        })
        .await
        .expect("retry terminal");
    attribute(
        &store,
        retry_operation.id,
        &repository,
        /*occurred_at_ms*/ 1_205,
    )
    .await;

    let rework_operation = NewOperation {
        id: OperationId::new(),
        started_at_ms: 1_300,
        parent_operation_id: Some(retry_operation.id),
        retry_of_operation_id: None,
        rework_of_operation_id: Some(model_operation.id),
        ..model_operation.clone()
    };
    store
        .begin_operation(&rework_operation)
        .await
        .expect("rework operation");
    let rework_request_id = ModelRequestId::new();
    store
        .record_model_request(&NewModelRequest {
            id: rework_request_id,
            operation_id: rework_operation.id,
            provider_kind: ProviderKind::new("openai").expect("provider"),
            model: ModelName::new("gpt-test").expect("model"),
            transport_kind: TransportKind::new("sse").expect("transport"),
            attempt_number: 1,
            account: AccountAttributionSnapshot::new(
                Some(account_ref.clone()),
                Some(AccountAuthMode::Chatgpt),
            ),
            client_origin: ClientOrigin::new("root").expect("origin"),
        })
        .await
        .expect("rework request");
    store
        .record_model_request_context(&NewModelRequestContext {
            model_request_id: rework_request_id,
            policy_estimated_tokens: 7,
            conversation_estimated_tokens: 8,
            tool_output_estimated_tokens: 9,
            observed_at_ms: 1_301,
        })
        .await
        .expect("rework request context");
    store
        .finish_operation(&TerminalOperation {
            operation_id: rework_operation.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 1_350,
            duration_ns: 50,
            error_category: None,
        })
        .await
        .expect("rework terminal");
    attribute(
        &store,
        rework_operation.id,
        &repository,
        /*occurred_at_ms*/ 1_305,
    )
    .await;
    let merge_target = store
        .resolve_repository(
            &RepositoryIdentityInput::new(
                CanonicalRepositoryPath::new("/repo-target").expect("target path"),
            ),
            &SafeRepositoryLabel::new("repo-target").expect("target label"),
            /*observed_at_ms*/ 1_400,
        )
        .await
        .expect("merge target");
    store
        .append_repository_alias(
            FactEventId::new(),
            &repository,
            &SafeRepositoryLabel::new("primary-repo").expect("alias"),
            /*occurred_at_ms*/ 1_410,
        )
        .await
        .expect("repository alias");
    store
        .append_repository_merge(
            FactEventId::new(),
            &repository,
            &merge_target,
            /*occurred_at_ms*/ 1_420,
        )
        .await
        .expect("repository merge");

    let mut first_query = detail_query(/*limit*/ 1);
    first_query.time_range =
        Some(UtcTimeRange::new(/*start_ms*/ 1_000, /*end_ms*/ 1_200).expect("time range"));
    first_query.thread_id = Some(thread_id.clone());
    first_query.repository_id = Some(repository.clone());
    first_query.account_profile_ref = Some(account_ref.clone());
    let first = store
        .list_details(UsageDetailKind::Operations, &first_query, |_| {
            "primary".to_string()
        })
        .await
        .expect("first operation page");
    assert_eq!(first.data.len(), 1);
    let terminal_event_id = match &first.data[0] {
        UsageDetailRecord::Operation(detail) => detail.terminal_event_id.clone(),
        other => panic!("unexpected detail: {other:?}"),
    };
    assert_eq!(
        first.data[0],
        UsageDetailRecord::Operation(Box::new(UsageOperationDetail {
            id: tool_operation.id.as_string(),
            process_id: process_id.as_string(),
            thread_id: Some(thread_id.as_str().to_string()),
            turn_id: Some(turn_id.as_str().to_string()),
            agent_id: Some(agent_id.as_str().to_string()),
            parent_operation_id: None,
            retry_of_operation_id: None,
            rework_of_operation_id: None,
            operation_kind: "local_tool".to_string(),
            started_at_ms: 1_100,
            taxonomy_version: 1,
            phase: "implementation".to_string(),
            activity: "coding".to_string(),
            activity_state: "tool_active".to_string(),
            attribution_provenance: "agent_declared".to_string(),
            account: Some("primary".to_string()),
            account_auth_mode: Some("chatgpt".to_string()),
            terminal_event_id,
            terminal_status: Some("completed".to_string()),
            completed_at_ms: Some(1_150),
            duration_ns: Some(50),
            error_category: None,
            model_request: None,
            tool: Some(UsageToolDetail {
                id: tool_id.as_string(),
                tool_kind: "builtin".to_string(),
                safe_tool_name: "apply_patch".to_string(),
                operation_family: "filesystem".to_string(),
                observation_timing: "runtime".to_string(),
                covering_model_request_id: None,
                execution_group_id: None,
                execution_role: "standalone".to_string(),
            }),
        }))
    );
    let mut second_query = first_query;
    second_query.page.cursor = first.next_cursor;
    let second = store
        .list_details(UsageDetailKind::Operations, &second_query, |_| {
            "primary".to_string()
        })
        .await
        .expect("second operation page");
    assert!(matches!(
        &second.data[..],
        [UsageDetailRecord::Operation(detail)]
            if matches!(detail.model_request, Some(UsageModelRequestDetail { attempt_number: 2, .. }))
    ));

    let linked = store
        .list_details(
            UsageDetailKind::Operations,
            &detail_query(/*limit*/ 100),
            |_| "primary".to_string(),
        )
        .await
        .expect("linked operations");
    let retry = linked
        .data
        .iter()
        .find(|record| {
            matches!(record, UsageDetailRecord::Operation(detail) if detail.id == retry_operation.id.as_string())
        })
        .expect("retry detail")
        .clone();
    let retry_terminal_event_id = match &retry {
        UsageDetailRecord::Operation(detail) => detail.terminal_event_id.clone(),
        _ => unreachable!(),
    };
    assert_eq!(
        retry,
        UsageDetailRecord::Operation(Box::new(UsageOperationDetail {
            id: retry_operation.id.as_string(),
            process_id: process_id.as_string(),
            thread_id: Some(thread_id.as_str().to_string()),
            turn_id: Some(turn_id.as_str().to_string()),
            agent_id: Some(agent_id.as_str().to_string()),
            parent_operation_id: Some(model_operation.id.as_string()),
            retry_of_operation_id: Some(model_operation.id.as_string()),
            rework_of_operation_id: None,
            operation_kind: "model_request".to_string(),
            started_at_ms: 1_200,
            taxonomy_version: 1,
            phase: "implementation".to_string(),
            activity: "coding".to_string(),
            activity_state: "model_active".to_string(),
            attribution_provenance: "agent_declared".to_string(),
            account: Some("primary".to_string()),
            account_auth_mode: Some("chatgpt".to_string()),
            terminal_event_id: retry_terminal_event_id,
            terminal_status: Some("failed".to_string()),
            completed_at_ms: Some(1_250),
            duration_ns: Some(50),
            error_category: Some("transport".to_string()),
            model_request: Some(UsageModelRequestDetail {
                id: retry_request_id.as_string(),
                provider: "openai".to_string(),
                model: "gpt-test".to_string(),
                transport: "sse".to_string(),
                attempt_number: 3,
                account: Some("primary".to_string()),
                account_auth_mode: Some("chatgpt".to_string()),
                client_origin: "root".to_string(),
                context: None,
            }),
            tool: None,
        }))
    );
    let rework = linked
        .data
        .iter()
        .find(|record| {
            matches!(record, UsageDetailRecord::Operation(detail) if detail.id == rework_operation.id.as_string())
        })
        .expect("rework detail")
        .clone();
    let rework_terminal_event_id = match &rework {
        UsageDetailRecord::Operation(detail) => detail.terminal_event_id.clone(),
        _ => unreachable!(),
    };
    assert_eq!(
        rework,
        UsageDetailRecord::Operation(Box::new(UsageOperationDetail {
            id: rework_operation.id.as_string(),
            process_id: process_id.as_string(),
            thread_id: Some(thread_id.as_str().to_string()),
            turn_id: Some(turn_id.as_str().to_string()),
            agent_id: Some(agent_id.as_str().to_string()),
            parent_operation_id: Some(retry_operation.id.as_string()),
            retry_of_operation_id: None,
            rework_of_operation_id: Some(model_operation.id.as_string()),
            operation_kind: "model_request".to_string(),
            started_at_ms: 1_300,
            taxonomy_version: 1,
            phase: "implementation".to_string(),
            activity: "coding".to_string(),
            activity_state: "model_active".to_string(),
            attribution_provenance: "agent_declared".to_string(),
            account: Some("primary".to_string()),
            account_auth_mode: Some("chatgpt".to_string()),
            terminal_event_id: rework_terminal_event_id,
            terminal_status: Some("completed".to_string()),
            completed_at_ms: Some(1_350),
            duration_ns: Some(50),
            error_category: None,
            model_request: Some(UsageModelRequestDetail {
                id: rework_request_id.as_string(),
                provider: "openai".to_string(),
                model: "gpt-test".to_string(),
                transport: "sse".to_string(),
                attempt_number: 1,
                account: Some("primary".to_string()),
                account_auth_mode: Some("chatgpt".to_string()),
                client_origin: "root".to_string(),
                context: Some(UsageModelRequestContextDetail {
                    policy_estimated_tokens: 7,
                    conversation_estimated_tokens: 8,
                    tool_output_estimated_tokens: 9,
                    estimator: MODEL_REQUEST_CONTEXT_ESTIMATOR.to_string(),
                    observed_at_ms: 1_301,
                }),
            }),
            tool: None,
        }))
    );

    let mut all_records = Vec::new();
    for kind in UsageDetailKind::ALL {
        let page = store
            .list_details(kind, &detail_query(/*limit*/ 100), |_| {
                "primary".to_string()
            })
            .await
            .unwrap_or_else(|error| panic!("{} details: {error}", kind.as_str()));
        assert!(!page.data.is_empty(), "{} details", kind.as_str());
        all_records.extend(page.data);
    }
    let encoded = serde_json::to_string(&all_records).expect("serialize details");
    assert!(encoded.contains("primary"));
    assert!(encoded.contains("primary-repo"));
    assert!(all_records.iter().any(|record| matches!(
        record,
        UsageDetailRecord::RepositoryEvent {
            event,
            repository_id,
            target_repository_id: Some(target),
            ..
        } if event == "merge"
            && repository_id == repository.as_str()
            && target == merge_target.as_str()
    )));
    assert!(!encoded.contains(account_ref.as_str()));
    assert!(!encoded.contains("/repo"));
    assert!(!encoded.contains(&OS_PID_SENTINEL.to_string()));
}

async fn attribute(
    store: &UsageStore,
    operation_id: OperationId,
    repository_id: &RepositoryId,
    occurred_at_ms: i64,
) {
    store
        .record_repository_attribution(&NewRepositoryAttribution {
            event_id: FactEventId::new(),
            operation_id,
            repository_id: Some(repository_id.clone()),
            kind: RepositoryAttributionKind::Primary,
            provenance: RepositoryAttributionProvenance::RuntimeObserved,
            occurred_at_ms,
        })
        .await
        .expect("repository attribution");
}
