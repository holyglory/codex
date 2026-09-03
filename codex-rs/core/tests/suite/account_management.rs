use std::sync::Arc;

use anyhow::Result;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::CodexAuth;
use codex_login::save_auth;
use codex_protocol::auth::AuthMode;
use codex_protocol::models::PermissionProfile;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

fn legacy_auth(secret: &str) -> AuthDotJson {
    AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some(secret.to_string()),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    }
}

fn tool_output(request: &responses::ResponsesRequest, call_id: &str) -> Value {
    let content = request
        .function_call_output_text(call_id)
        .expect("text function output");
    serde_json::from_str(&content).expect("JSON tool output")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_lists_and_mutates_priorities_without_credential_exposure() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "set-priority",
                    "account_management",
                    &json!({
                        "action": "set_priority",
                        "account": "default",
                        "priority": 900
                    })
                    .to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call(
                    "list-accounts",
                    "account_management",
                    &json!({"action": "list"}).to_string(),
                ),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;
    let home = Arc::new(TempDir::new()?);
    let credential = "must-never-enter-tool-output";
    save_auth(
        home.path(),
        &legacy_auth(credential),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?;
    let mut builder = test_codex()
        .with_home(Arc::clone(&home))
        .with_auth(CodexAuth::from_api_key("model-route"))
        .with_config(|config| {
            config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
        });
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_turn_with_permission_profile(
        "inspect and adjust the local account priority",
        PermissionProfile::read_only(),
    )
    .await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[0].body_json()["tools"]
            .as_array()
            .is_some_and(|tools| tools
                .iter()
                .any(|tool| { tool["name"].as_str() == Some("account_management") }))
    );
    let mutation = tool_output(&requests[1], "set-priority");
    assert_eq!(mutation["priority"], 900);
    assert_eq!(mutation["changedCount"], 1);
    let listed = tool_output(&requests[2], "list-accounts");
    assert_eq!(listed["priorityOrder"], "higherFirst");
    assert_eq!(listed["routedAccount"], "default");
    assert_eq!(listed["accounts"][0]["alias"], "default");
    assert_eq!(listed["accounts"][0]["priority"], 900);
    assert_eq!(listed["accounts"][0]["authenticated"], true);
    assert_eq!(listed["accounts"][0]["isCurrentTurn"], true);
    let encoded = serde_json::to_string(&(mutation, listed))?;
    assert!(!encoded.contains(credential));
    assert!(!encoded.contains("email"));
    Ok(())
}
