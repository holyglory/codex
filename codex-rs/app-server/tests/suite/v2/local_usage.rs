use anyhow::Result;
use app_test_support::DEFAULT_CLIENT_NAME;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::LocalUsageActivityKind;
use codex_app_server_protocol::LocalUsageActivityListParams;
use codex_app_server_protocol::LocalUsageActivityListResponse;
use codex_app_server_protocol::LocalUsageClassificationCorrectParams;
use codex_app_server_protocol::LocalUsageClassificationCorrectResponse;
use codex_app_server_protocol::LocalUsageCoverage;
use codex_app_server_protocol::LocalUsageEventKind;
use codex_app_server_protocol::LocalUsageEventListParams;
use codex_app_server_protocol::LocalUsageEventListResponse;
use codex_app_server_protocol::LocalUsageExportCreateParams;
use codex_app_server_protocol::LocalUsageExportCreateResponse;
use codex_app_server_protocol::LocalUsageExportFormat;
use codex_app_server_protocol::LocalUsagePhase;
use codex_app_server_protocol::LocalUsageReport;
use codex_app_server_protocol::LocalUsageReportCounts;
use codex_app_server_protocol::LocalUsageReportTimeRange;
use codex_app_server_protocol::LocalUsageReportWorktree;
use codex_app_server_protocol::LocalUsageReportWorktreeCoverage;
use codex_app_server_protocol::LocalUsageRepositoryListParams;
use codex_app_server_protocol::LocalUsageRepositoryListResponse;
use codex_app_server_protocol::LocalUsageRepositoryMergeParams;
use codex_app_server_protocol::LocalUsageRepositoryMergeResponse;
use codex_app_server_protocol::LocalUsageRepositoryReadParams;
use codex_app_server_protocol::LocalUsageRepositoryReadResponse;
use codex_app_server_protocol::LocalUsageRepositoryUpdateParams;
use codex_app_server_protocol::LocalUsageRepositoryUpdateResponse;
use codex_app_server_protocol::LocalUsageSummaryParams;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use codex_app_server_protocol::LocalUsageThreadReadParams;
use codex_app_server_protocol::LocalUsageThreadReadResponse;
use codex_app_server_protocol::LocalUsageToolListParams;
use codex_app_server_protocol::LocalUsageToolListResponse;
use codex_app_server_protocol::LocalUsageUpdatedNotification;
use codex_app_server_protocol::RequestId;
use codex_usage::AccountAttributionSnapshot;
use codex_usage::AccountAuthMode;
use codex_usage::AccountProfileRef;
use codex_usage::Activity;
use codex_usage::ActivityState;
use codex_usage::AgentId;
use codex_usage::AgentRoleKind;
use codex_usage::AttributionProvenance;
use codex_usage::CanonicalRepositoryPath;
use codex_usage::ClientOrigin;
use codex_usage::CoverageScopeKind;
use codex_usage::CoverageState;
use codex_usage::FactEventId;
use codex_usage::MeasurementProvenance;
use codex_usage::ModelName;
use codex_usage::ModelRequestId;
use codex_usage::NewAgent;
use codex_usage::NewCoverageEvent;
use codex_usage::NewModelRequest;
use codex_usage::NewOperation;
use codex_usage::NewRepositoryAttribution;
use codex_usage::NewThread;
use codex_usage::NewTokenObservation;
use codex_usage::NewToolInvocation;
use codex_usage::NewTurn;
use codex_usage::ObservationTiming;
use codex_usage::OperationFamily;
use codex_usage::OperationId;
use codex_usage::OperationKind;
use codex_usage::Phase;
use codex_usage::ProcessId;
use codex_usage::ProviderKind;
use codex_usage::RepositoryAttributionKind;
use codex_usage::RepositoryAttributionProvenance;
use codex_usage::RepositoryBucket;
use codex_usage::RepositoryId;
use codex_usage::RepositoryIdentityInput;
use codex_usage::SafeRepositoryLabel;
use codex_usage::StructuredUsageSummary;
use codex_usage::TerminalOperation;
use codex_usage::TerminalStatus;
use codex_usage::ThreadId;
use codex_usage::ThreadSourceKind;
use codex_usage::TokenCategoryPath;
use codex_usage::TokenObservationSource;
use codex_usage::TokenUnit;
use codex_usage::ToolInvocationId;
use codex_usage::ToolKind;
use codex_usage::ToolName;
use codex_usage::TransportKind;
use codex_usage::TurnId;
use codex_usage::UsageStore;
use codex_usage::UsageSummaryQuery;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

const STARTED_AT_MS: i64 = 1_700_000_000_000;

struct SeededUsage {
    repository_one: RepositoryId,
    repository_two: RepositoryId,
    model_operation_id: OperationId,
}

#[tokio::test]
async fn empty_summary_is_explicitly_unobserved() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut server = initialized_server(codex_home.path()).await?;
    let response: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: empty_summary_params(),
        })
        .await?;
    assert_eq!(response.aggregate.coverage, LocalUsageCoverage::Unknown);
    assert_eq!(response.aggregate.input_tokens, None);
    assert_eq!(response.aggregate.model_requests, 0);
    assert_eq!(response.token_categories, Vec::new());
    assert_eq!(response.report.schema_version, 1);
    assert_eq!(response.report.coverage.state, "unobserved");
    assert!(response.report.coverage.has_gaps);
    Ok(())
}

