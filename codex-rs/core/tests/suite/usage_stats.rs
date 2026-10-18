use anyhow::Context;
use anyhow::Result;
use codex_exec_server::CreateDirectoryOptions;
use codex_features::Feature;
use codex_usage::AccountAttributionSnapshot;
use codex_usage::Activity;
use codex_usage::ActivityState;
use codex_usage::AttributionProvenance;
use codex_usage::FactEventId;
use codex_usage::MeasurementProvenance;
use codex_usage::ModelName;
use codex_usage::NewModelRequest;
use codex_usage::NewOperation;
use codex_usage::NewThread;
use codex_usage::NewTokenObservation;
use codex_usage::OperationId;
use codex_usage::OperationKind;
use codex_usage::Phase;
use codex_usage::ProcessId;
use codex_usage::ProviderKind;
use codex_usage::RepositoryBucket;
use codex_usage::TerminalOperation;
use codex_usage::TerminalStatus;
use codex_usage::ThreadId;
use codex_usage::ThreadSourceKind;
use codex_usage::TokenCategoryPath;
use codex_usage::TokenObservationSource;
use codex_usage::TokenUnit;
use codex_usage::TransportKind;
use codex_usage::UsageDetailKind;
use codex_usage::UsageDetailListQuery;
use codex_usage::UsageDetailRecord;
use codex_usage::UsagePageRequest;
use codex_usage::UsageStore;
use codex_usage::UsageSummaryScope;
use codex_utils_path_uri::PathUri;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::local;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

fn tool_output(request: &responses::ResponsesRequest, call_id: &str) -> Value {
    let content = request
        .function_call_output_text(call_id)
        .expect("text function output");
    serde_json::from_str(&content)
        .unwrap_or_else(|error| panic!("JSON tool output ({error}): {content}"))
}

async fn update_usage_metadata(home: &TempDir, statement: &'static str) -> Result<()> {
    let pool = codex_state::open_sqlite_pool(
        &home.path().join("usage/usage.sqlite3"),
        codex_state::SqlitePoolProfile::DurableEvents,
    )
    .await?;
    sqlx::query(statement).execute(&pool).await?;
    pool.close().await;
    Ok(())
}

async fn mark_global_report_warming(home: &TempDir) -> Result<()> {
    update_usage_metadata(
        home,
        "UPDATE _usage_report_global_meta SET ready=0 WHERE singleton=1",
    )
    .await
}

async fn mark_core_report_warming(home: &TempDir) -> Result<()> {
    update_usage_metadata(
        home,
        "UPDATE _usage_report_cache_meta SET schema_version=999, ready=0 WHERE singleton=1",
    )
    .await
}

fn ev_completed_with_usage(id: &str, input_tokens: i64, output_tokens: i64) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": {
                "input_tokens": input_tokens,
                "input_tokens_details": null,
                "output_tokens": output_tokens,
                "output_tokens_details": null,
                "total_tokens": input_tokens + output_tokens
            }
        }
    })
}

