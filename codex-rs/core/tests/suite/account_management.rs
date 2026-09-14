use std::sync::Arc;

use anyhow::Result;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::CodexAuth;
use codex_login::migrate_legacy_auth_if_needed;
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
async fn agent_manages_existing_profiles_without_replacing_its_turn_lease() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let home = Arc::new(TempDir::new()?);
    let credential = "profile-management-secret-must-stay-private";
    save_auth(
        home.path(),
        &legacy_auth(credential),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?;
    migrate_legacy_auth_if_needed(
        home.path(),
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
    let before = codex_login::read_managed_accounts(&test.config.auth_config())?;
    let beta = codex_account_registry::AccountMetadata::new(
        "beta".parse()?,
        AuthMode::ApiKey,
        chrono::Utc::now(),
    );
    codex_login::ProfileAuthStorage::new(
        home.path(),
        beta.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?
    .save(&legacy_auth("secondary-private-key"))?;
    codex_account_registry::RegistryStore::new(home.path())
        .compare_and_swap(before.generation, |registry| {
            registry.accounts.push(beta.clone())
        })?;
    let initial = codex_login::read_managed_accounts(&test.config.auth_config())?;
    let generation = initial.generation;
    let mode = if initial.auto_selection_enabled {
        "disabled"
    } else {
        "enabled"
    };
    let actions = [
        ("list", json!({"action":"list"})),
        (
            "rename",
            json!({"action":"rename","account":"default","new_alias":"primary","expected_generation":generation}),
        ),
        (
            "disable",
            json!({"action":"disable","account":"primary","expected_generation":generation + 1}),
        ),
        (
            "enable",
            json!({"action":"enable","account":"primary","expected_generation":generation + 2}),
        ),
        (
            "default",
            json!({"action":"set_default","account":"primary","expected_generation":generation + 3}),
        ),
        (
            "auto",
            json!({"action":"set_auto_selection","mode":mode,"expected_generation":generation + 4}),
        ),
    ];
    let mut events = actions
        .iter()
        .map(|(id, args)| {
            sse(vec![
                ev_response_created(id),
                ev_function_call(id, "account_management", &args.to_string()),
                ev_completed(id),
            ])
        })
        .collect::<Vec<_>>();
    events.push(sse(vec![
        ev_response_created("done"),
        ev_assistant_message("done-message", "done"),
        ev_completed("done"),
    ]));
    let mocked = responses::mount_sse_sequence(&server, events).await;
    test.submit_turn_with_permission_profile("Rename my default profile, disable and re-enable it, select it and toggle automatic selection", PermissionProfile::read_only()).await?;
    let requests = mocked.requests();
    assert_eq!(requests.len(), 7);
    let renamed = tool_output(&requests[2], "rename");
    let disabled = tool_output(&requests[3], "disable");
    let enabled = tool_output(&requests[4], "enable");
    let selected = tool_output(&requests[5], "default");
    let automatic = tool_output(&requests[6], "auto");
    assert_eq!(
        json!({"alias":renamed["account"]["alias"],"routed":renamed["routedAccount"],"disabledDefault":disabled["defaultAccount"],"enabledDefault":enabled["defaultAccount"],"defaultChanged":selected["changed"],"automatic":automatic["autoSelectionEnabled"]}),
        json!({"alias":"primary","routed":"primary","disabledDefault":"beta","enabledDefault":"beta","defaultChanged":true,"automatic":!initial.auto_selection_enabled})
    );
    assert_eq!(enabled["account"]["isCurrentTurn"], true);
    let final_state = codex_login::read_managed_accounts(&test.config.auth_config())?;
    assert_eq!(
        final_state
            .accounts
            .iter()
            .find(|account| account.is_default)
            .unwrap()
            .account_id,
        initial
            .accounts
            .iter()
            .find(|account| account.is_default)
            .unwrap()
            .account_id
    );
    assert_eq!(final_state.generation, generation + 5);
    let output = serde_json::to_string(&(renamed, disabled, enabled, selected, automatic))?;
    assert!(!output.contains(credential));
    assert!(!output.contains("email"));
    Ok(())
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
    migrate_legacy_auth_if_needed(
        home.path(),
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
