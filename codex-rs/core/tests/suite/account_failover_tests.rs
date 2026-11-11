use super::*;
use codex_core::TurnInputRequest;
use codex_core::UsageWorkAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use codex_usage::TimeRange;
use codex_usage::UsageFilter;
use codex_usage::UsageReader;
use codex_usage::WorkBindingTransition;
use codex_usage::WorkBindingUpdate;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use wiremock::MockServer;

async fn failover_home(server: &MockServer) -> Result<Arc<TempDir>> {
    failover_home_with_aliases(server, &["a", "b"]).await
}

async fn failover_home_with_aliases(server: &MockServer, aliases: &[&str]) -> Result<Arc<TempDir>> {
    let home = Arc::new(tempfile::tempdir()?);
    seed_profiles_with_aliases(home.path(), AuthMode::Chatgpt, aliases)?;
    RegistryStore::new(home.path()).compare_and_swap(
        /*expected_generation*/ 0,
        |registry| {
            registry.auto_selection.enabled = true;
            for (index, account) in registry.accounts.iter_mut().enumerate() {
                account.priority = 2000 - index as u32;
            }
        },
    )?;
    for alias in aliases {
        Mock::given(method("GET"))
            .and(path("/backend-api/wham/usage"))
            .and(header("chatgpt-account-id", format!("{alias}-workspace")))
            .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(json!({
                "plan_type": "pro", "rate_limit": {
                    "allowed": true, "limit_reached": false,
                    "primary_window": {"used_percent": 0, "limit_window_seconds": 18000,
                        "reset_after_seconds": 3600, "reset_at": 2000000000},
                    "secondary_window": {"used_percent": 10, "limit_window_seconds": 604800,
                        "reset_after_seconds": 3600, "reset_at": 2000000000}
                }
            })))
            .mount(server)
            .await;
    }
    Ok(home)
}

fn failover_builder(home: Arc<TempDir>, server: &MockServer) -> TestCodexBuilder {
    let backend = format!("{}/backend-api", server.uri());
    test_codex()
        .with_home(home)
        .with_auth(CodexAuth::from_api_key("legacy"))
        .with_config(move |config| {
            config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
            config.chatgpt_base_url = backend;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config
                .features
                .disable(Feature::UseAgentIdentity)
                .expect("disable agent identity");
        })
}

fn usage_error() -> Value {
    json!({"type": "usage_limit_reached", "plan_type": "pro",
        "resets_at": 2000000000, "limit_window_minutes": 10080})
}

fn usage_rejection() -> ResponseTemplate {
    ResponseTemplate::new(/*s*/ 429).set_body_json(json!({"error": usage_error()}))
}