#[tokio::test]
async fn populated_summary_and_drilldowns_preserve_coverage() -> Result<()> {
    let codex_home = TempDir::new()?;
    let seeded = seed_usage(codex_home.path()).await?;
    let (expected_all, expected_thread, expected_repository) =
        expected_reports(codex_home.path(), &seeded.repository_one).await?;
    let mut server = initialized_server(codex_home.path()).await?;

    let summary: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: empty_summary_params(),
        })
        .await?;
    assert_eq!(summary.aggregate.coverage, LocalUsageCoverage::Complete);
    assert_eq!(summary.aggregate.input_tokens, Some(12));
    assert_eq!(summary.aggregate.output_tokens, Some(3));
    assert_eq!(summary.aggregate.total_tokens, Some(15));
    assert_eq!(summary.aggregate.model_requests, 1);
    assert_eq!(summary.aggregate.tool_calls, 2);
    assert_report_matches_structured(&summary.report, expected_all);
    assert!(
        summary
            .report
            .provider_tokens
            .iter()
            .all(|tokens| tokens.repository_label.as_deref() == Some("repo-one"))
    );
    let encoded_report = serde_json::to_string(&summary.report)?;
    assert!(!encoded_report.contains("/private/"));
    assert!(!encoded_report.contains("@example.com"));
    assert!(!encoded_report.contains("workspace"));

    let thread: LocalUsageThreadReadResponse = server
        .request(|request_id| ClientRequest::LocalUsageThreadRead {
            request_id,
            params: LocalUsageThreadReadParams {
                thread_id: "thread-one".to_string(),
            },
        })
        .await?;
    assert_eq!(thread.thread.aggregate.input_tokens, Some(12));
    assert_eq!(thread.thread.account_id.as_deref(), Some("profile-one"));
    assert_eq!(
        thread.thread.repository_keys,
        vec![seeded.repository_one.as_str().to_string()]
    );
    assert_report_matches_structured(&thread.report, expected_thread);
    assert!(
        thread
            .report
            .account
            .as_deref()
            .is_some_and(|account| account.starts_with("removed-account-"))
    );

    let repository: LocalUsageRepositoryReadResponse = server
        .request(|request_id| ClientRequest::LocalUsageRepositoryRead {
            request_id,
            params: LocalUsageRepositoryReadParams {
                repository_key: seeded.repository_one.as_str().to_string(),
            },
        })
        .await?;
    assert_eq!(repository.repository.label, "repo-one");
    assert_eq!(repository.repository.aggregate.total_tokens, Some(15));
    assert_report_matches_structured(&repository.report, expected_repository);

    let repositories: LocalUsageRepositoryListResponse = server
        .request(|request_id| ClientRequest::LocalUsageRepositoryList {
            request_id,
            params: LocalUsageRepositoryListParams {
                cursor: None,
                limit: Some(10),
            },
        })
        .await?;
    assert_eq!(repositories.data.len(), 2);

    let tools: LocalUsageToolListResponse = server
        .request(|request_id| ClientRequest::LocalUsageToolList {
            request_id,
            params: tool_list_params(/*limit*/ Some(10), /*cursor*/ None),
        })
        .await?;
    assert_eq!(tools.data.len(), 2);
    assert!(tools.data.iter().all(|tool| tool.tool_name == "shell"));

    let activities: LocalUsageActivityListResponse = server
        .request(|request_id| ClientRequest::LocalUsageActivityList {
            request_id,
            params: activity_list_params(/*limit*/ Some(10), /*cursor*/ None),
        })
        .await?;
    assert_eq!(activities.data.len(), 3);

    let events: LocalUsageEventListResponse = server
        .request(|request_id| ClientRequest::LocalUsageEventList {
            request_id,
            params: event_list_params(/*limit*/ Some(100), /*cursor*/ None),
        })
        .await?;
    assert!(
        events
            .data
            .iter()
            .any(|event| event.kind == LocalUsageEventKind::ModelRequestStarted)
    );
    assert!(events.data.iter().all(|event| {
        event.thread_id.as_deref() == Some("thread-one")
            && !event.event_id.contains('/')
            && !event.event_id.contains("prompt")
    }));
    Ok(())
}

