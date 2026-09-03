use super::*;
use crate::ActivitySpanEventKind;
use crate::ActivitySpanId;
use crate::NewActivitySpan;
use crate::NewActivitySpanEvent;
use crate::facts::*;
use crate::types::*;
use pretty_assertions::assert_eq;

fn operation(
    process_id: ProcessId,
    agent_id: &str,
    started_at_ms: i64,
    phase: Phase,
    activity: Activity,
    state: ActivityState,
    kind: OperationKind,
) -> NewOperation {
    NewOperation {
        id: OperationId::new(),
        process_id,
        thread_id: Some(ThreadId::new("timing-thread").expect("thread")),
        turn_id: None,
        agent_id: Some(AgentId::new(agent_id).expect("agent")),
        parent_operation_id: None,
        retry_of_operation_id: None,
        rework_of_operation_id: None,
        kind,
        started_at_ms,
        phase,
        activity,
        activity_state: state,
        attribution_provenance: AttributionProvenance::AgentDeclared,
    }
}

async fn insert_thread_and_agents(store: &UsageStore) {
    sqlx::query(
        "INSERT INTO threads(id, parent_thread_id, source_kind, created_at_ms) VALUES ('timing-thread', NULL, 'cli', 0)",
    )
    .execute(&store.pool)
    .await
    .expect("thread");
    for agent in ["agent-one", "agent-two"] {
        sqlx::query(
            "INSERT INTO agents(id, thread_id, parent_agent_id, role_kind, created_at_ms) VALUES (?, 'timing-thread', NULL, 'root', 0)",
        )
        .bind(agent)
        .execute(&store.pool)
        .await
        .expect("agent");
    }
}

async fn model_request(store: &UsageStore, operation: &NewOperation) -> ModelRequestId {
    store.begin_operation(operation).await.expect("operation");
    let id = ModelRequestId::new();
    store
        .record_model_request(&NewModelRequest {
            id,
            operation_id: operation.id,
            provider_kind: ProviderKind::new("openai").expect("provider"),
            model: ModelName::new("test-model").expect("model"),
            transport_kind: TransportKind::new("sse").expect("transport"),
            attempt_number: 1,
            account: AccountAttributionSnapshot::unknown(),
            client_origin: ClientOrigin::new("test").expect("client origin"),
        })
        .await
        .expect("model request");
    id
}

async fn finish(
    store: &UsageStore,
    operation_id: OperationId,
    ended_at_ms: i64,
    status: TerminalStatus,
) {
    store
        .finish_operation(&TerminalOperation {
            operation_id,
            status,
            occurred_at_ms: ended_at_ms,
            duration_ns: 1,
            error_category: None,
        })
        .await
        .expect("terminal");
}

async fn provider_tokens(
    store: &UsageStore,
    request_id: ModelRequestId,
    count: Option<u64>,
    observed_at_ms: i64,
) {
    for (category, token_count) in [
        ("total_tokens", count),
        ("input_tokens", count.map(|count| count / 2)),
    ] {
        store
            .record_token_observation(&NewTokenObservation {
                id: FactEventId::new(),
                source_event_id: FactEventId::new(),
                source: TokenObservationSource::ModelRequest(request_id),
                category_path: TokenCategoryPath::new(category).expect("category"),
                token_count,
                unit: TokenUnit::Tokens,
                measurement_provenance: MeasurementProvenance::ProviderReported,
                coverage_state: if token_count.is_some() {
                    CoverageState::Complete
                } else {
                    CoverageState::Unknown
                },
                repository_bucket: RepositoryBucket::Unknown,
                observed_at_ms,
            })
            .await
            .expect("tokens");
    }
}

fn named<'a>(durations: &'a [NamedDuration], name: &str) -> &'a DurationAggregate {
    &durations
        .iter()
        .find(|duration| duration.name == name)
        .expect("named duration")
        .duration
}

