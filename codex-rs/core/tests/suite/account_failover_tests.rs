use super::*;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
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
                    responses::ev_completed("listed"),
                ])),
                responses::sse_response(responses::sse(vec![
                    responses::ev_assistant_message("done", "Continued the same work"),
                    responses::ev_completed("done"),
                ])),
            ],
        )
        .await;
        let test = compaction_builder(Arc::clone(&home), &server, backend)
            .build_with_auto_env(&server)
            .await?;
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
                responses::ev_completed("image"),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("done", "Finished"),
                responses::ev_completed("done"),
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
                    responses::ev_completed("next"),
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
            responses::ev_completed("tool"),
        ])),
            usage_rejection(),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("backup-tool"),
                responses::ev_function_call("list", "account_management", "{\"action\":\"list\"}"),
                responses::ev_completed("backup-tool"),
            ])),
        ];
    for id in ["done", "next"] {
        script.push(responses::sse_response(responses::sse(vec![
            responses::ev_response_created(id),
            responses::ev_assistant_message(id, "continued"),
            responses::ev_completed(id),
        ])));
    }
    let mock = responses::mount_response_sequence(&server, script).await;
    let test = failover_builder(Arc::clone(&home), &server)
        .build_with_auto_env(&server)
        .await?;
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