#[tokio::test]
async fn worktree_summary_preserves_token_ownership_filters_and_restart() -> Result<()> {
    let codex_home = TempDir::new()?;
    let seeded = seed_usage(codex_home.path()).await?;
    let store = UsageStore::open(codex_home.path()).await?;
    let mut workspaces = Vec::new();
    let mut worktrees = Vec::new();
    for label in ["checkout-one", "checkout-two"] {
        let path = codex_home.path().join(label);
        std::fs::create_dir(&path)?;
        let workspace = CanonicalRepositoryPath::new(
            std::fs::canonicalize(path)?.to_string_lossy().into_owned(),
        )?;
        let worktree = store.worktree_id_for_workspace(&seeded.repository_one, &workspace)?;
        store
            .ensure_worktree(
                &worktree,
                &seeded.repository_one,
                &SafeRepositoryLabel::new(label)?,
                STARTED_AT_MS,
            )
            .await?;
        workspaces.push(workspace);
        worktrees.push(worktree);
    }
    assert_ne!(worktrees[0], worktrees[1]);
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 43, STARTED_AT_MS)
        .await?;
    let root = ThreadId::new("thread-one")?;
    let root_turn = TurnId::new("turn-one")?;
    let root_agent = AgentId::new("agent-one")?;
    let child = ThreadId::new("thread-child")?;
    let child_turn = TurnId::new("turn-child")?;
    let child_agent = AgentId::new("agent-child")?;
    store
        .ensure_thread(&NewThread {
            id: child.clone(),
            parent_thread_id: Some(root.clone()),
            source_kind: ThreadSourceKind::new("subagent")?,
            created_at_ms: STARTED_AT_MS + 900,
        })
        .await?;
    store
        .ensure_turn(&NewTurn {
            id: child_turn.clone(),
            thread_id: child.clone(),
            account: AccountAttributionSnapshot::new(
                Some(AccountProfileRef::new("profile-one")?),
                Some(AccountAuthMode::Chatgpt),
            ),
            created_at_ms: STARTED_AT_MS + 900,
        })
        .await?;
    store
        .ensure_agent(&NewAgent {
            id: child_agent.clone(),
            thread_id: child.clone(),
            parent_agent_id: Some(root_agent.clone()),
            role_kind: AgentRoleKind::new("worker")?,
            created_at_ms: STARTED_AT_MS + 900,
        })
        .await?;
    let mut owners = vec![(seeded.model_operation_id, Some(worktrees[0].clone()))];
    // All three operations overlap the finite window. Their receipts sit at its
    // inclusive start, final millisecond, and exclusive end, respectively.
    for (thread, turn, agent, worktree, started, observed, input, output) in [
        (
            &child,
            &child_turn,
            &child_agent,
            Some(worktrees[1].clone()),
            1_000,
            2_000,
            18,
            12,
        ),
        (&root, &root_turn, &root_agent, None, 1_250, 2_999, 4, 1),
        (
            &root,
            &root_turn,
            &root_agent,
            Some(worktrees[0].clone()),
            1_500,
            3_000,
            60,
            10,
        ),
    ] {
        let op = operation(
            process_id,
            thread,
            turn,
            agent,
            OperationKind::ModelRequest,
            STARTED_AT_MS + started,
        );
        store.begin_operation(&op).await?;
        let request_id = ModelRequestId::new();
        store
            .record_model_request(&NewModelRequest {
                id: request_id,
                operation_id: op.id,
                provider_kind: ProviderKind::new("openai")?,
                model: ModelName::new("test-model")?,
                transport_kind: TransportKind::new("sse")?,
                attempt_number: 1,
                account: AccountAttributionSnapshot::new(
                    Some(AccountProfileRef::new("profile-one")?),
                    Some(AccountAuthMode::Chatgpt),
                ),
                client_origin: ClientOrigin::new("test")?,
            })
            .await?;
        let source_event_id = FactEventId::new();
        for (category, count) in [
            ("input_tokens", input),
            ("output_tokens", output),
            ("total_tokens", input + output),
        ] {
            let receipt = NewTokenObservation {
                id: FactEventId::new(),
                source_event_id,
                source: TokenObservationSource::ModelRequest(request_id),
                category_path: TokenCategoryPath::new(category)?,
                token_count: Some(count),
                unit: TokenUnit::Tokens,
                measurement_provenance: MeasurementProvenance::ProviderReported,
                coverage_state: CoverageState::Complete,
                repository_bucket: RepositoryBucket::Single(seeded.repository_one.clone()),
                observed_at_ms: STARTED_AT_MS + observed,
            };
            store.record_token_observation(&receipt).await?;
            store
                .record_token_observation(&NewTokenObservation {
                    id: FactEventId::new(),
                    ..receipt
                })
                .await?;
        }
        finish(&store, op.id, STARTED_AT_MS + 4_000).await?;
        owners.push((op.id, worktree));
    }
    let tool = operation(
        process_id,
        &child,
        &child_turn,
        &child_agent,
        OperationKind::LocalTool,
        STARTED_AT_MS + 1_600,
    );
    store.begin_operation(&tool).await?;
    let tool_id = ToolInvocationId::new();
    store
        .record_tool_invocation(&NewToolInvocation {
            id: tool_id,
            operation_id: tool.id,
            operation_kind: OperationKind::LocalTool,
            tool_kind: ToolKind::new("shell")?,
            safe_tool_name: ToolName::new("shell")?,
            operation_family: OperationFamily::new("test")?,
            observation_timing: ObservationTiming::new("before_dispatch")?,
            covering_model_request_id: None,
            execution_group_id: None,
            execution_role: codex_usage::ToolExecutionRole::Standalone,
        })
        .await?;
    let receipt = NewTokenObservation {
        id: FactEventId::new(),
        source_event_id: FactEventId::new(),
        source: TokenObservationSource::ToolInvocation(tool_id),
        category_path: TokenCategoryPath::new("total_tokens")?,
        token_count: Some(7),
        unit: TokenUnit::Tokens,
        measurement_provenance: MeasurementProvenance::ProviderReported,
        coverage_state: CoverageState::Complete,
        repository_bucket: RepositoryBucket::Single(seeded.repository_one.clone()),
        observed_at_ms: STARTED_AT_MS + 2_500,
    };
    store.record_token_observation(&receipt).await?;
    store.record_token_observation(&receipt).await?;
    finish(&store, tool.id, STARTED_AT_MS + 4_000).await?;
    owners.push((tool.id, Some(worktrees[1].clone())));
    // Replayed events and multiple attribution sources must not multiply usage.
    for (operation_id, worktree_id) in owners {
        for kind in [
            RepositoryAttributionKind::Primary,
            RepositoryAttributionKind::ObservedCwd,
        ] {
            let attribution = NewRepositoryAttribution {
                event_id: FactEventId::new(),
                operation_id,
                repository_id: Some(seeded.repository_one.clone()),
                worktree_id: worktree_id.clone(),
                kind,
                provenance: RepositoryAttributionProvenance::RuntimeObserved,
                occurred_at_ms: STARTED_AT_MS + 1,
            };
            store.record_repository_attribution(&attribution).await?;
            store.record_repository_attribution(&attribution).await?;
        }
    }
    store.close().await;

    let one = LocalUsageReportWorktree {
        worktree_key: worktrees[0].as_str().to_string(),
        repository_key: seeded.repository_one.as_str().to_string(),
        label: "checkout-one".to_string(),
        operation_count: 2,
        model_request_count: 2,
        tool_count: 0,
        total_tokens: Some(85),
        unknown_token_observations: 0,
    };
    let two = LocalUsageReportWorktree {
        worktree_key: worktrees[1].as_str().to_string(),
        repository_key: seeded.repository_one.as_str().to_string(),
        label: "checkout-two".to_string(),
        operation_count: 2,
        model_request_count: 1,
        tool_count: 1,
        total_tokens: Some(37),
        unknown_token_observations: 0,
    };
    let window_one = LocalUsageReportWorktree {
        operation_count: 1,
        model_request_count: 1,
        total_tokens: None,
        ..one.clone()
    };
    let finite = LocalUsageSummaryParams {
        from_at: Some(STARTED_AT_MS / 1_000 + 2),
        to_at: Some(STARTED_AT_MS / 1_000 + 3),
        ..empty_summary_params()
    };
    // Count arrays are [operations, model requests, tools]; coverage arrays are
    // [assigned operations, unassigned operations, assigned receipts, unassigned receipts].
    let cases = [
        (
            "all",
            empty_summary_params(),
            [7, 4, 3],
            Some(127),
            vec![one.clone(), two.clone()],
            [4, 3, 10, 3],
        ),
        (
            "empty keys",
            LocalUsageSummaryParams {
                worktree_keys: Some(vec![]),
                ..empty_summary_params()
            },
            [7, 4, 3],
            Some(127),
            vec![one.clone(), two.clone()],
            [4, 3, 10, 3],
        ),
        (
            "assigned only",
            LocalUsageSummaryParams {
                include_unassigned: Some(false),
                ..empty_summary_params()
            },
            [4, 3, 1],
            Some(122),
            vec![one.clone(), two.clone()],
            [4, 0, 10, 0],
        ),
        (
            "empty keys with explicit exclusion",
            LocalUsageSummaryParams {
                worktree_keys: Some(vec![]),
                include_unassigned: Some(false),
                ..empty_summary_params()
            },
            [4, 3, 1],
            Some(122),
            vec![one.clone(), two.clone()],
            [4, 0, 10, 0],
        ),
        (
            "first checkout",
            LocalUsageSummaryParams {
                worktree_keys: Some(vec![one.worktree_key.clone()]),
                ..empty_summary_params()
            },
            [2, 2, 0],
            Some(85),
            vec![one.clone()],
            [2, 0, 6, 0],
        ),
        (
            "second checkout",
            LocalUsageSummaryParams {
                worktree_keys: Some(vec![two.worktree_key.clone()]),
                ..empty_summary_params()
            },
            [2, 1, 1],
            Some(37),
            vec![two.clone()],
            [2, 0, 4, 0],
        ),
        (
            "duplicate keys",
            LocalUsageSummaryParams {
                worktree_keys: Some(vec![
                    two.worktree_key.clone(),
                    one.worktree_key.clone(),
                    one.worktree_key.clone(),
                ]),
                ..empty_summary_params()
            },
            [4, 3, 1],
            Some(122),
            vec![one.clone(), two.clone()],
            [4, 0, 10, 0],
        ),
        (
            "first checkout plus historical usage",
            LocalUsageSummaryParams {
                worktree_keys: Some(vec![one.worktree_key.clone()]),
                include_unassigned: Some(true),
                ..empty_summary_params()
            },
            [5, 3, 2],
            Some(90),
            vec![one.clone()],
            [2, 3, 6, 3],
        ),
        (
            "root without descendants",
            LocalUsageSummaryParams {
                thread_id: Some(root.as_str().to_string()),
                ..empty_summary_params()
            },
            [5, 3, 2],
            Some(90),
            vec![one.clone()],
            [2, 3, 6, 3],
        ),
        (
            "child scope",
            LocalUsageSummaryParams {
                thread_id: Some(child.as_str().to_string()),
                worktree_keys: Some(vec![two.worktree_key.clone()]),
                ..empty_summary_params()
            },
            [2, 1, 1],
            Some(37),
            vec![two.clone()],
            [2, 0, 4, 0],
        ),
        (
            "finite window",
            finite.clone(),
            [4, 3, 1],
            Some(42),
            vec![window_one.clone(), two.clone()],
            [3, 1, 4, 3],
        ),
        (
            "finite first checkout",
            LocalUsageSummaryParams {
                worktree_keys: Some(vec![one.worktree_key.clone()]),
                ..finite.clone()
            },
            [1, 1, 0],
            None,
            vec![window_one.clone()],
            [1, 0, 0, 0],
        ),
        (
            "finite second checkout",
            LocalUsageSummaryParams {
                worktree_keys: Some(vec![two.worktree_key.clone()]),
                ..finite.clone()
            },
            [2, 1, 1],
            Some(37),
            vec![two.clone()],
            [2, 0, 4, 0],
        ),
        (
            "repository intersection",
            LocalUsageSummaryParams {
                repository_key: Some(seeded.repository_two.as_str().to_string()),
                worktree_keys: Some(vec![one.worktree_key.clone(), two.worktree_key.clone()]),
                include_unassigned: Some(true),
                ..empty_summary_params()
            },
            [0, 0, 0],
            None,
            vec![],
            [0, 0, 0, 0],
        ),
    ];
    let mut server = initialized_server(codex_home.path()).await?;
    let mut first_reports = Vec::new();
    for restart in 0..2 {
        for (index, (name, params, counts, total_tokens, rows, coverage)) in
            cases.iter().enumerate()
        {
            let response: LocalUsageSummaryResponse = server
                .request(|request_id| ClientRequest::LocalUsageSummary {
                    request_id,
                    params: params.clone(),
                })
                .await?;
            let [operations, model_requests, tools] = *counts;
            let [
                assigned_operation_count,
                unassigned_operation_count,
                assigned_token_observation_count,
                unassigned_token_observation_count,
            ] = *coverage;
            assert_eq!(
                (
                    response.aggregate.total_tokens,
                    response.report.counts,
                    response.report.worktrees.clone(),
                    response.report.worktree_coverage
                ),
                (
                    *total_tokens,
                    LocalUsageReportCounts {
                        operations,
                        model_requests,
                        tools
                    },
                    rows.clone(),
                    LocalUsageReportWorktreeCoverage {
                        assigned_operation_count,
                        unassigned_operation_count,
                        assigned_token_observation_count,
                        unassigned_token_observation_count
                    }
                ),
                "{name}, restart {restart}"
            );
            assert_eq!(
                response.report.time_range,
                params
                    .from_at
                    .zip(params.to_at)
                    .map(|(start, end)| LocalUsageReportTimeRange {
                        start_ms: start * 1_000,
                        end_ms: end * 1_000
                    })
            );
            let serialized = serde_json::to_string(&response.report)?;
            assert!(!serialized.contains(&codex_home.path().to_string_lossy().to_string()));
            assert!(!serialized.contains("/private/"));
            let stable = (
                response.aggregate,
                response.token_categories,
                response.report,
            );
            if restart == 0 {
                first_reports.push(stable);
            } else {
                assert_eq!(stable, first_reports[index], "{name} after reopening");
            }
        }
        assert!(server.shutdown_gracefully().await?.success());
        if restart == 0 {
            let reopened = UsageStore::open(codex_home.path()).await?;
            for (workspace, id) in workspaces.iter().zip(&worktrees) {
                assert_eq!(
                    reopened
                        .find_worktree_for_workspace(&seeded.repository_one, workspace)
                        .await?,
                    Some(id.clone())
                );
            }
            reopened.close().await;
            server = initialized_server(codex_home.path()).await?;
        }
    }
    let mut server = initialized_server(codex_home.path()).await?;
    let all_tools: LocalUsageToolListResponse = server
        .request(|request_id| ClientRequest::LocalUsageToolList {
            request_id,
            params: LocalUsageToolListParams {
                thread_id: None,
                repository_key: Some(seeded.repository_one.as_str().to_string()),
                ..tool_list_params(/*limit*/ Some(100), /*cursor*/ None)
            },
        })
        .await?;
    assert_eq!(all_tools.data.len(), 3);
    let mut cursor = None;
    let mut paged = Vec::new();
    for _ in 0..all_tools.data.len() {
        let page: LocalUsageToolListResponse = server
            .request(|request_id| ClientRequest::LocalUsageToolList {
                request_id,
                params: LocalUsageToolListParams {
                    thread_id: None,
                    repository_key: Some(seeded.repository_one.as_str().to_string()),
                    ..tool_list_params(/*limit*/ Some(1), cursor.take())
                },
            })
            .await?;
        assert_eq!(page.data.len(), 1);
        paged.extend(page.data);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(paged, all_tools.data);
    assert_eq!(cursor, None);
    assert!(server.shutdown_gracefully().await?.success());
    Ok(())
}

