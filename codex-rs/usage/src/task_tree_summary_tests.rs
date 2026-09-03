use super::*;
use crate::AccountAttributionSnapshot;
use crate::Activity;
use crate::ActivityState;
use crate::AgentId;
use crate::AgentRoleKind;
use crate::AttributionProvenance;
use crate::ClientOrigin;
use crate::CoverageState;
use crate::ErrorCategory;
use crate::FactEventId;
use crate::MeasurementProvenance;
use crate::ModelName;
use crate::ModelRequestId;
use crate::NewAgent;
use crate::NewModelRequest;
use crate::NewModelRequestContext;
use crate::NewOperation;
use crate::NewThread;
use crate::NewTokenObservation;
use crate::NewToolInvocation;
use crate::ObservationTiming;
use crate::OperationFamily;
use crate::OperationId;
use crate::OperationKind;
use crate::Phase;
use crate::ProcessId;
use crate::ProviderKind;
use crate::RepositoryBucket;
use crate::TerminalOperation;
use crate::TerminalStatus;
use crate::ThreadId;
use crate::ThreadSourceKind;
use crate::TokenCategoryPath;
use crate::TokenObservationSource;
use crate::TokenUnit;
use crate::ToolExecutionGroupId;
use crate::ToolExecutionRole;
use crate::ToolInvocationId;
use crate::ToolKind;
use crate::ToolName;
use crate::TransportKind;
use pretty_assertions::assert_eq;

fn operation(
    process_id: ProcessId,
    thread_id: &ThreadId,
    agent_id: &AgentId,
    kind: OperationKind,
    activity_state: ActivityState,
    started_at_ms: i64,
) -> NewOperation {
    NewOperation {
        id: OperationId::new(),
        process_id,
        thread_id: Some(thread_id.clone()),
        turn_id: None,
        agent_id: Some(agent_id.clone()),
        parent_operation_id: None,
        retry_of_operation_id: None,
        rework_of_operation_id: None,
        kind,
        started_at_ms,
        phase: Phase::Implementation,
        activity: Activity::Coding,
        activity_state,
        attribution_provenance: AttributionProvenance::AgentDeclared,
    }
}

async fn finish(
    store: &UsageStore,
    operation: &NewOperation,
    ended_at_ms: i64,
    status: TerminalStatus,
    error_category: Option<ErrorCategory>,
) {
    store
        .finish_operation(&TerminalOperation {
            operation_id: operation.id,
            status,
            occurred_at_ms: ended_at_ms,
            duration_ns: u64::try_from(ended_at_ms - operation.started_at_ms)
                .expect("nonnegative duration")
                * NS_PER_MS,
            error_category,
        })
        .await
        .expect("finish operation");
}

async fn record_model_request(
    store: &UsageStore,
    operation: &NewOperation,
    context: Option<(u64, u64, u64)>,
) -> ModelRequestId {
    store
        .begin_operation(operation)
        .await
        .expect("begin model operation");
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
        .expect("record model request");
    if let Some((policy, conversation, tool_output)) = context {
        store
            .record_model_request_context(&NewModelRequestContext {
                model_request_id: id,
                policy_estimated_tokens: policy,
                conversation_estimated_tokens: conversation,
                tool_output_estimated_tokens: tool_output,
                observed_at_ms: operation.started_at_ms,
            })
            .await
            .expect("record model context");
    }
    id
}