async fn turn_events(test: &TestCodex, prompt: &str) -> Result<Vec<EventMsg>> {
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut events = Vec::new();
    wait_for_event(&test.codex, |event| {
        events.push(event.clone());
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(events)
}

fn error_summaries(events: &[EventMsg]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| {
            let EventMsg::Error(error) = event else {
                return None;
            };
            let message = error.message.chars().take(/*n*/ 240).collect::<String>();
            Some(format!("{:?}: {message}", error.codex_error_info))
        })
        .take(/*n*/ 4)
        .collect()
}

async fn declare_failover_work(test: &TestCodex) -> Result<WorkBindingTransition> {
    let thread_id = test.session_configured.session_id;
    let receipt = test
        .thread_manager
        .usage_work(
            thread_id,
            UsageWorkAction::Set {
                native_project_id: "failover-fixture-project".into(),
                workstream_id: Some("weekly-limit-workstream".into()),
                outcome_id: Some("continue-existing-job".into()),
                expected_revision: 0,
            },
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(receipt.revision, 1);
    let transition = receipt.transition.expect("persisted work declaration");
    assert_eq!(
        transition.update,
        WorkBindingUpdate {
            version: 1,
            native_project_id: Some("failover-fixture-project".into()),
            thread_id: thread_id.to_string(),
            workstream_id: Some("weekly-limit-workstream".into()),
            outcome_id: Some("continue-existing-job".into()),
            transition_sequence: 1,
            source: "agent_declared".into(),
            authority: "agent_declared".into(),
        }
    );
    Ok(transition)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ExpectedWorkUsage {
    model_requests: u64,
    tools: u64,
    retries: u64,
    provider_total_tokens: u64,
    missing_provider_totals: u64,
}

fn assert_failover_work_usage(
    test: &TestCodex,
    transition: &WorkBindingTransition,
    expected_accounts: &[ExpectedWorkUsage],
) -> Result<()> {
    assert_eq!(
        codex_usage::read_work_binding_transitions(test.home.path())?,
        vec![transition.clone()],
        "retries, account changes and compaction must not redeclare the job"
    );
    let ended_at_ms: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .try_into()?;
    let range = TimeRange::from_millis(transition.captured_at_ms, ended_at_ms.saturating_add(1))?;
    let reader = UsageReader::open(test.home.path())?;
    assert_eq!(reader.pending_entries(), 0);
    let accounts = RegistryStore::new(test.home.path()).read()?.accounts;
    assert_eq!(accounts.len(), expected_accounts.len());
    let total = expected_accounts
        .iter()
        .fold(ExpectedWorkUsage::default(), |sum, account| {
            ExpectedWorkUsage {
                model_requests: sum.model_requests + account.model_requests,
                tools: sum.tools + account.tools,
                retries: sum.retries + account.retries,
                provider_total_tokens: sum.provider_total_tokens + account.provider_total_tokens,
                missing_provider_totals: sum.missing_provider_totals
                    + account.missing_provider_totals,
            }
        });
    let filters = std::iter::once((None, total)).chain(
        accounts
            .iter()
            .zip(expected_accounts)
            .map(|(account, expected)| (Some(account.id.to_string()), *expected)),
    );
    for (account_id, expected) in filters {
        let filter = UsageFilter {
            account_id,
            thread_id: Some(test.session_configured.session_id.to_string()),
            ..UsageFilter::default()
        };
        let report = reader.performance_review(range, &filter)?;
        let tokens = report["tokens"]
            .as_array()
            .expect("token categories")
            .iter()
            .find(|row| row["category"] == "total_tokens")
            .expect("provider total category");
        assert_eq!(
            (
                &report["counts"]["model_requests"],
                &report["counts"]["tools"],
                &report["links"]["retryOperations"],
                &tokens["measuredTokens"],
                &report["coverage"]["modelRequestsWithoutProviderTotal"],
            ),
            (
                &json!(expected.model_requests),
                &json!(expected.tools),
                &json!(expected.retries),
                &json!(expected.provider_total_tokens),
                &json!(expected.missing_provider_totals),
            ),
            "account {:?}",
            filter.account_id
        );
        let rows = report["outcomes"]["rows"].as_array().expect("outcome rows");
        assert_eq!(rows.len(), 1, "account {:?}", filter.account_id);
        let row = &rows[0];
        assert_eq!(
            json!({
                "nativeProjectId": row["nativeProjectId"],
                "workstreamId": row["workstreamId"],
                "outcomeId": row["outcomeId"],
                "operations": row["effort"]["operations"],
                "retryOperations": row["effort"]["retryOperations"],
                "providerTotalTokens": row["effort"]["providerTotalTokens"]["measured"],
            }),
            json!({
                "nativeProjectId": "failover-fixture-project",
                "workstreamId": "weekly-limit-workstream",
                "outcomeId": "continue-existing-job",
                "operations": expected.model_requests + expected.tools,
                "retryOperations": expected.retries,
                "providerTotalTokens": expected.provider_total_tokens,
            }),
            "account {:?}",
            filter.account_id
        );
        assert_eq!(report["outcomes"]["totalRows"], 1);
        assert_eq!(report["outcomes"]["attributed"], row["effort"]);
        assert_eq!(
            report["workBindings"]["boundOperations"],
            expected.model_requests + expected.tools
        );
        assert_eq!(report["outcomes"]["unattributed"]["operations"], 0);
        assert_eq!(
            report["outcomes"]["unattributed"]["providerTotalTokens"]["measured"],
            0
        );
        // Provider-reported tokens do not fill missing rejected-response usage
        // or establish exact active time or a delivered outcome.
        assert_eq!(report["outcomes"]["coverage"], "partial");
        assert!(row["effort"]["providerTotalTokens"]["exact"].is_null());
        assert!(row["effort"]["activeAgentMs"]["exact"].is_null());
        assert!(row["effort"]["elapsedExecutionMs"]["exact"].is_null());
    }
    Ok(())
}

fn compaction_builder(
    home: Arc<TempDir>,
    server: &MockServer,
    backend: &'static str,
) -> TestCodexBuilder {
    failover_builder(home, server).with_config(move |config| {
        config.model_provider.name = if backend == "remote" {
            "OpenAI"
        } else {
            "OpenAI (test)"
        }
        .into();
        config.model_auto_compact_token_limit = Some(100_000);
        config.model_post_turn_compact_threshold_percent = 0;
        config
            .features
            .disable(Feature::AsyncCompaction)
            .expect("disable speculative compaction");
    })
}

fn compaction_success(backend: &str) -> ResponseTemplate {
    let output = if backend == "remote" {
        json!({"type":"response.output_item.done", "item":{
            "type":"compaction", "encrypted_content":"retained history"}})
    } else {
        responses::ev_assistant_message("summary", "Retained history")
    };
    responses::sse_response(responses::sse(vec![
        output,
        responses::ev_completed("compact"),
    ]))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_compaction_and_following_sampling_can_switch_accounts_in_the_same_turn()
-> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    for backend in ["local", "remote"] {
        let server = responses::start_mock_server().await;
        let home = failover_home_with_aliases(&server, &["a", "b", "c"]).await?;
        let mock = responses::mount_response_sequence(
            &server,
            vec![
                responses::sse_response(responses::sse(vec![
                    responses::ev_assistant_message("seed", "Before compaction"),
                    responses::ev_completed_with_tokens("seed", /*total_tokens*/ 200_000),
                ])),
                usage_rejection(),
                compaction_success(backend),
                usage_rejection(),
                responses::sse_response(responses::sse(vec![
                    responses::ev_function_call(
                        "list",
                        "account_management",
                        "{\"action\":\"list\"}",
                    ),
                    responses::ev_completed_with_tokens("listed", /*total_tokens*/ 13),
                ])),
                responses::sse_response(responses::sse(vec![
                    responses::ev_assistant_message("done", "Continued the same work"),
                    responses::ev_completed_with_tokens("done", /*total_tokens*/ 17),
                ])),
            ],
        )
        .await;
        let test = compaction_builder(Arc::clone(&home), &server, backend)
            .build_with_auto_env(&server)
            .await?;
        let binding = declare_failover_work(&test).await?;
        turn_events(&test, "Keep this history").await?;
        let events = turn_events(&test, "Preserve this pending input and finish").await?;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EventMsg::Error(_))),
            "{backend}: {:?}",
            error_summaries(&events)
        );
        let requests = mock.requests();
        assert_eq!(requests.len(), 6, "{backend}");
        assert_eq!(
            requests[1].input(),
            requests[2].input(),
            "compaction request must be unchanged"
        );
        assert_eq!(
            requests[3].input(),
            requests[4].input(),
            "sampling input must be unchanged"
        );
        assert!(requests[3].body_contains_text("Preserve this pending input and finish"));
        assert_eq!(
            management_output(&requests[5], "list")["routedAccount"],
            "c"
        );
        assert_eq!(
            requests
                .iter()
                .map(|request| request.header("authorization"))
                .collect::<Vec<_>>(),
            ["a", "a", "b", "b", "c", "c"]
                .map(|alias| Some(format!("Bearer {alias}-access")))
                .to_vec()
        );
        test.codex.shutdown_and_wait().await?;
        let records = codex_usage::UsageJournal::new(
            home.path()
                .join(codex_usage::USAGE_DIRECTORY_NAME)
                .join("events.jsonl"),
        )
        .records()?;
        let attempts = records
            .iter()
            .filter(|record| record.event.as_deref() == Some("model_request_started"))
            .collect::<Vec<_>>();
        assert_eq!(attempts.len(), 6);
        assert_eq!(
            (
                &attempts[2].retry_of_operation_id,
                &attempts[4].retry_of_operation_id
            ),
            (&attempts[1].operation_id, &attempts[3].operation_id)
        );
        assert_failover_work_usage(
            &test,
            &binding,
            &[
                ExpectedWorkUsage {
                    model_requests: 2,
                    tools: 0,
                    retries: 0,
                    provider_total_tokens: 200_000,
                    missing_provider_totals: 1,
                },
                ExpectedWorkUsage {
                    model_requests: 2,
                    tools: 0,
                    retries: 1,
                    provider_total_tokens: 0,
                    missing_provider_totals: 1,
                },
                ExpectedWorkUsage {
                    model_requests: 2,
                    tools: 1,
                    retries: 1,
                    provider_total_tokens: 30,
                    missing_provider_totals: 0,
                },
            ],
        )?;
    }
    Ok(())
}