#[cfg(unix)]
#[tokio::test]
async fn unions_clip_overlaps_sum_agents_and_keep_missing_lifecycle_unknown() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 0)
        .await
        .expect("process");
    insert_thread_and_agents(&store).await;

    let first = operation(
        process_id,
        "agent-one",
        /*started_at_ms*/ 0,
        Phase::Planning,
        Activity::WorkPlanning,
        ActivityState::ModelActive,
        OperationKind::ModelRequest,
    );
    let first_request = model_request(&store, &first).await;
    finish(
        &store,
        first.id,
        /*ended_at_ms*/ 1_000,
        TerminalStatus::Completed,
    )
    .await;
    provider_tokens(&store, first_request, Some(10), /*observed_at_ms*/ 500).await;
    let correction = NewClassificationEvent {
        event_id: FactEventId::new(),
        operation_id: first.id,
        phase: Phase::Implementation,
        activity: Activity::Coding,
        activity_state: ActivityState::ModelActive,
        provenance: AttributionProvenance::UserCorrected,
        supersedes_event_id: None,
        occurred_at_ms: 1_001,
    };
    store
        .record_classification(&correction)
        .await
        .expect("correction");

    let tool = operation(
        process_id,
        "agent-one",
        /*started_at_ms*/ 200,
        Phase::Implementation,
        Activity::Coding,
        ActivityState::ToolActive,
        OperationKind::LocalTool,
    );
    store.begin_operation(&tool).await.expect("tool operation");
    store
        .record_tool_invocation(&NewToolInvocation {
            id: ToolInvocationId::new(),
            operation_id: tool.id,
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
    finish(
        &store,
        tool.id,
        /*ended_at_ms*/ 600,
        TerminalStatus::Failed,
    )
    .await;

    let mut retry = operation(
        process_id,
        "agent-two",
        /*started_at_ms*/ 400,
        Phase::Testing,
        Activity::IntegrationTesting,
        ActivityState::ModelActive,
        OperationKind::ModelRequest,
    );
    retry.retry_of_operation_id = Some(first.id);
    let retry_request = model_request(&store, &retry).await;
    finish(
        &store,
        retry.id,
        /*ended_at_ms*/ 1_200,
        TerminalStatus::Completed,
    )
    .await;
    provider_tokens(&store, retry_request, Some(20), /*observed_at_ms*/ 800).await;

    let mut rework = operation(
        process_id,
        "agent-two",
        /*started_at_ms*/ 1_300,
        Phase::Reporting,
        Activity::CompletionHandoff,
        ActivityState::BlockedWait,
        OperationKind::ModelRequest,
    );
    rework.rework_of_operation_id = Some(first.id);
    let rework_request = model_request(&store, &rework).await;
    provider_tokens(
        &store,
        rework_request,
        /*count*/ None,
        /*observed_at_ms*/ 1_400,
    )
    .await;

    let incomplete_tool = operation(
        process_id,
        "agent-two",
        /*started_at_ms*/ 1_500,
        Phase::Reporting,
        Activity::CompletionHandoff,
        ActivityState::ToolActive,
        OperationKind::LocalTool,
    );
    store
        .begin_operation(&incomplete_tool)
        .await
        .expect("incomplete tool operation");
    store
        .record_tool_invocation(&NewToolInvocation {
            id: ToolInvocationId::new(),
            operation_id: incomplete_tool.id,
            operation_kind: OperationKind::LocalTool,
            tool_kind: ToolKind::new("builtin").expect("kind"),
            safe_tool_name: ToolName::new("shell").expect("name"),
            operation_family: OperationFamily::new("other").expect("family"),
            observation_timing: ObservationTiming::new("runtime").expect("timing"),
            covering_model_request_id: None,
            execution_group_id: None,
            execution_role: ToolExecutionRole::Standalone,
        })
        .await
        .expect("incomplete tool");

    let range = UtcTimeRange::new(/*start_ms*/ 100, /*end_ms*/ 1_100).expect("range");
    let clipped = store
        .usage_summary_in_range(UsageSummaryScope::All, Some(range))
        .await
        .expect("clipped summary");
    assert_eq!(
        clipped.timing.request_to_delivery_wall.exact_ns,
        Some(1_000_000_000)
    );
    assert_eq!(
        clipped.timing.execution_wall_union.exact_ns,
        Some(1_000_000_000)
    );
    assert_eq!(
        named(&clipped.timing.phase_interval_unions, "implementation").exact_ns,
        Some(900_000_000)
    );
    assert_eq!(
        named(&clipped.timing.phase_interval_unions, "testing").exact_ns,
        Some(700_000_000)
    );
    assert_eq!(
        named(
            &clipped.timing.activity_state_interval_unions,
            "model_active"
        )
        .exact_ns,
        Some(1_000_000_000)
    );
    assert_eq!(
        named(
            &clipped.timing.activity_state_interval_unions,
            "tool_active"
        )
        .exact_ns,
        Some(400_000_000)
    );
    assert_eq!(
        clipped.timing.summed_per_agent_active.exact_ns,
        Some(1_600_000_000)
    );
    assert_eq!(clipped.tools.count, 1);
    assert_eq!(clipped.tools.duration.exact_ns, Some(400_000_000));
    assert_eq!(clipped.tools.outcomes[0].outcome, "failed");
    let by_activity = clipped
        .provider_tokens_by_activity
        .iter()
        .map(|tokens| tokens.measured_tokens)
        .sum::<i64>();
    let additive_tokens = clipped
        .tokens
        .iter()
        .map(|tokens| tokens.measured_tokens)
        .sum::<i64>();
    let total_tokens = clipped
        .tokens
        .iter()
        .find(|tokens| tokens.category_path == "total_tokens")
        .expect("provider total tokens")
        .measured_tokens;
    assert_eq!(by_activity, 30);
    assert_eq!(by_activity, total_tokens);
    assert_eq!(additive_tokens, 45);
    assert!(
        clipped
            .provider_tokens_by_activity
            .iter()
            .any(|tokens| tokens.activity == "coding"
                && tokens.attribution_provenance == "user_corrected")
    );
    assert!(!clipped.repository_participation.additive);

    let full = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("full summary");
    assert_eq!(full.operation_count, 5);
    assert_eq!(full.timing.execution_wall_union.exact_ns, None);
    assert_eq!(full.timing.request_to_delivery_wall.exact_ns, None);
    assert_eq!(
        named(&full.timing.phase_interval_unions, "reporting").exact_ns,
        None
    );
    assert_eq!(full.tools.count, 2);
    assert_eq!(full.tools.duration.exact_ns, None);
    assert!(
        full.tools
            .outcomes
            .iter()
            .any(|outcome| outcome.outcome == "unknown" && outcome.count == 1)
    );
    assert!(full.coverage.has_gaps);
    sqlx::query("DELETE FROM _usage_report_cache_meta")
        .execute(&store.pool)
        .await
        .expect("invalidate only the derived report cache");
    assert_eq!(
        store
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("canonical full summary"),
        full
    );
}

#[test]
fn utc_ranges_reject_empty_or_reversed_intervals() {
    assert!(UtcTimeRange::new(/*start_ms*/ 1, /*end_ms*/ 1).is_err());
    assert!(UtcTimeRange::new(/*start_ms*/ 2, /*end_ms*/ 1).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn nested_wait_spans_remain_in_wall_math_but_not_agent_active_time() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 0)
        .await
        .expect("process");
    insert_thread_and_agents(&store).await;
    let wait = operation(
        process_id,
        "agent-one",
        /*started_at_ms*/ 100,
        Phase::Planning,
        Activity::Coordination,
        ActivityState::ToolActive,
        OperationKind::LocalTool,
    );
    store.begin_operation(&wait).await.expect("wait operation");
    store
        .record_tool_invocation(&NewToolInvocation {
            id: ToolInvocationId::new(),
            operation_id: wait.id,
            operation_kind: OperationKind::LocalTool,
            tool_kind: ToolKind::new("collaboration").expect("kind"),
            safe_tool_name: ToolName::new("collaboration").expect("name"),
            operation_family: OperationFamily::new("coordination").expect("family"),
            observation_timing: ObservationTiming::new("before_execution").expect("timing"),
            covering_model_request_id: None,
            execution_group_id: None,
            execution_role: ToolExecutionRole::Standalone,
        })
        .await
        .expect("wait tool");
    let span = NewActivitySpan {
        id: ActivitySpanId::new(),
        operation_id: wait.id,
        activity_state: ActivityState::BlockedWait,
        started_at_ms: 300,
    };
    store.begin_activity_span(&span).await.expect("wait span");
    store
        .record_activity_span_event(&NewActivitySpanEvent {
            event_id: FactEventId::new(),
            activity_span_id: span.id,
            kind: ActivitySpanEventKind::Ended,
            occurred_at_ms: 900,
        })
        .await
        .expect("wait end");
    finish(
        &store,
        wait.id,
        /*ended_at_ms*/ 1_100,
        TerminalStatus::TimedOut,
    )
    .await;

    let summary = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("summary");
    assert_eq!(
        summary.timing.execution_wall_union.exact_ns,
        Some(1_000_000_000)
    );
    assert_eq!(
        named(
            &summary.timing.activity_state_interval_unions,
            "blocked_wait"
        )
        .exact_ns,
        Some(600_000_000)
    );
    assert_eq!(
        named(
            &summary.timing.activity_state_interval_unions,
            "tool_active"
        )
        .exact_ns,
        Some(400_000_000)
    );
    assert_eq!(
        summary.timing.summed_per_agent_active.exact_ns,
        Some(400_000_000)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn wall_interval_conversion_reports_overflow() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, i64::MIN)
        .await
        .expect("process");
    let operation = NewOperation {
        id: OperationId::new(),
        process_id,
        thread_id: None,
        turn_id: None,
        agent_id: None,
        parent_operation_id: None,
        retry_of_operation_id: None,
        rework_of_operation_id: None,
        kind: OperationKind::ModelRequest,
        started_at_ms: i64::MIN,
        phase: Phase::Planning,
        activity: Activity::Research,
        activity_state: ActivityState::ModelActive,
        attribution_provenance: AttributionProvenance::AgentDeclared,
    };
    model_request(&store, &operation).await;
    finish(&store, operation.id, i64::MAX, TerminalStatus::Completed).await;
    assert!(matches!(
        store.usage_summary(UsageSummaryScope::All).await,
        Err(UsageStoreError::AggregateOverflow)
    ));
}