#[tokio::test]
async fn list_pagination_time_filters_and_cursors_are_validated() -> Result<()> {
    let codex_home = TempDir::new()?;
    seed_usage(codex_home.path()).await?;
    let mut server = initialized_server(codex_home.path()).await?;
    let first: LocalUsageToolListResponse = server
        .request(|request_id| ClientRequest::LocalUsageToolList {
            request_id,
            params: tool_list_params(/*limit*/ Some(1), /*cursor*/ None),
        })
        .await?;
    assert_eq!(first.data.len(), 1);
    let second: LocalUsageToolListResponse = server
        .request(|request_id| ClientRequest::LocalUsageToolList {
            request_id,
            params: tool_list_params(/*limit*/ Some(1), first.next_cursor),
        })
        .await?;
    assert_eq!(second.data.len(), 1);
    assert_ne!(first.data[0].tool_call_id, second.data[0].tool_call_id);

    let filtered: LocalUsageToolListResponse = server
        .request(|request_id| ClientRequest::LocalUsageToolList {
            request_id,
            params: LocalUsageToolListParams {
                from_at: Some(1_800_000_000),
                ..tool_list_params(/*limit*/ Some(10), /*cursor*/ None)
            },
        })
        .await?;
    assert_eq!(filtered.data, Vec::new());

    let error = raw_error(
        &mut server,
        "localUsageTool/list",
        json!({"cursor": "v1|event|0|bad", "limit": 1}),
    )
    .await?;
    assert_eq!(error.error.code, -32602);
    assert_eq!(error.error.message, "cursor is invalid");
    Ok(())
}

