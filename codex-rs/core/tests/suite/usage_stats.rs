use anyhow::Context;
use anyhow::Result;
use codex_exec_server::CreateDirectoryOptions;
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
use tempfile::TempDir;

fn tool_output(request: &responses::ResponsesRequest, call_id: &str) -> Value {
    let content = request
        .function_call_output_text(call_id)
        .expect("text function output");
    serde_json::from_str(&content)
        .unwrap_or_else(|error| panic!("JSON tool output ({error}): {content}"))
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
        .with_workspace_setup(|cwd, fs| async move {
            fs.create_directory(
                &PathUri::from_abs_path(&cwd).join(".git")?,
                CreateDirectoryOptions {
                    recursive: false,
                    follow_symlinks: false,
                },
                /*sandbox*/ None,
            )
            .await?;
            Ok(())
        });
    // Current-repository usage resolves app-host Git metadata, and both turns below explicitly use
    // the local environment. Keep the fixture local instead of projecting a remote Windows cwd
    // onto the app host in Wine-exec runs.
    let test = builder.build(&server).await?;
    test.submit_turn_with_environments(
        "inspect current repository usage",
        Some(vec![local(test.config.cwd.clone())]),
    )
    .await?;

    let first_requests = first_mock.requests();
    assert_eq!(first_requests.len(), 2);
    assert!(
        first_requests[0].body_json()["tools"]
            .as_array()
            .is_some_and(|tools| tools
                .iter()
                .any(|tool| tool["name"].as_str() == Some("usage_stats")))
    );
    let summary = tool_output(&first_requests[1], "read-usage");
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