#[test_case::test_case("local"; "local compaction")]
#[test_case::test_case("remote"; "remote compaction")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_compaction_failover_preserves_completed_tools_and_rebuilds_tool_auth(
    backend: &'static str,
) -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let home = failover_home(&server).await?;
    Mock::given(method("POST")).and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer b-access"))
        .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(json!({
            "created":1, "data":[{"b64_json":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="}]
        }))).expect(/*r*/ 1).mount(&server).await;
    let mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
            responses::ev_function_call("rename", "account_management", &json!({
                "action":"rename", "account":"a", "new_alias":"active", "expected_generation":1
            }).to_string()),
            responses::ev_completed_with_tokens("renamed", /*total_tokens*/ 200_000),
        ])),
            usage_rejection(),
            compaction_success(backend),
            responses::sse_response(responses::sse(vec![
                responses::ev_function_call_with_namespace(
                    "image",
                    "image_gen",
                    "imagegen",
                    "{\"prompt\":\"paint a blue whale\"}",
                ),
                responses::ev_completed_with_tokens("image", /*total_tokens*/ 23),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("done", "Finished"),
                responses::ev_completed_with_tokens("done", /*total_tokens*/ 29),
            ])),
        ],
    )
    .await;
    let mut extensions =
        codex_extension_api::ExtensionRegistryBuilder::<codex_core::config::Config>::new();
    codex_image_generation_extension::install(
        &mut extensions,
        codex_core::test_support::auth_manager_from_auth(CodexAuth::from_api_key("legacy")),
        |_config| None,
    );
    let test = compaction_builder(Arc::clone(&home), &server, backend)
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    let binding = declare_failover_work(&test).await?;
    let events = turn_events(&test, "Rename my profile, create an image, and finish").await?;
    let request_owners = mock
        .requests()
        .iter()
        .map(|request| request.header("authorization"))
        .collect::<Vec<_>>();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EventMsg::Error(_))),
        "{backend}: {:?}; request owners: {request_owners:?}",
        error_summaries(&events)
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 5, "{backend}");
    assert_eq!(requests[1].input(), requests[2].input());
    assert_eq!(RegistryStore::new(home.path()).read()?.generation, 2);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.header("authorization"))
            .collect::<Vec<_>>(),
        ["a", "a", "b", "b", "b"]
            .map(|alias| Some(format!("Bearer {alias}-access")))
            .to_vec()
    );
    server.verify().await;
    test.codex.shutdown_and_wait().await?;
    assert_failover_work_usage(
        &test,
        &binding,
        &[
            ExpectedWorkUsage {
                model_requests: 2,
                tools: 1,
                retries: 0,
                provider_total_tokens: 200_000,
                missing_provider_totals: 1,
            },
            ExpectedWorkUsage {
                model_requests: 3,
                tools: 1,
                retries: 1,
                provider_total_tokens: 52,
                missing_provider_totals: 0,
            },
        ],
    )?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_turn_compaction_failover_preserves_the_completed_answer_and_next_turn() -> Result<()>
{
    core_test_support::skip_if_no_network!(Ok(()));
    for backend in ["local", "remote"] {
        let server = responses::start_mock_server().await;
        let home = failover_home(&server).await?;
        let mock = responses::mount_response_sequence(
            &server,
            vec![
                responses::sse_response(responses::sse(vec![
                    responses::ev_assistant_message("answer", "Completed answer"),
                    responses::ev_completed_with_tokens("answer", /*total_tokens*/ 200_000),
                ])),
                usage_rejection(),
                compaction_success(backend),
                responses::sse_response(responses::sse(vec![
                    responses::ev_assistant_message("next", "Work continued"),
                    responses::ev_completed_with_tokens("next", /*total_tokens*/ 31),
                ])),
            ],
        )
        .await;
        let test = compaction_builder(home, &server, backend)
            .with_config(|config| {
                config.model_auto_compact_token_limit = Some(100_000);
                config.model_post_turn_compact_threshold_percent = 1;
            })
            .build_with_auto_env(&server)
            .await?;
        let binding = declare_failover_work(&test).await?;
        let events = turn_events(
            &test,
            "Finish this answer, then keep the conversation available",
        )
        .await?;
        assert!(events.iter().any(|event| matches!(event, EventMsg::TurnComplete(completed)
            if completed.error.is_none() && completed.last_agent_message.as_deref() == Some("Completed answer"))),
            "{backend}: {:?}", error_summaries(&events));
        let events = turn_events(&test, "Continue this chat").await?;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EventMsg::Error(_))),
            "{backend}: {:?}",
            error_summaries(&events)
        );
        let requests = mock.requests();
        assert_eq!(requests.len(), 4, "{backend}");
        assert_eq!(requests[1].input(), requests[2].input());
        assert_eq!(
            requests
                .iter()
                .map(|request| request.header("authorization"))
                .collect::<Vec<_>>(),
            ["a", "a", "b", "b"]
                .map(|alias| Some(format!("Bearer {alias}-access")))
                .to_vec()
        );
        test.codex.shutdown_and_wait().await?;
        assert_failover_work_usage(
            &test,
            &binding,
            &[
                ExpectedWorkUsage {
                    model_requests: 2,
                    tools: 0,
                    retries: 0,
                    provider_total_tokens: 200_000,
                    missing_provider_totals: 1,
                },
                ExpectedWorkUsage {
                    model_requests: 2,
                    tools: 0,
                    retries: 1,
                    provider_total_tokens: 31,
                    missing_provider_totals: 0,
                },
            ],
        )?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_compaction_refuses_started_responses_pins_and_unavailable_backups() -> Result<()>
{
    core_test_support::skip_if_no_network!(Ok(()));
    for backend in ["local", "remote"] {
        for case in ["started", "pinned", "unknown", "exhausted", "manual"] {
            let server = responses::start_mock_server().await;
            let home = failover_home(&server).await?;
            if case == "unknown" {
                Mock::given(method("GET"))
                    .and(path("/backend-api/wham/usage"))
                    .and(header("chatgpt-account-id", "b-workspace"))
                    .respond_with(ResponseTemplate::new(/*s*/ 503))
                    .with_priority(/*p*/ 1)
                    .mount(&server)
                    .await;
            }
            let mut script = vec![responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("seed", "Before compaction"),
                responses::ev_completed_with_tokens("seed", /*total_tokens*/ 200_000),
            ]))];
            if case == "started" {
                script.push(responses::sse_response(responses::sse(vec![
                    responses::ev_response_created("partial"),
                ])));
            }
            script.push(usage_rejection());
            if case == "exhausted" {
                script.push(usage_rejection());
            }
            let mock = responses::mount_response_sequence(&server, script).await;
            let test = compaction_builder(home, &server, backend)
                .with_config(move |config| {
                    if case == "pinned" {
                        config.process_account = Some("a".into());
                    }
                    if case == "started" {
                        config.model_provider.stream_max_retries = Some(1);
                    }
                })
                .build_with_auto_env(&server)
                .await?;
            turn_events(&test, "Seed the compacted history").await?;
            let events = if case == "manual" {
                test.codex
                    .submit(codex_protocol::protocol::Op::Compact)
                    .await?;
                let mut events = Vec::new();
                wait_for_event(&test.codex, |event| {
                    events.push(event.clone());
                    matches!(event, EventMsg::TurnComplete(_))
                })
                .await;
                events
            } else {
                let events = turn_events(&test, "Keep this pending input after rejection").await?;
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, EventMsg::UserMessage(message)
                    if message.message == "Keep this pending input after rejection"))
                        .count(),
                    1
                );
                events
            };
            assert!(events.iter().any(|event| matches!(event, EventMsg::Error(error)
                if error.codex_error_info == Some(codex_protocol::protocol::CodexErrorInfo::UsageLimitExceeded))),
                "{backend}/{case}: {:?}", error_summaries(&events));
            let expected = match case {
                "started" => vec!["a", "a", "a"],
                "exhausted" => vec!["a", "a", "b"],
                _ => vec!["a", "a"],
            };
            assert_eq!(
                mock.requests()
                    .iter()
                    .map(|request| request.header("authorization"))
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .map(|alias| Some(format!("Bearer {alias}-access")))
                    .collect::<Vec<_>>(),
                "{backend}/{case}"
            );
            test.codex.shutdown_and_wait().await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_limit_failover_keeps_completed_tools_and_usage_lineage() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let home = failover_home(&server).await?;
    let accounts = RegistryStore::new(home.path()).read()?.accounts;
    let mut script =
        vec![
            responses::sse_response(responses::sse(vec![
            responses::ev_response_created("tool"),
            responses::ev_function_call("rename", "account_management", &json!({
                "action":"rename", "account":"a", "new_alias":"active", "expected_generation":1
            }).to_string()),
            responses::ev_completed_with_tokens("tool", /*total_tokens*/ 11),
        ])),
            usage_rejection(),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("backup-tool"),
                responses::ev_function_call("list", "account_management", "{\"action\":\"list\"}"),
                responses::ev_completed_with_tokens("backup-tool", /*total_tokens*/ 13),
            ])),
        ];
    for (id, total_tokens) in [("done", 17), ("next", 19)] {
        script.push(responses::sse_response(responses::sse(vec![
            responses::ev_response_created(id),
            responses::ev_assistant_message(id, "continued"),
            responses::ev_completed_with_tokens(id, total_tokens),
        ])));
    }
    let mock = responses::mount_response_sequence(&server, script).await;
    let test = failover_builder(Arc::clone(&home), &server)
        .build_with_auto_env(&server)
        .await?;
    let binding = declare_failover_work(&test).await?;
    let events = turn_events(&test, "Rename my first profile, then finish the work").await?;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EventMsg::Error(_))),
        "unexpected turn errors: {:?}",
        error_summaries(&events)
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[1].input(), requests[2].input());
    assert_eq!(
        management_output(&requests[3], "list")["routedAccount"],
        "b"
    );
    let registry = RegistryStore::new(home.path()).read()?;
    assert_eq!(registry.generation, 2);
    assert_eq!(registry.accounts[0].alias.as_str(), "active");
    let events = turn_events(&test, "Keep using the available profile").await?;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EventMsg::Error(_))),
        "unexpected next-turn errors: {:?}",
        error_summaries(&events)
    );
    assert_eq!(
        mock.requests()
            .iter()
            .map(|request| request.header("authorization"))
            .collect::<Vec<_>>(),
        ["a", "a", "b", "b", "b"]
            .map(|alias| Some(format!("Bearer {alias}-access")))
            .to_vec()
    );
    test.codex.shutdown_and_wait().await?;
    let records = codex_usage::UsageJournal::new(
        home.path()
            .join(codex_usage::USAGE_DIRECTORY_NAME)
            .join("events.jsonl"),
    )
    .records()?;
    let attempts = records
        .iter()
        .filter(|record| record.event.as_deref() == Some("model_request_started"))
        .collect::<Vec<_>>();
    assert_eq!(
        attempts
            .iter()
            .map(|record| record.account_id.clone())
            .collect::<Vec<_>>(),
        [0, 0, 1, 1, 1]
            .map(|index| Some(accounts[index].id.to_string()))
            .to_vec()
    );
    assert_eq!(
        (
            &attempts[1].turn_id,
            &attempts[2].turn_id,
            &attempts[2].retry_of_operation_id
        ),
        (
            &attempts[0].turn_id,
            &attempts[0].turn_id,
            &attempts[1].operation_id
        )
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.event.as_deref() == Some("turn_finished"))
            .count(),
        2
    );
    assert_failover_work_usage(
        &test,
        &binding,
        &[
            ExpectedWorkUsage {
                model_requests: 2,
                tools: 1,
                retries: 0,
                provider_total_tokens: 11,
                missing_provider_totals: 1,
            },
            ExpectedWorkUsage {
                model_requests: 3,
                tools: 1,
                retries: 1,
                provider_total_tokens: 49,
                missing_provider_totals: 0,
            },
        ],
    )?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_limit_failover_respects_pins_manual_external_unknown_and_exhausted_profiles()
-> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    for case in [
        "pinned",
        "manual",
        "external",
        "unknown",
        "exhausted",
        "partial",
        "created",
    ] {
        let server = responses::start_mock_server().await;
        let home = failover_home(&server).await?;
        if case == "manual" {
            RegistryStore::new(home.path()).compare_and_swap(
                /*expected_generation*/ 1,
                |registry| registry.auto_selection.enabled = false,
            )?;
        }
        if case == "unknown" {
            Mock::given(method("GET"))
                .and(path("/backend-api/wham/usage"))
                .and(header("chatgpt-account-id", "b-workspace"))
                .respond_with(ResponseTemplate::new(/*s*/ 503))
                .with_priority(/*p*/ 1)
                .mount(&server)
                .await;
        }
        let mut script = Vec::new();
        if matches!(case, "partial" | "created") {
            let mut events = vec![responses::ev_response_created("partial")];
            if case == "partial" {
                events.push(responses::ev_assistant_message(
                    "partial-message",
                    "Already visible",
                ));
            }
            script.push(responses::sse_response(responses::sse(events)));
        }
        script.push(usage_rejection());
        if case == "exhausted" {
            script.push(usage_rejection());
        }
        let mock = responses::mount_response_sequence(&server, script).await;
        let test = failover_builder(home, &server)
            .with_config(move |config| {
                if case == "pinned" {
                    config.process_account = Some("a".to_string());
                }
                if case == "external" {
                    config.model_provider.experimental_bearer_token = Some("external".into());
                }
                if matches!(case, "partial" | "created") {
                    config.model_provider.stream_max_retries = Some(1);
                }
            })
            .build_with_auto_env(&server)
            .await?;
        let events = turn_events(
            &test,
            "Continue only when the selected account can safely change",
        )
        .await?;
        assert!(events.iter().any(|event| matches!(event, EventMsg::Error(error)
            if error.codex_error_info == Some(codex_protocol::protocol::CodexErrorInfo::UsageLimitExceeded))),
            "{case}: expected a usage-limit error; observed errors: {:?}", error_summaries(&events));
        let expected = match case {
            "external" => vec![Some("Bearer external".to_string())],
            "exhausted" => vec![
                Some("Bearer a-access".to_string()),
                Some("Bearer b-access".to_string()),
            ],
            "partial" | "created" => vec![Some("Bearer a-access".to_string()); 2],
            _ => vec![Some("Bearer a-access".to_string())],
        };
        assert_eq!(
            mock.requests()
                .iter()
                .map(|request| request.header("authorization"))
                .collect::<Vec<_>>(),
            expected,
            "{case}"
        );
        test.codex.shutdown_and_wait().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_usage_limit_failover_requires_no_response_items() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    for partial in [false, true] {
        let probes = responses::start_mock_server().await;
        let home = failover_home(&probes).await?;
        let mut rejection = Vec::new();
        if partial {
            rejection.push(responses::ev_message_item_added("partial", ""));
            rejection.push(responses::ev_output_text_delta("Already visible"));
        }
        rejection.push(json!({"type":"error", "status":429, "error":usage_error(),
            "headers":{"x-codex-secondary-used-percent":"100", "x-codex-secondary-window-minutes":"10080",
                "x-codex-secondary-reset-at":"2000000000"}}));
        let mut connections = vec![vec![
            vec![
                responses::ev_response_created("prewarm"),
                responses::ev_completed("prewarm"),
            ],
            rejection,
        ]];
        if !partial {
            connections.push(vec![vec![
                responses::ev_response_created("backup"),
                responses::ev_assistant_message("backup-message", "continued"),
                responses::ev_completed("backup"),
            ]]);
        }
        let server = responses::start_websocket_server(connections).await;
        let test = failover_builder(home, &probes)
            .build_with_websocket_server(&server)
            .await?;
        let events = turn_events(&test, "Finish this same turn after a weekly rejection").await?;
        assert_eq!(
            events
                .iter()
                .any(|event| matches!(event, EventMsg::Error(_))),
            partial,
            "partial={partial}: observed errors: {:?}",
            error_summaries(&events)
        );
        let expected = if partial { vec!["a"] } else { vec!["a", "b"] };
        assert_eq!(
            server
                .handshakes()
                .iter()
                .map(|handshake| handshake.header("authorization"))
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|alias| Some(format!("Bearer {alias}-access")))
                .collect::<Vec<_>>()
        );
        if !partial {
            assert_eq!(
                server.connections()[1][0].body_json()["previous_response_id"],
                Value::Null
            );
        }
        test.codex.shutdown_and_wait().await?;
        server.shutdown().await;
    }
    Ok(())
}
