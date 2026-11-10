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
    let home = Arc::new(tempfile::tempdir()?);
    seed_profiles(home.path(), AuthMode::Chatgpt)?;
    RegistryStore::new(home.path()).compare_and_swap(
        /*expected_generation*/ 0,
        |registry| {
            registry.auto_selection.enabled = true;
            registry.accounts[0].priority = 2000;
        },
    )?;
    for alias in ["a", "b"] {
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
