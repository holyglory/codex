//! A usage rejection must not terminate a chat that has another eligible account.

use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::write_chatgpt_auth;
use chrono::Utc;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_config::types::AuthCredentialsStoreMode;
use codex_features::Feature;
use codex_login::AuthKeyringBackendKind;
use codex_login::ProfileAuthStorage;
use codex_login::load_auth_dot_json;
use codex_protocol::auth::AuthMode;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[derive(Clone, Copy)]
enum LimitPhase {
    Sampling,
    PreTurn,
    PostTurnGoal,
}

#[test_case::test_case(LimitPhase::Sampling; "sampling")]
#[test_case::test_case(LimitPhase::PreTurn; "required pre turn compaction")]
#[test_case::test_case(LimitPhase::PostTurnGoal; "post turn goal continuation")]
#[tokio::test]
async fn weekly_limit_continues_the_same_chat_turn_on_another_account(
    phase: LimitPhase,
) -> Result<()> {
    let pre_turn = matches!(phase, LimitPhase::PreTurn);
    let post_turn_goal = matches!(phase, LimitPhase::PostTurnGoal);
    let server = responses::start_mock_server().await;
    let home = TempDir::new()?;
    let fixture_home = TempDir::new()?;
    let mut accounts = Vec::new();
    for alias in ["alpha", "beta"] {
        let mut account = AccountMetadata::new(alias.parse()?, AuthMode::Chatgpt, Utc::now());
        if alias == "alpha" {
            account.priority += 1;
        }
        write_chatgpt_auth(
            fixture_home.path(),
            ChatGptAuthFixture::new(format!("{alias}-access"))
                .account_id(format!("{alias}-workspace"))
                .chatgpt_account_id(format!("{alias}-workspace"))
                .plan_type("pro"),
            AuthCredentialsStoreMode::File,
        )?;
        let auth = load_auth_dot_json(
            fixture_home.path(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )?
        .expect("fixture credentials");
        ProfileAuthStorage::new(
            home.path(),
            account.id.clone(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )?
        .save(&auth)?;
        accounts.push(account);
    }
    let mut registry = AccountRegistry {
        default_account_id: Some(accounts[0].id.clone()),
        accounts,
        ..AccountRegistry::default()
    };
    registry.auto_selection.enabled = true;
    RegistryStore::new(home.path()).create(&registry)?;
    MockResponsesConfig::new(&server.uri())
        .with_provider_name("OpenAI")
        .with_provider_config("requires_openai_auth = true\nsupports_websockets = false")
        .with_root_config(&format!(
            "cli_auth_credentials_store = \"file\"\nchatgpt_base_url = \"{}/backend-api\"",
            server.uri()
        ))
        .with_root_config(if pre_turn {
            "model_auto_compact_token_limit = 100000\nmodel_post_turn_compact_threshold_percent = 0"
        } else if post_turn_goal {
            "model_auto_compact_token_limit = 100000\nmodel_post_turn_compact_threshold_percent = 1"
        } else {
            ""
        })
        .enable_feature(Feature::Goals)
        .disable_feature(Feature::UseAgentIdentity)
        .disable_feature(Feature::AsyncCompaction)
        .disable_feature(Feature::EnableRequestCompression)
        .write(home.path())?;

    // Capacity can change after the pre-turn probe, while a chat is still running.
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true, "limit_reached": false,
                "primary_window": {"used_percent": 10, "limit_window_seconds": 18000,
                    "reset_after_seconds": 3600, "reset_at": 2000000000},
                "secondary_window": {"used_percent": 10, "limit_window_seconds": 604800,
                    "reset_after_seconds": 3600, "reset_at": 2000000000}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer alpha-access"))
        .respond_with({
            let requests = std::sync::atomic::AtomicUsize::new(0);
            move |_: &wiremock::Request| {
                let index = requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if post_turn_goal && index == 0 {
                    return responses::sse_response(responses::sse(vec![
                        responses::ev_function_call("goal", "create_goal", "{\"objective\":\"Continue this work after a weekly limit, then complete the goal\"}"),
                        responses::ev_completed("created-goal"),
                    ]));
                }
                if post_turn_goal && index == 1 {
                    return responses::sse_response(responses::sse(vec![
                        responses::ev_assistant_message("progress", "Work continued"),
                        responses::ev_completed_with_tokens("progress", /*total_tokens*/ 200_000),
                    ]));
                }
                if pre_turn && index == 0 {
                    return responses::sse_response(responses::sse(vec![
                        responses::ev_assistant_message("seed", "Before compaction"),
                        responses::ev_completed_with_tokens("seed", /*total_tokens*/ 200_000),
                    ]));
                }
                ResponseTemplate::new(/*s*/ 429).set_body_json(json!({
                    "error": {"type": "usage_limit_reached", "plan_type": "pro",
                        "resets_at": 2000000000, "limit_window_minutes": 10080}
                }))
            }
        })
        .expect(/*r*/ if post_turn_goal { 3 } else if pre_turn { 2 } else { 1 })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer beta-access"))
        .respond_with({
            let requests = std::sync::atomic::AtomicUsize::new(0);
            move |request: &wiremock::Request| {
                let is_compaction = request
                    .body_json::<serde_json::Value>()
                    .expect("request JSON")["input"]
                    .as_array()
                    .expect("request input")
                    .iter()
                    .any(|item| item["type"] == "compaction_trigger");
                if is_compaction {
                    return responses::sse_response(responses::sse(vec![
                        json!({"type":"response.output_item.done", "item": {
                        "type":"compaction", "encrypted_content":"retained history"}}),
                        responses::ev_completed("compact"),
                    ]));
                }
                if post_turn_goal && requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
                {
                    return responses::sse_response(responses::sse(vec![
                        responses::ev_function_call(
                            "complete",
                            "update_goal",
                            "{\"status\":\"complete\"}",
                        ),
                        responses::ev_completed("completed-goal"),
                    ]));
                }
                responses::sse_response(responses::sse(vec![
                    responses::ev_response_created("backup-response"),
                    responses::ev_assistant_message("backup-message", "Work continued"),
                    responses::ev_completed("backup-response"),
                ]))
            }
        })
        .expect(/*r*/ if pre_turn || post_turn_goal { 3 } else { 2 })
        .mount(&server)
        .await;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized()
        .await?;
    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?
        .thread;
    if pre_turn {
        let _: TurnStartResponse = app_server
            .request(|request_id| ClientRequest::TurnStart {
                request_id,
                params: TurnStartParams {
                    thread_id: thread.id.clone(),
                    input: vec![UserInput::Text {
                        text: "Seed the history".into(),
                        text_elements: Vec::new(),
                    }],
                    ..Default::default()
                },
            })
            .await?;
        let seeded: TurnCompletedNotification =
            app_server.read_notification("turn/completed").await?;
        assert_eq!(seeded.turn.status, TurnStatus::Completed);
    }
    let TurnStartResponse { turn } = app_server
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: if post_turn_goal { "Create the goal, continue this work after a weekly limit, then mark it complete" } else { "Continue my work" }.into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    let completed: TurnCompletedNotification = timeout(
        Duration::from_secs(/*secs*/ 30),
        app_server.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(
        (
            &completed.thread_id,
            &completed.turn.id,
            &completed.turn.status,
            &completed.turn.error
        ),
        (&thread.id, &turn.id, &TurnStatus::Completed, &None)
    );
    let messages = completed
        .turn
        .items
        .iter()
        .filter_map(|item| match &item.payload {
            ThreadItemPayload::AgentMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(messages, vec!["Work continued"]);
    assert!(
        !app_server
            .pending_notification_methods()
            .iter()
            .any(|method| method == "error")
    );
    if post_turn_goal {
        let continued: TurnCompletedNotification = timeout(
            Duration::from_secs(/*secs*/ 30),
            app_server.read_notification("turn/completed"),
        )
        .await??;
        assert_eq!(
            (
                &continued.thread_id,
                &continued.turn.status,
                &continued.turn.error
            ),
            (&thread.id, &TurnStatus::Completed, &None)
        );
        assert_ne!(continued.turn.id, turn.id);
        let request = app_server
            .send_raw_request("thread/goal/get", Some(json!({"threadId":thread.id})))
            .await?;
        let goal: codex_app_server_protocol::ThreadGoalGetResponse =
            app_server.read_response(request).await?;
        assert_eq!(
            goal.goal
                .expect("active goal must continue to completion")
                .status,
            codex_app_server_protocol::ThreadGoalStatus::Complete
        );
        server.verify().await;
        app_server.shutdown_gracefully().await?;
        return Ok(());
    }
    let TurnStartResponse { turn: next_turn } = app_server
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "Keep working in this chat".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    let next_completed: TurnCompletedNotification = timeout(
        Duration::from_secs(/*secs*/ 30),
        app_server.read_notification("turn/completed"),
    )
    .await??;
    assert_ne!(next_turn.id, turn.id);
    assert_eq!(
        (
            &next_completed.thread_id,
            &next_completed.turn.id,
            &next_completed.turn.status,
            &next_completed.turn.error
        ),
        (&thread.id, &next_turn.id, &TurnStatus::Completed, &None)
    );
    assert!(
        !app_server
            .pending_notification_methods()
            .iter()
            .any(|method| method == "error")
    );
    let requests = server.received_requests().await.expect("captured requests");
    let owners = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/responses")
        .map(|request| {
            request.headers["authorization"]
                .to_str()
                .map(str::to_string)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let expected = if pre_turn {
        vec!["alpha", "alpha", "beta", "beta", "beta"]
    } else {
        vec!["alpha", "beta", "beta"]
    };
    assert_eq!(
        owners,
        expected
            .iter()
            .map(|alias| format!("Bearer {alias}-access"))
            .collect::<Vec<_>>()
    );
    server.verify().await;
    app_server.shutdown_gracefully().await?;
    Ok(())
}