#[tokio::test]
async fn corrections_and_repository_mutations_are_append_only_and_notify() -> Result<()> {
    let codex_home = TempDir::new()?;
    let seeded = seed_usage(codex_home.path()).await?;
    let mut server = initialized_server(codex_home.path()).await?;
    let corrected: LocalUsageClassificationCorrectResponse = server
        .request(
            |request_id| ClientRequest::LocalUsageClassificationCorrect {
                request_id,
                params: LocalUsageClassificationCorrectParams {
                    event_id: seeded.model_operation_id.as_string(),
                    phase: LocalUsagePhase::Testing,
                    activity: LocalUsageActivityKind::IntegrationTesting,
                },
            },
        )
        .await?;
    assert_eq!(
        corrected.event.kind,
        LocalUsageEventKind::ClassificationCorrected
    );
    let notification: LocalUsageUpdatedNotification =
        server.read_notification("localUsage/updated").await?;
    assert_eq!(notification.thread_id.as_deref(), Some("thread-one"));

    let updated: LocalUsageRepositoryUpdateResponse = server
        .request(|request_id| ClientRequest::LocalUsageRepositoryUpdate {
            request_id,
            params: LocalUsageRepositoryUpdateParams {
                repository_key: seeded.repository_one.as_str().to_string(),
                label: "Primary Repo".to_string(),
            },
        })
        .await?;
    assert_eq!(updated.repository.label, "Primary Repo");
    let _: LocalUsageUpdatedNotification = server.read_notification("localUsage/updated").await?;

    let merged: LocalUsageRepositoryMergeResponse = server
        .request(|request_id| ClientRequest::LocalUsageRepositoryMerge {
            request_id,
            params: LocalUsageRepositoryMergeParams {
                source_repository_key: seeded.repository_one.as_str().to_string(),
                target_repository_key: seeded.repository_two.as_str().to_string(),
            },
        })
        .await?;
    assert_eq!(
        merged.repository.repository_key,
        seeded.repository_two.as_str()
    );
    let _: LocalUsageUpdatedNotification = server.read_notification("localUsage/updated").await?;

    let cycle = raw_error(
        &mut server,
        "localUsageRepository/merge",
        serde_json::to_value(LocalUsageRepositoryMergeParams {
            source_repository_key: seeded.repository_two.as_str().to_string(),
            target_repository_key: seeded.repository_one.as_str().to_string(),
        })?,
    )
    .await?;
    assert_eq!(cycle.error.code, -32602);
    assert_eq!(cycle.error.message, "repository merge would create a cycle");
    Ok(())
}