async fn seed_small_performance_review(home: &TempDir) -> Result<()> {
    let store = UsageStore::open(home.path()).await?;
    let process = ProcessId::new();
    store.register_process(&process, 42, 0).await?;
    for (id, parent) in [
        ("scale-root", None),
        ("scale-child", Some("scale-root")),
        ("scale-other", None),
    ] {
        store
            .ensure_thread(&NewThread {
                id: ThreadId::new(id)?,
                parent_thread_id: parent.map(ThreadId::new).transpose()?,
                source_kind: ThreadSourceKind::new(if parent.is_some() {
                    "subagent"
                } else {
                    "cli"
                })?,
                created_at_ms: 0,
            })
            .await?;
    }
    for (thread, started_at_ms, tokens) in [
        ("scale-root", 1_000, 11),
        ("scale-child", 1_100, 13),
        ("scale-other", 1_200, 99),
    ] {
        let operation = NewOperation {
            work_context: None,
            id: OperationId::new(),
            process_id: process,
            thread_id: Some(ThreadId::new(thread)?),
            turn_id: None,
            agent_id: None,
            parent_operation_id: None,
            retry_of_operation_id: None,
            rework_of_operation_id: None,
            kind: OperationKind::ModelRequest,
            started_at_ms,
            phase: Phase::Implementation,
            activity: Activity::Coding,
            activity_state: ActivityState::ModelActive,
            attribution_provenance: AttributionProvenance::AgentDeclared,
        };
        store.begin_operation(&operation).await?;
        let request = codex_usage::ModelRequestId::new();
        store
            .record_model_request(&NewModelRequest {
                id: request,
                operation_id: operation.id,
                provider_kind: ProviderKind::new("openai")?,
                model: ModelName::new("fixture")?,
                transport_kind: TransportKind::new("http")?,
                attempt_number: 1,
                account: AccountAttributionSnapshot::unknown(),
                client_origin: codex_usage::ClientOrigin::new("test")?,
            })
            .await?;
        store
            .record_token_observation(&NewTokenObservation {
                id: FactEventId::new(),
                source_event_id: FactEventId::new(),
                source: TokenObservationSource::ModelRequest(request),
                category_path: TokenCategoryPath::new("total_tokens")?,
                token_count: Some(tokens),
                unit: TokenUnit::Tokens,
                measurement_provenance: MeasurementProvenance::ProviderReported,
                coverage_state: codex_usage::CoverageState::Complete,
                repository_bucket: RepositoryBucket::Unknown,
                observed_at_ms: 1_500,
            })
            .await?;
        store
            .finish_operation(&TerminalOperation {
                operation_id: operation.id,
                status: TerminalStatus::Completed,
                occurred_at_ms: started_at_ms + 1_000,
                duration_ns: 1_000_000_000,
                error_category: None,
            })
            .await?;
    }
    Ok(())
}

