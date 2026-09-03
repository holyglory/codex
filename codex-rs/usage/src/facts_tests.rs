use super::*;
use crate::AccountAttributionSnapshot;
use crate::AccountAuthMode;
use crate::AccountProfileRef;
use crate::types::ProcessId;

fn operation(process_id: ProcessId, kind: OperationKind) -> crate::types::NewOperation {
    crate::types::NewOperation {
        id: OperationId::new(),
        process_id,
        thread_id: None,
        turn_id: None,
        agent_id: None,
        parent_operation_id: None,
        retry_of_operation_id: None,
        rework_of_operation_id: None,
        kind,
        started_at_ms: 1,
        phase: Phase::Implementation,
        activity: Activity::Coding,
        activity_state: ActivityState::ModelActive,
        attribution_provenance: AttributionProvenance::AgentDeclared,
    }
}

fn model_fact(operation_id: OperationId) -> NewModelRequest {
    NewModelRequest {
        id: ModelRequestId::new(),
        operation_id,
        provider_kind: ProviderKind::new("openai").expect("provider"),
        model: ModelName::new("test-model").expect("model"),
        transport_kind: TransportKind::new("sse").expect("transport"),
        attempt_number: 1,
        account: AccountAttributionSnapshot::unknown(),
        client_origin: ClientOrigin::new("test").expect("client origin"),
    }
}

fn observation(source: TokenObservationSource) -> NewTokenObservation {
    NewTokenObservation {
        id: FactEventId::new(),
        source_event_id: FactEventId::new(),
        source,
        category_path: TokenCategoryPath::new("input_tokens").expect("category"),
        token_count: Some(1),
        unit: TokenUnit::Tokens,
        measurement_provenance: MeasurementProvenance::ProviderReported,
        coverage_state: CoverageState::Complete,
        repository_bucket: RepositoryBucket::Unknown,
        observed_at_ms: 2,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn token_combinations_local_provider_usage_and_hosted_covering_are_enforced() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 1)
        .await
        .expect("process");
    let model_operation = operation(process_id, OperationKind::ModelRequest);
    store
        .begin_operation(&model_operation)
        .await
        .expect("model operation");
    let model = model_fact(model_operation.id);
    store
        .record_model_request(&model)
        .await
        .expect("model request");
    let context = NewModelRequestContext {
        model_request_id: model.id,
        policy_estimated_tokens: 10,
        conversation_estimated_tokens: 20,
        tool_output_estimated_tokens: 30,
        observed_at_ms: 2,
    };
    store
        .record_model_request_context(&context)
        .await
        .expect("model context");
    store
        .record_model_request_context(&context)
        .await
        .expect("model context replay");
    let mut conflicting_context = context;
    conflicting_context.conversation_estimated_tokens += 1;
    assert!(matches!(
        store
            .record_model_request_context(&conflicting_context)
            .await,
        Err(UsageStoreError::FactConflict)
    ));
    let mut conflicting_account = model.clone();
    conflicting_account.account = AccountAttributionSnapshot::new(
        Some(AccountProfileRef::new("acct_other").expect("account")),
        Some(AccountAuthMode::ApiKey),
    );
    assert!(matches!(
        store.record_model_request(&conflicting_account).await,
        Err(UsageStoreError::FactConflict)
    ));

    let mut complete_null = observation(TokenObservationSource::ModelRequest(model.id));
    complete_null.token_count = None;
    assert!(matches!(
        store.record_token_observation(&complete_null).await,
        Err(UsageStoreError::InvalidFact)
    ));
    let mut unknown_value = observation(TokenObservationSource::ModelRequest(model.id));
    unknown_value.measurement_provenance = MeasurementProvenance::Unknown;
    assert!(matches!(
        store.record_token_observation(&unknown_value).await,
        Err(UsageStoreError::InvalidFact)
    ));
    let mut runtime_value = observation(TokenObservationSource::ModelRequest(model.id));
    runtime_value.measurement_provenance = MeasurementProvenance::RuntimeObserved;
    assert!(matches!(
        store.record_token_observation(&runtime_value).await,
        Err(UsageStoreError::InvalidFact)
    ));

    let local_operation = operation(process_id, OperationKind::LocalTool);
    store
        .begin_operation(&local_operation)
        .await
        .expect("local tool operation");
    let local_tool = NewToolInvocation {
        id: ToolInvocationId::new(),
        operation_id: local_operation.id,
        operation_kind: OperationKind::LocalTool,
        tool_kind: ToolKind::new("builtin").expect("kind"),
        safe_tool_name: ToolName::new("shell").expect("name"),
        operation_family: OperationFamily::new("other").expect("family"),
        observation_timing: ObservationTiming::new("runtime").expect("timing"),
        covering_model_request_id: None,
        execution_group_id: None,
        execution_role: ToolExecutionRole::Standalone,
    };
    store
        .record_tool_invocation(&local_tool)
        .await
        .expect("local tool");
    let mut invalid_group = local_tool.clone();
    invalid_group.id = ToolInvocationId::new();
    invalid_group.execution_group_id = Some(ToolExecutionGroupId::from_stable_key(b"group"));
    assert!(matches!(
        store.record_tool_invocation(&invalid_group).await,
        Err(UsageStoreError::InvalidFact)
    ));
    let mut conflicting_role = local_tool.clone();
    conflicting_role.execution_role = ToolExecutionRole::Nested;
    assert!(matches!(
        store.record_tool_invocation(&conflicting_role).await,
        Err(UsageStoreError::FactConflict)
    ));
    store
        .record_token_observation(&observation(TokenObservationSource::ToolInvocation(
            local_tool.id,
        )))
        .await
        .expect("local nested-provider token");
    let approval = NewToolApprovalEvent {
        event_id: FactEventId::new(),
        tool_invocation_id: local_tool.id,
        outcome: ApprovalOutcome::Approved,
        provenance: ApprovalProvenance::User,
        occurred_at_ms: 2,
    };
    store
        .record_tool_approval(&approval)
        .await
        .expect("approval");
    store
        .record_tool_approval(&approval)
        .await
        .expect("approval replay");
    let mut conflicting_approval = approval;
    conflicting_approval.outcome = ApprovalOutcome::Denied;
    assert!(matches!(
        store.record_tool_approval(&conflicting_approval).await,
        Err(UsageStoreError::FactConflict)
    ));

    let hosted_operation = operation(process_id, OperationKind::HostedTool);
    store
        .begin_operation(&hosted_operation)
        .await
        .expect("hosted tool operation");
    let hosted_tool = NewToolInvocation {
        id: ToolInvocationId::new(),
        operation_id: hosted_operation.id,
        operation_kind: OperationKind::HostedTool,
        tool_kind: ToolKind::new("hosted").expect("kind"),
        safe_tool_name: ToolName::new("web_search").expect("name"),
        operation_family: OperationFamily::new("network").expect("family"),
        observation_timing: ObservationTiming::new("observed_after_execution").expect("timing"),
        covering_model_request_id: Some(model.id),
        execution_group_id: None,
        execution_role: ToolExecutionRole::Standalone,
    };
    store
        .record_tool_invocation(&hosted_tool)
        .await
        .expect("hosted tool");
    store
        .record_token_observation(&observation(TokenObservationSource::ToolInvocation(
            hosted_tool.id,
        )))
        .await
        .expect("hosted tool token");

    let mut missing_cover = hosted_tool;
    missing_cover.id = ToolInvocationId::new();
    missing_cover.covering_model_request_id = None;
    assert!(matches!(
        store.record_tool_invocation(&missing_cover).await,
        Err(UsageStoreError::InvalidFact)
    ));
}