#[tokio::test]
async fn repository_mutations_reject_before_append_when_usage_cache_warms() -> Result<()> {
    let codex_home = TempDir::new()?;
    let seeded = seed_usage(codex_home.path()).await?;
    let mut server = initialized_server(codex_home.path()).await?;

    set_usage_core_cache_ready(codex_home.path(), false).await?;
    let update_error = raw_error(
        &mut server,
        "localUsageRepository/update",
        serde_json::to_value(LocalUsageRepositoryUpdateParams {
            repository_key: seeded.repository_one.as_str().to_string(),
            label: "Warming Label".to_string(),
        })?,
    )
    .await?;
    assert!(update_error.error.message.contains("warming"));
    assert_eq!(repository_alias_count(codex_home.path()).await?, 0);

    set_usage_core_cache_ready(codex_home.path(), true).await?;
    let updated: LocalUsageRepositoryUpdateResponse = server
        .request(|request_id| ClientRequest::LocalUsageRepositoryUpdate {
            request_id,
            params: LocalUsageRepositoryUpdateParams {
                repository_key: seeded.repository_one.as_str().to_string(),
                label: "Warming Label".to_string(),
            },
        })
        .await?;
    assert_eq!(updated.repository.label, "Warming Label");
    let _: LocalUsageUpdatedNotification = server.read_notification("localUsage/updated").await?;
    assert_eq!(repository_alias_count(codex_home.path()).await?, 1);

    set_usage_core_cache_ready(codex_home.path(), false).await?;
    let merge_error = raw_error(
        &mut server,
        "localUsageRepository/merge",
        serde_json::to_value(LocalUsageRepositoryMergeParams {
            source_repository_key: seeded.repository_one.as_str().to_string(),
            target_repository_key: seeded.repository_two.as_str().to_string(),
        })?,
    )
    .await?;
    assert!(merge_error.error.message.contains("warming"));
    assert_eq!(repository_merge_count(codex_home.path()).await?, 0);

    set_usage_core_cache_ready(codex_home.path(), true).await?;
    let merged: LocalUsageRepositoryMergeResponse = server
        .request(|request_id| ClientRequest::LocalUsageRepositoryMerge {
            request_id,
            params: LocalUsageRepositoryMergeParams {
                source_repository_key: seeded.repository_one.as_str().to_string(),
                target_repository_key: seeded.repository_two.as_str().to_string(),
            },
        })
        .await?;
    assert_eq!(
        merged.repository.repository_key,
        seeded.repository_two.as_str()
    );
    let _: LocalUsageUpdatedNotification = server.read_notification("localUsage/updated").await?;
    assert_eq!(repository_merge_count(codex_home.path()).await?, 1);
    Ok(())
}

#[tokio::test]
async fn export_is_private_atomic_no_clobber_and_content_free() -> Result<()> {
    let codex_home = TempDir::new()?;
    seed_usage(codex_home.path()).await?;
    let export_dir = codex_home.path().join("private-exports");
    std::fs::create_dir(&export_dir)?;
    std::fs::set_permissions(&export_dir, std::fs::Permissions::from_mode(0o700))?;
    let output_path = export_dir.join("usage.json");
    let mut server = initialized_server(codex_home.path()).await?;
    let exported: LocalUsageExportCreateResponse = server
        .request(|request_id| ClientRequest::LocalUsageExportCreate {
            request_id,
            params: LocalUsageExportCreateParams {
                format: LocalUsageExportFormat::Json,
                output_path: output_path.to_string_lossy().into_owned(),
                repository_key: None,
                thread_id: None,
                from_at: None,
                to_at: None,
            },
        })
        .await?;
    assert_eq!(exported.file_name, "usage.json");
    assert_eq!(output_path.metadata()?.permissions().mode() & 0o777, 0o600);
    let contents = std::fs::read_to_string(&output_path)?;
    assert!(!contents.contains("/private/repo-one"));
    assert!(!contents.contains("https://"));
    assert!(!contents.contains("prompt"));

    let duplicate = raw_error(
        &mut server,
        "localUsageExport/create",
        serde_json::to_value(LocalUsageExportCreateParams {
            format: LocalUsageExportFormat::Json,
            output_path: output_path.to_string_lossy().into_owned(),
            repository_key: None,
            thread_id: None,
            from_at: None,
            to_at: None,
        })?,
    )
    .await?;
    assert_eq!(duplicate.error.code, -32602);
    assert!(
        !duplicate
            .error
            .message
            .contains(output_path.to_string_lossy().as_ref())
    );
    Ok(())
}