async fn prepare_performance_review_fixture(home: &TempDir) -> Result<(i64, i64, u64, u64)> {
    #[cfg(unix)]
    if let Some(fixture) = std::env::var_os("CXM_USAGE_SCALE_FIXTURE") {
        let fixture = std::path::PathBuf::from(fixture);
        let scopes: Value = serde_json::from_slice(&std::fs::read(fixture.join("scope.json"))?)?;
        anyhow::ensure!(scopes["fixture_shape_version"] == 2);
        std::os::unix::fs::symlink(fixture.join("usage"), home.path().join("usage"))?;
        return Ok((
            scopes["window_start_ms"].as_i64().context("window start")?,
            scopes["window_end_ms"].as_i64().context("window end")?,
            scopes["expected_root_window_operations"]
                .as_u64()
                .context("root operation total")?,
            scopes["expected_root_window_provider_tokens"]
                .as_u64()
                .context("root token total")?,
        ));
    }
    seed_small_performance_review(home).await?;
    Ok((900, 2_200, 2, 24))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_reads_finite_performance_review_with_descendants() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let home = Arc::new(TempDir::new()?);
    let (from_at_ms, to_at_ms, expected_operations, expected_provider_tokens) =
        prepare_performance_review_fixture(&home).await?;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-review"),
                ev_function_call(
                    "read-review",
                    "usage_stats",
                    &json!({
                        "action": "performance_review",
                        "thread_id": "scale-root",
                        "include_descendants": true,
                        "from_at_ms": from_at_ms,
                        "to_at_ms": to_at_ms
                    })
                    .to_string(),
                ),
                ev_completed("resp-review"),
            ]),
            sse(vec![
                ev_response_created("resp-review-followup"),
                ev_assistant_message("msg-review", "review read"),
                ev_completed("resp-review-followup"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_home(Arc::clone(&home))
        .with_config(|config| config.local_control_tools_enabled = true)
        .build(&server)
        .await?;
    test.submit_turn("inspect the finite performance review")
        .await?;

    let requests = response_mock.requests();
    let review = requests
        .iter()
        .find_map(|request| request.function_call_output_text("read-review"))
        .context("performance review tool output")?;
    let review: Value = serde_json::from_str(&review)?;
    assert_eq!(review["kind"], "performanceReview");
    assert_eq!(review["window"]["fromAtMs"], from_at_ms);
    assert_eq!(review["window"]["toAtMs"], to_at_ms);
    assert_eq!(review["coverage"]["rawOperations"], expected_operations);
    assert!(
        review["coverage"]["rawTokenObservations"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
    assert_eq!(
        review["tokens"][0]["measuredTokens"],
        expected_provider_tokens
    );
    assert_eq!(
        review["outcomes"]["totals"]["operations"],
        expected_operations
    );
    assert_eq!(review["reportingOperationInProgress"], true);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_gets_typed_warming_for_unbounded_current_chat_summary() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-warming"),
                ev_function_call(
                    "read-current-chat",
                    "usage_stats",
                    &json!({
                        "action": "summary",
                        "scope": "current_chat"
                    })
                    .to_string(),
                ),
                ev_completed("resp-warming"),
            ]),
            sse(vec![
                ev_response_created("resp-warming-from"),
                ev_function_call(
                    "read-current-chat-from",
                    "usage_stats",
                    &json!({
                        "action": "summary",
                        "scope": "current_chat",
                        "from_at_ms": 1
                    })
                    .to_string(),
                ),
                ev_completed("resp-warming-from"),
            ]),
            sse(vec![
                ev_response_created("resp-warming-to"),
                ev_function_call(
                    "read-current-chat-to",
                    "usage_stats",
                    &json!({
                        "action": "summary",
                        "scope": "current_chat",
                        "to_at_ms": 2
                    })
                    .to_string(),
                ),
                ev_completed("resp-warming-to"),
            ]),
            sse(vec![
                ev_response_created("resp-warming-followup"),
                ev_assistant_message("msg-warming", "warming read"),
                ev_completed("resp-warming-followup"),
            ]),
        ],
    )
    .await;
    let home = Arc::new(TempDir::new()?);
    UsageStore::open(home.path()).await?.close().await;
    let test = test_codex()
        .with_home(Arc::clone(&home))
        .with_config(|config| config.local_control_tools_enabled = true)
        .build(&server)
        .await?;
    mark_core_report_warming(&home).await?;
    test.submit_turn("inspect current chat usage").await?;

    let requests = response_mock.requests();
    for call_id in [
        "read-current-chat",
        "read-current-chat-from",
        "read-current-chat-to",
    ] {
        let warming = requests
            .iter()
            .find_map(|request| request.function_call_output_text(call_id))
            .with_context(|| format!("current chat usage output for {call_id}"))?;
        let warming: Value = serde_json::from_str(&warming)?;
        assert_eq!(warming["kind"], "usageReportWarming");
        assert_eq!(warming["action"], "summary");
        assert_eq!(warming["reportingOperationInProgress"], false);
        assert_eq!(warming["retryAfterMs"], 1_000);
        assert_eq!(warming["cache"]["ready"], false);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_activity_crosses_turns_without_auxiliary_root_becoming_multi_repo() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "declare-activity",
                    "usage_activity",
                    &json!({
                        "action": "set",
                        "phase": "planning",
                        "activity": "diagnosis"
                    })
                    .to_string(),
                ),
                ev_completed_with_usage(
                    "resp-1", /*input_tokens*/ 10, /*output_tokens*/ 2,
                ),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed_with_usage(
                    "resp-2", /*input_tokens*/ 20, /*output_tokens*/ 3,
                ),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-2", "continued"),
                ev_completed_with_usage(
                    "resp-3", /*input_tokens*/ 30, /*output_tokens*/ 5,
                ),
            ]),
        ],
    )
    .await;
    let home = Arc::new(TempDir::new()?);
    let mut builder = test_codex()
        .with_home(Arc::clone(&home))
        .with_config(|config| config.local_control_tools_enabled = true)
        .with_workspace_setup(|cwd, fs| async move {
            let auxiliary_output = cwd.join("auxiliary-output");
            for root in [&cwd, &auxiliary_output] {
                fs.create_directory(
                    &PathUri::from_abs_path(root).join(".git")?,
                    CreateDirectoryOptions {
                        recursive: true,
                        follow_symlinks: false,
                    },
                    /*sandbox*/ None,
                )
                .await?;
            }
            Ok(())
        });
    // Repository identity discovery is deliberately app-host-local. Keep this fixture local while
    // the remote-executor matrix covers transport-independent usage accounting elsewhere.
    let test = builder.build(&server).await?;
    let mut environment = local(test.config.cwd.clone());
    environment.workspace_roots.push(PathUri::from_abs_path(
        &test.config.cwd.join("auxiliary-output"),
    ));

    test.submit_turn_with_environments("inspect the project", Some(vec![environment.clone()]))
        .await?;
    test.submit_turn_with_environments("continue the project", Some(vec![environment]))
        .await?;

    assert_eq!(response_mock.requests().len(), 3);
    let usage = UsageStore::open(home.path()).await?;
    let summary = usage.usage_summary(UsageSummaryScope::All).await?;
    let repository_buckets = summary
        .tokens
        .iter()
        .map(|tokens| tokens.repository_bucket.clone())
        .collect::<BTreeSet<_>>();
    assert!(!repository_buckets.is_empty());
    assert!(!repository_buckets.contains("multi_repo"));
    let operations = usage
        .list_details(
            UsageDetailKind::Operations,
            &UsageDetailListQuery {
                page: UsagePageRequest {
                    cursor: None,
                    limit: 50,
                },
                time_range: None,
                thread_id: None,
                repository_id: None,
                account_profile_ref: None,
            },
            codex_usage::redacted_account_profile_label,
        )
        .await?;
    let classified_request_ids = operations
        .data
        .iter()
        .filter_map(|record| match record {
            UsageDetailRecord::Operation(operation)
                if operation.operation_kind == "model_request"
                    && operation.activity == "diagnosis" =>
            {
                operation
                    .model_request
                    .as_ref()
                    .map(|request| request.id.clone())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert!(!classified_request_ids.is_empty());
    let tokens = usage
        .list_details(
            UsageDetailKind::Tokens,
            &UsageDetailListQuery {
                page: UsagePageRequest {
                    cursor: None,
                    limit: 50,
                },
                time_range: None,
                thread_id: None,
                repository_id: None,
                account_profile_ref: None,
            },
            codex_usage::redacted_account_profile_label,
        )
        .await?;
    let classified_buckets = tokens
        .data
        .iter()
        .filter_map(|record| match record {
            UsageDetailRecord::Token {
                model_request_id: Some(model_request_id),
                repository_bucket,
                ..
            } if classified_request_ids.contains(model_request_id) => {
                Some(repository_bucket.clone())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert!(!classified_buckets.is_empty());
    assert!(!classified_buckets.contains("multi_repo"));
    assert!(
        classified_buckets.iter().any(|bucket| bucket != "unknown"),
        "at least one enriched classified request should use the project bucket: {classified_buckets:?}"
    );
    assert!(
        summary
            .provider_tokens_by_activity
            .iter()
            .any(|tokens| tokens.activity == "diagnosis" && tokens.measured_tokens == 58)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_reads_current_repository_usage_and_appends_a_classification_correction() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let first_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-seed"),
                ev_assistant_message("msg-seed", "usage seeded"),
                ev_completed("resp-seed"),
            ]),
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "read-usage",
                    "usage_stats",
                    &json!({
                        "action": "summary",
                        "scope": "current_repository"
                    })
                    .to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "usage read"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let home = Arc::new(TempDir::new()?);
    let mut builder = test_codex()
        .with_home(Arc::clone(&home))
        .with_config(|config| config.local_control_tools_enabled = true)
        .with_workspace_setup(|cwd, _fs| async move {
            let output = std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&cwd)
                .output()?;
            anyhow::ensure!(output.status.success(), "initialize isolated repository");
            Ok(())
        });
    // Current-repository usage resolves app-host Git metadata, and both turns below explicitly use
    // the local environment. Keep the fixture local instead of projecting a remote Windows cwd
    // onto the app host in Wine-exec runs.
    let test = builder.build(&server).await?;
    test.submit_turn_with_environments(
        "seed usage history",
        Some(vec![local(test.config.cwd.clone())]),
    )
    .await?;
    let usage = UsageStore::open(home.path()).await?;
    let mut seeded = false;
    for _ in 0..100 {
        let details = usage
            .list_details(
                UsageDetailKind::Operations,
                &UsageDetailListQuery {
                    page: UsagePageRequest {
                        cursor: None,
                        limit: 20,
                    },
                    time_range: None,
                    thread_id: None,
                    repository_id: None,
                    account_profile_ref: None,
                },
                codex_usage::redacted_account_profile_label,
            )
            .await?;
        if details.data.iter().any(|record| {
            matches!(
                record,
                UsageDetailRecord::Operation(detail)
                    if detail.operation_kind == "model_request"
            )
        }) {
            seeded = true;
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert!(seeded, "seed model request was not captured");
    let mut report_ready = false;
    for _ in 0..100 {
        if usage.report_cache_status().await?.ready {
            report_ready = true;
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert!(report_ready, "usage report cache did not become ready");
    mark_global_report_warming(&home).await?;
    test.submit_turn_with_environments(
        "inspect current repository usage",
        Some(vec![local(test.config.cwd.clone())]),
    )
    .await?;

    let first_requests = first_mock.requests();
    assert_eq!(first_requests.len(), 3);
    assert!(
        first_requests[0].body_json()["tools"]
            .as_array()
            .is_some_and(|tools| tools
                .iter()
                .any(|tool| tool["name"].as_str() == Some("usage_stats")))
    );
    let summary = tool_output(&first_requests[2], "read-usage");
    assert_eq!(summary["kind"], "usageSummary");
    assert_eq!(summary["scope"]["type"], "repository");
    assert_eq!(summary["reportingOperationInProgress"], true);
    assert!(summary["providerTokens"].is_array());
    assert!(summary["classifications"].is_array());
    assert!(summary["time"]["executionWallUnion"].is_object());
    assert!(
        !summary
            .to_string()
            .contains(test.config.cwd.to_string_lossy().as_ref())
    );

    let usage = UsageStore::open(home.path()).await?;
    let operations = usage
        .list_details(
            UsageDetailKind::Operations,
            &UsageDetailListQuery {
                page: UsagePageRequest {
                    cursor: None,
                    limit: 20,
                },
                time_range: None,
                thread_id: None,
                repository_id: None,
                account_profile_ref: None,
            },
            codex_usage::redacted_account_profile_label,
        )
        .await?;
    let target_id = operations
        .data
        .iter()
        .find_map(|record| match record {
            UsageDetailRecord::Operation(detail) if detail.operation_kind == "model_request" => {
                Some(detail.id.clone())
            }
            _ => None,
        })
        .context("model operation detail")?;

    let correction_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-3"),
                ev_function_call(
                    "correct-usage",
                    "usage_activity",
                    &json!({
                        "action": "correct_classification",
                        "target_id": target_id,
                        "phase": "reporting",
                        "activity": "verification_review"
                    })
                    .to_string(),
                ),
                ev_completed("resp-3"),
            ]),
            sse(vec![
                ev_response_created("resp-4"),
                ev_assistant_message("msg-2", "usage corrected"),
                ev_completed("resp-4"),
            ]),
        ],
    )
    .await;
    test.submit_turn_with_environments(
        "correct that classification",
        Some(vec![local(test.config.cwd.clone())]),
    )
    .await?;

    let correction_requests = correction_mock.requests();
    assert_eq!(correction_requests.len(), 2);
    let correction = tool_output(&correction_requests[1], "correct-usage");
    assert_eq!(correction["kind"], "usageClassificationCorrected");
    assert_eq!(correction["event"]["event"], "classification_corrected");
    assert_eq!(correction["event"]["provenance"], "user_corrected");
    assert!(correction.get("reportingOperationInProgress").is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_tree_summary_reports_code_mode_deduplication_and_context_sources() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-tree-1"),
                ev_custom_tool_call(
                    "code-mode-wrapper",
                    "exec",
                    r#"
const result = await tools.get_context_remaining({});
text(JSON.stringify(result));
"#,
                ),
                ev_completed_with_usage(
                    "resp-tree-1",
                    /*input_tokens*/ 10,
                    /*output_tokens*/ 2,
                ),
            ]),
            sse(vec![
                ev_response_created("resp-tree-2"),
                ev_function_call(
                    "read-tree",
                    "usage_stats",
                    &json!({
                        "action": "task_tree_summary",
                        "root_thread_id": "current",
                        "include_descendants": true,
                        "from_at_ms": 0,
                        "to_at_ms": 4_000_000_000_000_i64
                    })
                    .to_string(),
                ),
                ev_completed_with_usage(
                    "resp-tree-2",
                    /*input_tokens*/ 20,
                    /*output_tokens*/ 3,
                ),
            ]),
            sse(vec![
                ev_response_created("resp-tree-3"),
                ev_assistant_message("msg-tree", "done"),
                ev_completed("resp-tree-3"),
            ]),
        ],
    )
    .await;
    let home = Arc::new(TempDir::new()?);
    let mut builder = test_codex()
        .with_home(Arc::clone(&home))
        .with_config(|config| {
            config.local_control_tools_enabled = true;
            config
                .features
                .enable(Feature::CodeMode)
                .expect("code mode feature");
            config
                .features
                .enable(Feature::TokenBudget)
                .expect("token budget feature");
            config.model_context_window = Some(10_000);
        });
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_turn("inspect this task tree").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    let summary = tool_output(&requests[2], "read-tree");
    assert_eq!(summary["kind"], "taskTreeSummary");
    assert_eq!(summary["includeDescendants"], true);
    assert_eq!(summary["counts"]["wrapperToolOperations"], 1);
    assert_eq!(summary["counts"]["nestedToolOperations"], 1);
    assert_eq!(summary["counts"]["rawToolOperations"], 3);
    assert_eq!(summary["counts"]["deduplicatedToolOperations"], 2);
    assert_eq!(
        summary["totals"]["providerTotalTokens"]["measuredTokens"],
        35
    );
    assert_eq!(summary["context"]["observedRequests"], 2);
    let sources = summary["context"]["sources"]
        .as_array()
        .context("context source array")?;
    assert!(sources.iter().any(|source| {
        source["source"] == "tool_output"
            && source["estimatedTokens"]
                .as_u64()
                .is_some_and(|value| value > 0)
    }));

    let usage = UsageStore::open(home.path()).await?;
    let operations = usage
        .list_details(
            UsageDetailKind::Operations,
            &UsageDetailListQuery {
                page: UsagePageRequest {
                    cursor: None,
                    limit: 50,
                },
                time_range: None,
                thread_id: None,
                repository_id: None,
                account_profile_ref: None,
            },
            codex_usage::redacted_account_profile_label,
        )
        .await?;
    let tool_details = operations
        .data
        .iter()
        .filter_map(|record| match record {
            UsageDetailRecord::Operation(operation) => operation.tool.as_ref(),
            _ => None,
        })
        .collect::<Vec<_>>();
    let wrapper = tool_details
        .iter()
        .find(|tool| tool.execution_role == "wrapper")
        .context("code mode wrapper usage")?;
    let nested = tool_details
        .iter()
        .find(|tool| tool.execution_role == "nested")
        .context("nested code mode usage")?;
    assert_eq!(wrapper.execution_group_id, nested.execution_group_id);
    assert!(wrapper.execution_group_id.is_some());
    let model_contexts = operations
        .data
        .iter()
        .filter_map(|record| match record {
            UsageDetailRecord::Operation(operation) => operation
                .model_request
                .as_ref()
                .and_then(|request| request.context.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(model_contexts.len(), 3);
    assert!(
        model_contexts
            .iter()
            .all(|context| context.policy_estimated_tokens > 0)
    );
    assert!(
        model_contexts
            .iter()
            .any(|context| context.tool_output_estimated_tokens > 0)
    );
    Ok(())
}
