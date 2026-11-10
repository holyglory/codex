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

#[tokio::test]
async fn weekly_limit_continues_the_same_chat_turn_on_another_account() -> Result<()> {
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
        .disable_feature(Feature::UseAgentIdentity)
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
        .respond_with(ResponseTemplate::new(/*s*/ 429).set_body_json(json!({
            "error": {"type": "usage_limit_reached", "plan_type": "pro",
                "resets_at": 2000000000, "limit_window_minutes": 10080}
        })))
        .expect(/*r*/ 1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer beta-access"))
        .respond_with(responses::sse_response(responses::sse(vec![
            responses::ev_response_created("backup-response"),
            responses::ev_assistant_message("backup-message", "Work continued"),
            responses::ev_completed("backup-response"),
        ])))
        .expect(/*r*/ 2)
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
    let TurnStartResponse { turn } = app_server
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "Continue my work".into(),
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
    assert_eq!(
        owners,
        vec![
            "Bearer alpha-access",
            "Bearer beta-access",
            "Bearer beta-access"
        ]
    );
    app_server.shutdown_gracefully().await?;
    Ok(())
}