#[tokio::test]
async fn corruption_returns_a_stable_redacted_error_without_resetting() -> Result<()> {
    let codex_home = TempDir::new()?;
    let usage_dir = codex_home.path().join("usage");
    std::fs::create_dir(&usage_dir)?;
    std::fs::set_permissions(&usage_dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(
        usage_dir.join("usage.sqlite3"),
        b"not sqlite or prompt text",
    )?;
    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let error = raw_error(
        &mut server,
        "localUsage/summary",
        serde_json::to_value(empty_summary_params())?,
    )
    .await?;
    assert_eq!(error.error.code, -32603);
    assert_eq!(error.error.message, "local usage database is unavailable");
    assert!(
        !error
            .error
            .message
            .contains(codex_home.path().to_string_lossy().as_ref())
    );
    assert!(!error.error.message.contains("prompt"));
    assert_eq!(
        std::fs::read(usage_dir.join("usage.sqlite3"))?,
        b"not sqlite or prompt text"
    );
    Ok(())
}

#[tokio::test]
async fn stock_initialize_can_ignore_extension_capabilities() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    let initialized = server
        .initialize_with_capabilities(
            ClientInfo {
                name: DEFAULT_CLIENT_NAME.to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            /*capabilities*/ None,
        )
        .await?;
    let JSONRPCMessage::Response(response) = initialized else {
        anyhow::bail!("stock initialize should succeed")
    };
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct StockInitializeResponse {
        user_agent: String,
    }
    let stock: StockInitializeResponse = serde_json::from_value(response.result)?;
    assert!(!stock.user_agent.is_empty());
    Ok(())
}

async fn initialized_server(codex_home: &Path) -> Result<TestAppServer> {
    let server = TestAppServer::builder()
        .with_codex_home(codex_home)
        .build_initialized()
        .await?;
    let usage = UsageStore::open(codex_home).await?;
    let mut ready = false;
    for _ in 0..100 {
        if usage.report_cache_status().await?.ready {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    usage.close().await;
    assert!(ready, "usage report cache did not become ready");
    Ok(server)
}

async fn set_usage_core_cache_ready(codex_home: &Path, ready: bool) -> Result<()> {
    let database = codex_home.join("usage").join("usage.sqlite3");
    let pool =
        codex_state::open_sqlite_pool(&database, codex_state::SqlitePoolProfile::DurableEvents)
            .await?;
    sqlx::query("PRAGMA busy_timeout = 5000")
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE _usage_report_cache_meta SET ready=? WHERE singleton=1")
        .bind(if ready { 1_i64 } else { 0_i64 })
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

async fn repository_alias_count(codex_home: &Path) -> Result<i64> {
    let database = codex_home.join("usage").join("usage.sqlite3");
    let pool =
        codex_state::open_sqlite_pool(&database, codex_state::SqlitePoolProfile::DurableEvents)
            .await?;
    let count = sqlx::query_scalar("SELECT COUNT(*) FROM repository_alias_events")
        .fetch_one(&pool)
        .await?;
    pool.close().await;
    Ok(count)
}

async fn repository_merge_count(codex_home: &Path) -> Result<i64> {
    let database = codex_home.join("usage").join("usage.sqlite3");
    let pool =
        codex_state::open_sqlite_pool(&database, codex_state::SqlitePoolProfile::DurableEvents)
            .await?;
    let count = sqlx::query_scalar("SELECT COUNT(*) FROM repository_merge_events")
        .fetch_one(&pool)
        .await?;
    pool.close().await;
    Ok(count)
}

async fn raw_error(
    server: &mut TestAppServer,
    method: &str,
    params: serde_json::Value,
) -> Result<JSONRPCError> {
    let request_id = server.send_raw_request(method, Some(params)).await?;
    timeout(
        Duration::from_secs(10),
        server.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await?
}

async fn expected_reports(
    codex_home: &Path,
    repository_id: &RepositoryId,
) -> Result<(serde_json::Value, serde_json::Value, serde_json::Value)> {
    let store = UsageStore::open(codex_home).await?;
    let all = store
        .usage_summary_query(UsageSummaryQuery {
            worktree_ids: None,
            include_unassigned_worktrees: true,
            thread_id: None,
            repository_id: None,
            account_profile_ref: None,
            time_range: None,
        })
        .await?;
    let thread = store
        .usage_summary_query(UsageSummaryQuery {
            worktree_ids: None,
            include_unassigned_worktrees: true,
            thread_id: Some(ThreadId::new("thread-one")?),
            repository_id: None,
            account_profile_ref: None,
            time_range: None,
        })
        .await?;
    let repository = store
        .usage_summary_query(UsageSummaryQuery {
            worktree_ids: None,
            include_unassigned_worktrees: true,
            thread_id: None,
            repository_id: Some(repository_id.clone()),
            account_profile_ref: None,
            time_range: None,
        })
        .await?;
    store.close().await;
    let profile = AccountProfileRef::new("profile-one")?;
    Ok((
        serde_json::to_value(StructuredUsageSummary::new(&all, /*account*/ None))?,
        serde_json::to_value(StructuredUsageSummary::new(
            &thread,
            Some(codex_usage::redacted_account_profile_label(&profile)),
        ))?,
        serde_json::to_value(StructuredUsageSummary::new(
            &repository,
            /*account*/ None,
        ))?,
    ))
}

fn assert_report_matches_structured(report: &LocalUsageReport, expected: serde_json::Value) {
    let mut actual = serde_json::to_value(report).expect("report should serialize");
    let provider_tokens = actual
        .get_mut("providerTokens")
        .and_then(serde_json::Value::as_array_mut)
        .expect("provider tokens should be an array");
    for tokens in provider_tokens {
        tokens
            .as_object_mut()
            .expect("provider token should be an object")
            .remove("repositoryLabel");
    }
    assert_eq!(actual, expected);
}

fn empty_summary_params() -> LocalUsageSummaryParams {
    LocalUsageSummaryParams {
        worktree_keys: None,
        include_unassigned: None,
        repository_key: None,
        thread_id: None,
        account_id: None,
        from_at: None,
        to_at: None,
    }
}

fn tool_list_params(limit: Option<u32>, cursor: Option<String>) -> LocalUsageToolListParams {
    LocalUsageToolListParams {
        cursor,
        limit,
        thread_id: Some("thread-one".to_string()),
        repository_key: None,
        from_at: None,
        to_at: None,
    }
}

fn activity_list_params(
    limit: Option<u32>,
    cursor: Option<String>,
) -> LocalUsageActivityListParams {
    LocalUsageActivityListParams {
        cursor,
        limit,
        thread_id: Some("thread-one".to_string()),
        agent_id: None,
        from_at: None,
        to_at: None,
    }
}

fn event_list_params(limit: Option<u32>, cursor: Option<String>) -> LocalUsageEventListParams {
    LocalUsageEventListParams {
        cursor,
        limit,
        thread_id: Some("thread-one".to_string()),
        repository_key: None,
        kind: None,
        from_at: None,
        to_at: None,
    }
}

async fn seed_usage(codex_home: &Path) -> Result<SeededUsage> {
    let store = UsageStore::open(codex_home).await?;
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, STARTED_AT_MS - 100)
        .await?;
    let thread_id = ThreadId::new("thread-one")?;
    let turn_id = TurnId::new("turn-one")?;
    let agent_id = AgentId::new("agent-one")?;
    store
        .ensure_thread(&NewThread {
            id: thread_id.clone(),
            parent_thread_id: None,
            source_kind: ThreadSourceKind::new("cli")?,
            created_at_ms: STARTED_AT_MS,
        })
        .await?;
    store
        .ensure_turn(&NewTurn {
            id: turn_id.clone(),
            thread_id: thread_id.clone(),
            account: AccountAttributionSnapshot::new(
                Some(AccountProfileRef::new("profile-one")?),
                Some(AccountAuthMode::Chatgpt),
            ),
            created_at_ms: STARTED_AT_MS,
        })
        .await?;
    store
        .ensure_agent(&NewAgent {
            id: agent_id.clone(),
            thread_id: thread_id.clone(),
            parent_agent_id: None,
            role_kind: AgentRoleKind::new("root")?,
            created_at_ms: STARTED_AT_MS,
        })
        .await?;
    let repository_one = store
        .resolve_repository(
            &RepositoryIdentityInput::new(CanonicalRepositoryPath::new("/private/repo-one")?),
            &SafeRepositoryLabel::new("repo-one")?,
            STARTED_AT_MS,
        )
        .await?;
    let repository_two = store
        .resolve_repository(
            &RepositoryIdentityInput::new(CanonicalRepositoryPath::new("/private/repo-two")?),
            &SafeRepositoryLabel::new("repo-two")?,
            STARTED_AT_MS + 1,
        )
        .await?;

    let model_operation = operation(
        process_id,
        &thread_id,
        &turn_id,
        &agent_id,
        OperationKind::ModelRequest,
        STARTED_AT_MS + 10,
    );
    store.begin_operation(&model_operation).await?;
    let request_id = ModelRequestId::new();
    store
        .record_model_request(&NewModelRequest {
            id: request_id,
            operation_id: model_operation.id,
            provider_kind: ProviderKind::new("openai")?,
            model: ModelName::new("test-model")?,
            transport_kind: TransportKind::new("sse")?,
            attempt_number: 1,
            account: AccountAttributionSnapshot::new(
                Some(AccountProfileRef::new("profile-one")?),
                Some(AccountAuthMode::Chatgpt),
            ),
            client_origin: ClientOrigin::new("test")?,
        })
        .await?;
    attribute(&store, model_operation.id, repository_one.clone()).await?;
    let source_event_id = FactEventId::new();
    for (path, count) in [
        ("input_tokens", 12),
        ("output_tokens", 3),
        ("total_tokens", 15),
    ] {
        store
            .record_token_observation(&NewTokenObservation {
                id: FactEventId::new(),
                source_event_id,
                source: TokenObservationSource::ModelRequest(request_id),
                category_path: TokenCategoryPath::new(path)?,
                token_count: Some(count),
                unit: TokenUnit::Tokens,
                measurement_provenance: MeasurementProvenance::ProviderReported,
                coverage_state: CoverageState::Complete,
                repository_bucket: RepositoryBucket::Single(repository_one.clone()),
                observed_at_ms: STARTED_AT_MS + 20,
            })
            .await?;
    }
    store
        .record_coverage(&NewCoverageEvent {
            event_id: FactEventId::new(),
            operation_id: Some(model_operation.id),
            scope_kind: CoverageScopeKind::new("model_attempt")?,
            state: CoverageState::Complete,
            reason_code: None,
            occurred_at_ms: STARTED_AT_MS + 21,
        })
        .await?;
    finish(&store, model_operation.id, STARTED_AT_MS + 30).await?;

    for offset in [40, 50] {
        let tool_operation = operation(
            process_id,
            &thread_id,
            &turn_id,
            &agent_id,
            OperationKind::LocalTool,
            STARTED_AT_MS + offset,
        );
        store.begin_operation(&tool_operation).await?;
        store
            .record_tool_invocation(&NewToolInvocation {
                id: ToolInvocationId::new(),
                operation_id: tool_operation.id,
                operation_kind: OperationKind::LocalTool,
                tool_kind: ToolKind::new("shell")?,
                safe_tool_name: ToolName::new("shell")?,
                operation_family: OperationFamily::new("test")?,
                observation_timing: ObservationTiming::new("before_dispatch")?,
                covering_model_request_id: None,
                execution_group_id: None,
                execution_role: codex_usage::ToolExecutionRole::Standalone,
            })
            .await?;
        attribute(&store, tool_operation.id, repository_one.clone()).await?;
        finish(&store, tool_operation.id, STARTED_AT_MS + offset + 5).await?;
    }
    store.close().await;
    Ok(SeededUsage {
        repository_one,
        repository_two,
        model_operation_id: model_operation.id,
    })
}

fn operation(
    process_id: ProcessId,
    thread_id: &ThreadId,
    turn_id: &TurnId,
    agent_id: &AgentId,
    kind: OperationKind,
    started_at_ms: i64,
) -> NewOperation {
    NewOperation {
        work_context: None,
        id: OperationId::new(),
        process_id,
        thread_id: Some(thread_id.clone()),
        turn_id: Some(turn_id.clone()),
        agent_id: Some(agent_id.clone()),
        parent_operation_id: None,
        retry_of_operation_id: None,
        rework_of_operation_id: None,
        kind,
        started_at_ms,
        phase: Phase::Implementation,
        activity: Activity::Coding,
        activity_state: if kind == OperationKind::ModelRequest {
            ActivityState::ModelActive
        } else {
            ActivityState::ToolActive
        },
        attribution_provenance: AttributionProvenance::AgentDeclared,
    }
}

async fn attribute(store: &UsageStore, operation_id: OperationId, id: RepositoryId) -> Result<()> {
    store
        .record_repository_attribution(&NewRepositoryAttribution {
            event_id: FactEventId::new(),
            operation_id,
            repository_id: Some(id),
            worktree_id: None,
            kind: RepositoryAttributionKind::Primary,
            provenance: RepositoryAttributionProvenance::RuntimeObserved,
            occurred_at_ms: STARTED_AT_MS + 1,
        })
        .await?;
    Ok(())
}

async fn finish(store: &UsageStore, operation_id: OperationId, occurred_at_ms: i64) -> Result<()> {
    store
        .finish_operation(&TerminalOperation {
            operation_id,
            status: TerminalStatus::Completed,
            occurred_at_ms,
            duration_ns: 5_000_000,
            error_category: None,
        })
        .await?;
    Ok(())
}