#[allow(clippy::too_many_arguments)]
async fn record_tool(
    store: &UsageStore,
    operation: &NewOperation,
    tool_kind: &str,
    tool_name: &str,
    covering_model_request_id: Option<ModelRequestId>,
    execution_group_id: Option<ToolExecutionGroupId>,
    execution_role: ToolExecutionRole,
) -> ToolInvocationId {
    store
        .begin_operation(operation)
        .await
        .expect("begin tool operation");
    let id = ToolInvocationId::new();
    store
        .record_tool_invocation(&NewToolInvocation {
            id,
            operation_id: operation.id,
            operation_kind: operation.kind,
            tool_kind: ToolKind::new(tool_kind).expect("tool kind"),
            safe_tool_name: ToolName::new(tool_name).expect("tool name"),
            operation_family: OperationFamily::new("test").expect("operation family"),
            observation_timing: ObservationTiming::new("runtime").expect("observation timing"),
            covering_model_request_id,
            execution_group_id,
            execution_role,
        })
        .await
        .expect("record tool invocation");
    id
}

async fn record_total_tokens(
    store: &UsageStore,
    source: TokenObservationSource,
    source_event_id: FactEventId,
    count: u64,
    observed_at_ms: i64,
) {
    store
        .record_token_observation(&NewTokenObservation {
            id: FactEventId::new(),
            source_event_id,
            source,
            category_path: TokenCategoryPath::new("total_tokens").expect("token category"),
            token_count: Some(count),
            unit: TokenUnit::Tokens,
            measurement_provenance: MeasurementProvenance::ProviderReported,
            coverage_state: CoverageState::Complete,
            repository_bucket: RepositoryBucket::Unknown,
            observed_at_ms,
        })
        .await
        .expect("record provider tokens");
}

async fn register_tree(store: &UsageStore) -> (ProcessId, ThreadId, ThreadId, AgentId, AgentId) {
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("register process");
    let root_thread = ThreadId::new("root-task").expect("root thread");
    let child_thread = ThreadId::new("child-task").expect("child thread");
    store
        .ensure_thread(&NewThread {
            id: root_thread.clone(),
            parent_thread_id: None,
            source_kind: ThreadSourceKind::new("root").expect("thread source"),
            created_at_ms: 900,
        })
        .await
        .expect("root thread");
    store
        .ensure_thread(&NewThread {
            id: child_thread.clone(),
            parent_thread_id: Some(root_thread.clone()),
            source_kind: ThreadSourceKind::new("subagent").expect("thread source"),
            created_at_ms: 2_000,
        })
        .await
        .expect("child thread");
    let root_agent = AgentId::new("agent-root").expect("root agent");
    let child_agent = AgentId::new("agent-child").expect("child agent");
    store
        .ensure_agent(&NewAgent {
            id: root_agent.clone(),
            thread_id: root_thread.clone(),
            parent_agent_id: None,
            role_kind: AgentRoleKind::new("root").expect("agent role"),
            created_at_ms: 900,
        })
        .await
        .expect("root agent");
    store
        .ensure_agent(&NewAgent {
            id: child_agent.clone(),
            thread_id: child_thread.clone(),
            parent_agent_id: Some(root_agent.clone()),
            role_kind: AgentRoleKind::new("worker").expect("agent role"),
            created_at_ms: 2_000,
        })
        .await
        .expect("child agent");
    (
        process_id,
        root_thread,
        child_thread,
        root_agent,
        child_agent,
    )
}

fn duration(measured_ns: u64) -> TaskTreeDuration {
    TaskTreeDuration {
        measured_ns,
        exact_ns: Some(measured_ns),
        unknown_intervals: 0,
    }
}

fn tokens(measured_tokens: i64) -> TaskTreeTokenAggregate {
    TaskTreeTokenAggregate {
        measured_tokens,
        exact_tokens: Some(measured_tokens),
        unknown_observations: 0,
    }
}

fn outcome(count: u64, measured_ns: u64) -> TaskTreeOutcomeAggregate {
    TaskTreeOutcomeAggregate {
        count,
        wall_time: duration(measured_ns),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn management_summary_deduplicates_and_preserves_factual_breakdowns() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let (process_id, root_thread, child_thread, root_agent, child_agent) =
        register_tree(&store).await;

    let model = operation(
        process_id,
        &root_thread,
        &root_agent,
        OperationKind::ModelRequest,
        ActivityState::ModelActive,
        1_000,
    );
    let request = record_model_request(&store, &model, Some((10, 20, 5))).await;
    let execution_group = ToolExecutionGroupId::from_stable_key(b"root-code-mode-execution");
    let mut wrapper = operation(
        process_id,
        &root_thread,
        &root_agent,
        OperationKind::LocalTool,
        ActivityState::ToolActive,
        1_100,
    );
    wrapper.parent_operation_id = Some(model.id);
    record_tool(
        &store,
        &wrapper,
        "builtin",
        "exec",
        None,
        Some(execution_group),
        ToolExecutionRole::Wrapper,
    )
    .await;
    let mut nested = operation(
        process_id,
        &root_thread,
        &root_agent,
        OperationKind::HostedTool,
        ActivityState::ToolActive,
        1_200,
    );
    nested.parent_operation_id = Some(model.id);
    let nested_tool = record_tool(
        &store,
        &nested,
        "hosted",
        "web_search",
        Some(request),
        Some(execution_group),
        ToolExecutionRole::Nested,
    )
    .await;
    let provider_event = FactEventId::new();
    record_total_tokens(
        &store,
        TokenObservationSource::ModelRequest(request),
        provider_event,
        100,
        1_600,
    )
    .await;
    record_total_tokens(
        &store,
        TokenObservationSource::ToolInvocation(nested_tool),
        provider_event,
        100,
        1_600,
    )
    .await;
    finish(&store, &nested, 1_500, TerminalStatus::Completed, None).await;
    finish(&store, &wrapper, 1_900, TerminalStatus::Completed, None).await;
    finish(&store, &model, 2_000, TerminalStatus::Completed, None).await;

    let intentional_wait = operation(
        process_id,
        &child_thread,
        &child_agent,
        OperationKind::LocalTool,
        ActivityState::ExternalWait,
        2_100,
    );
    record_tool(
        &store,
        &intentional_wait,
        "collaboration",
        "collaboration",
        None,
        None,
        ToolExecutionRole::Standalone,
    )
    .await;
    finish(
        &store,
        &intentional_wait,
        2_500,
        TerminalStatus::TimedOut,
        None,
    )
    .await;
    let failed_wait = operation(
        process_id,
        &child_thread,
        &child_agent,
        OperationKind::LocalTool,
        ActivityState::ExternalWait,
        2_600,
    );
    record_tool(
        &store,
        &failed_wait,
        "builtin",
        "utility",
        None,
        None,
        ToolExecutionRole::Standalone,
    )
    .await;
    finish(
        &store,
        &failed_wait,
        2_800,
        TerminalStatus::TimedOut,
        Some(ErrorCategory::Timeout),
    )
    .await;

    let mut rework_model = operation(
        process_id,
        &child_thread,
        &child_agent,
        OperationKind::ModelRequest,
        ActivityState::ModelActive,
        3_000,
    );
    rework_model.rework_of_operation_id = Some(model.id);
    let rework_request = record_model_request(&store, &rework_model, Some((3, 4, 5))).await;
    record_total_tokens(
        &store,
        TokenObservationSource::ModelRequest(rework_request),
        FactEventId::new(),
        40,
        3_400,
    )
    .await;
    let mut rework_nested = operation(
        process_id,
        &child_thread,
        &child_agent,
        OperationKind::LocalTool,
        ActivityState::ToolActive,
        3_100,
    );
    rework_nested.parent_operation_id = Some(rework_model.id);
    record_tool(
        &store,
        &rework_nested,
        "shell",
        "shell",
        None,
        None,
        ToolExecutionRole::Nested,
    )
    .await;
    finish(
        &store,
        &rework_nested,
        3_400,
        TerminalStatus::Completed,
        None,
    )
    .await;
    finish(
        &store,
        &rework_model,
        3_500,
        TerminalStatus::Completed,
        None,
    )
    .await;

    let summary = store
        .task_tree_summary(TaskTreeSummaryQuery {
            root_thread_id: root_thread.clone(),
            include_descendants: true,
            time_range: UtcTimeRange::new(900, 4_000).expect("time range"),
        })
        .await
        .expect("task tree summary")
        .expect("root exists");

    assert_eq!(
        summary.counts,
        TaskTreeCounts {
            threads: 2,
            agents: 2,
            raw_operations: 7,
            deduplicated_operations: 6,
            model_requests: 2,
            raw_tool_operations: 5,
            deduplicated_tool_operations: 4,
            wrapper_tool_operations: 1,
            nested_tool_operations: 2,
            unlinked_wrapper_tool_operations: 0,
            unlinked_nested_tool_operations: 1,
        }
    );
    assert_eq!(
        summary.totals,
        TaskTreeEffort {
            operations: 6,
            model_requests: 2,
            provider_total_tokens: tokens(140),
            wall_time: duration(2_100 * NS_PER_MS),
        }
    );
    assert_eq!(
        summary.agents,
        vec![
            TaskTreeAgentSummary {
                agent_id: Some(child_agent.as_str().to_string()),
                role: "worker".to_string(),
                operations: 4,
                model_requests: 1,
                provider_total_tokens: tokens(40),
                wall_time: duration(1_100 * NS_PER_MS),
            },
            TaskTreeAgentSummary {
                agent_id: Some(root_agent.as_str().to_string()),
                role: "root".to_string(),
                operations: 2,
                model_requests: 1,
                provider_total_tokens: tokens(100),
                wall_time: duration(1_000 * NS_PER_MS),
            },
        ]
    );
    assert_eq!(
        summary.waits,
        TaskTreeWaitSummary {
            completed: outcome(0, 0),
            intentional_expiry: outcome(1, 400 * NS_PER_MS),
            failed: outcome(1, 200 * NS_PER_MS),
            cancelled: outcome(0, 0),
            unknown: outcome(0, 0),
        }
    );
    assert_eq!(
        summary.context,
        TaskTreeContextSummary {
            estimator: MODEL_REQUEST_CONTEXT_ESTIMATOR,
            observed_requests: 2,
            unknown_requests: 0,
            sources: vec![
                TaskTreeContextSource {
                    source: "policy",
                    estimated_tokens: 13,
                },
                TaskTreeContextSource {
                    source: "conversation",
                    estimated_tokens: 24,
                },
                TaskTreeContextSource {
                    source: "tool_output",
                    estimated_tokens: 10,
                },
            ],
        }
    );
    assert_eq!(
        summary.work,
        TaskTreeWorkSummary {
            first_pass: TaskTreeEffort {
                operations: 4,
                model_requests: 1,
                provider_total_tokens: tokens(100),
                wall_time: duration(1_600 * NS_PER_MS),
            },
            post_integration_rework: TaskTreeEffort {
                operations: 2,
                model_requests: 1,
                provider_total_tokens: tokens(40),
                wall_time: duration(500 * NS_PER_MS),
            },
        }
    );
    assert_eq!(
        summary.totals.provider_total_tokens.measured_tokens,
        summary
            .agents
            .iter()
            .map(|agent| agent.provider_total_tokens.measured_tokens)
            .sum::<i64>()
    );
    assert_eq!(
        summary.totals.provider_total_tokens.measured_tokens,
        summary
            .work
            .first_pass
            .provider_total_tokens
            .measured_tokens
            + summary
                .work
                .post_integration_rework
                .provider_total_tokens
                .measured_tokens
    );

    let root_only = store
        .task_tree_summary(TaskTreeSummaryQuery {
            root_thread_id: root_thread,
            include_descendants: false,
            time_range: UtcTimeRange::new(900, 4_000).expect("time range"),
        })
        .await
        .expect("root-only summary")
        .expect("root exists");
    assert_eq!(root_only.counts.threads, 1);
    assert_eq!(root_only.counts.raw_operations, 3);
    assert_eq!(root_only.counts.deduplicated_operations, 2);
    assert_eq!(root_only.totals.provider_total_tokens, tokens(100));

    let windowed = store
        .task_tree_summary(TaskTreeSummaryQuery {
            root_thread_id: ThreadId::new("root-task").expect("root thread"),
            include_descendants: true,
            time_range: UtcTimeRange::new(1_250, 1_750).expect("time range"),
        })
        .await
        .expect("windowed summary")
        .expect("root exists");
    assert_eq!(windowed.counts.threads, 2);
    assert_eq!(windowed.counts.agents, 1);
    assert_eq!(windowed.counts.raw_operations, 3);
    assert_eq!(windowed.counts.deduplicated_operations, 2);
    assert_eq!(windowed.totals.wall_time, duration(500 * NS_PER_MS));
    assert_eq!(windowed.totals.provider_total_tokens, tokens(100));
    assert_eq!(windowed.context.observed_requests, 0);
    assert_eq!(windowed.context.unknown_requests, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn missing_historical_measurements_remain_explicitly_unknown() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let (process_id, root_thread, _, root_agent, _) = register_tree(&store).await;
    let original = operation(
        process_id,
        &root_thread,
        &root_agent,
        OperationKind::ActivityControl,
        ActivityState::ToolActive,
        600,
    );
    store
        .begin_operation(&original)
        .await
        .expect("begin original operation");
    finish(&store, &original, 700, TerminalStatus::Completed, None).await;
    let mut prior_rework = operation(
        process_id,
        &root_thread,
        &root_agent,
        OperationKind::ActivityControl,
        ActivityState::ToolActive,
        750,
    );
    prior_rework.rework_of_operation_id = Some(original.id);
    store
        .begin_operation(&prior_rework)
        .await
        .expect("begin prior rework");
    finish(&store, &prior_rework, 800, TerminalStatus::Completed, None).await;
    let mut model = operation(
        process_id,
        &root_thread,
        &root_agent,
        OperationKind::ModelRequest,
        ActivityState::ModelActive,
        1_000,
    );
    model.parent_operation_id = Some(prior_rework.id);
    model.retry_of_operation_id = Some(prior_rework.id);
    record_model_request(&store, &model, None).await;
    finish(&store, &model, 1_100, TerminalStatus::Completed, None).await;

    let summary = store
        .task_tree_summary(TaskTreeSummaryQuery {
            root_thread_id: root_thread,
            include_descendants: false,
            time_range: UtcTimeRange::new(900, 1_200).expect("time range"),
        })
        .await
        .expect("summary")
        .expect("root exists");
    assert_eq!(
        summary.totals.provider_total_tokens,
        TaskTreeTokenAggregate {
            measured_tokens: 0,
            exact_tokens: None,
            unknown_observations: 1,
        }
    );
    assert_eq!(
        summary.context,
        TaskTreeContextSummary {
            estimator: MODEL_REQUEST_CONTEXT_ESTIMATOR,
            observed_requests: 0,
            unknown_requests: 1,
            sources: vec![
                TaskTreeContextSource {
                    source: "policy",
                    estimated_tokens: 0,
                },
                TaskTreeContextSource {
                    source: "conversation",
                    estimated_tokens: 0,
                },
                TaskTreeContextSource {
                    source: "tool_output",
                    estimated_tokens: 0,
                },
            ],
        }
    );
    assert_eq!(summary.work.first_pass.operations, 0);
    assert_eq!(summary.work.post_integration_rework.operations, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn unknown_root_returns_no_summary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    assert_eq!(
        store
            .task_tree_summary(TaskTreeSummaryQuery {
                root_thread_id: ThreadId::new("missing").expect("thread id"),
                include_descendants: true,
                time_range: UtcTimeRange::new(0, 1).expect("time range"),
            })
            .await
            .expect("query"),
        None
    );
}
