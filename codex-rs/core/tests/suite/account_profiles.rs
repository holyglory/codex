use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use base64::Engine as _;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_config::types::AuthCredentialsStoreMode;
use codex_features::Feature;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::CodexAuth;
use codex_login::ProfileAuthStorage;
use codex_login::token_data::TokenData;
use codex_login::token_data::parse_chatgpt_jwt_claims;
use codex_protocol::auth::AuthMode;
use codex_protocol::models::PermissionProfile;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[path = "account_failover_tests.rs"]
mod failover;

fn seed_profiles(home: &Path, mode: AuthMode) -> Result<Vec<AccountMetadata>> {
    let mut accounts = Vec::new();
    for alias in ["a", "b"] {
        let account = AccountMetadata::new(alias.parse()?, mode, chrono::Utc::now());
        let mut auth = AuthDotJson {
            auth_mode: Some(mode),
            openai_api_key: Some(format!("{alias}-key")),
            tokens: None,
            last_refresh: Some(chrono::Utc::now()),
            agent_identity: None,
            personal_access_token: None,
            bedrock_api_key: None,
            bedrock_access_keys: None,
        };
        if mode == AuthMode::Chatgpt {
            let payload = json!({"https://api.openai.com/auth": {
                "chatgpt_plan_type": "pro", "chatgpt_account_id": format!("{alias}-workspace")
            }});
            let jwt = format!(
                "e30.{}.signature",
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(serde_json::to_vec(&payload)?)
            );
            auth.openai_api_key = None;
            auth.tokens = Some(TokenData {
                id_token: parse_chatgpt_jwt_claims(&jwt)?,
                access_token: format!("{alias}-access"),
                refresh_token: format!("{alias}-refresh"),
                account_id: Some(format!("{alias}-workspace")),
                ..TokenData::default()
            });
        }
        ProfileAuthStorage::new(
            home,
            account.id.clone(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )?
        .save(&auth)?;
        accounts.push(account);
    }
    RegistryStore::new(home).create(&AccountRegistry {
        accounts: accounts.clone(),
        default_account_id: Some(accounts[0].id.clone()),
        ..AccountRegistry::default()
    })?;
    Ok(accounts)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inference_uses_current_default_and_preserves_a_process_pin() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    for pin in [None, Some("a")] {
        let server = responses::start_mock_server().await;
        let home = Arc::new(tempfile::tempdir()?);
        let accounts = seed_profiles(home.path(), AuthMode::ApiKey)?;
        let response = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_response_created("one"),
                    responses::ev_completed("one"),
                ]),
                responses::sse(vec![
                    responses::ev_response_created("two"),
                    responses::ev_completed("two"),
                ]),
            ],
        )
        .await;
        let test = test_codex()
            .with_home(Arc::clone(&home))
            .with_auth(CodexAuth::from_api_key("legacy"))
            .with_config(move |config| {
                config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
                config.process_account = pin.map(str::to_string);
            })
            .build_with_auto_env(&server)
            .await?;
        test.submit_turn("first").await?;
        RegistryStore::new(home.path()).compare_and_swap(
            /*expected_generation*/ 0,
            |registry| {
                registry.default_account_id = Some(accounts[1].id.clone());
            },
        )?;
        test.submit_turn("second").await?;
        assert_eq!(
            response
                .requests()
                .iter()
                .map(|request| request.header("authorization"))
                .collect::<Vec<_>>(),
            vec![
                Some("Bearer a-key".to_string()),
                Some(format!(
                    "Bearer {}-key",
                    if pin.is_some() { "a" } else { "b" }
                ))
            ]
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_selection_probes_capacity_and_uses_the_selected_inference_owner() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let home = Arc::new(tempfile::tempdir()?);
    seed_profiles(home.path(), AuthMode::Chatgpt)?;
    RegistryStore::new(home.path()).compare_and_swap(
        /*expected_generation*/ 0,
        |registry| {
            registry.auto_selection.enabled = true;
        },
    )?;
    for (alias, used) in [("a", 100), ("b", 10)] {
        Mock::given(method("GET"))
            .and(path("/backend-api/wham/usage"))
            .and(header("chatgpt-account-id", format!("{alias}-workspace")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "plan_type": "pro", "rate_limit": {
                    "allowed": used < 100, "limit_reached": used == 100,
                    "primary_window": {"used_percent": used, "limit_window_seconds": 18000,
                        "reset_after_seconds": 3600, "reset_at": 2000000000}
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
    }
    let response = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("selected"),
            responses::ev_completed("selected"),
        ]),
    )
    .await;
    let backend = format!("{}/backend-api", server.uri());
    let test = test_codex()
        .with_home(home)
        .with_auth(CodexAuth::from_api_key("legacy"))
        .with_config(move |config| {
            config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
            config.chatgpt_base_url = backend;
            config
                .features
                .disable(Feature::UseAgentIdentity)
                .expect("disable agent identity for the account fixture");
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("use available included capacity").await?;
    let request = response.single_request();
    assert_eq!(
        (
            request.header("authorization"),
            request.header("chatgpt-account-id")
        ),
        (
            Some("Bearer b-access".to_string()),
            Some("b-workspace".to_string())
        )
    );
    Ok(())
}

fn management_output(request: &responses::ResponsesRequest, call_id: &str) -> serde_json::Value {
    serde_json::from_str(
        &request
            .function_call_output_text(call_id)
            .expect("account tool output"),
    )
    .expect("account JSON result")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_tool_mutations_preserve_turn_lease_and_apply_to_the_next_turn() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let home = Arc::new(tempfile::tempdir()?);
    let accounts = seed_profiles(home.path(), AuthMode::ApiKey)?;
    let actions = [
        ("list", json!({"action":"list","limit":1})),
        (
            "rename",
            json!({"action":"rename","account":"a","new_alias":"primary","expected_generation":0}),
        ),
        (
            "credits",
            json!({"action":"set_credit_usage","account":"primary","mode":"enabled","expected_generation":1}),
        ),
        (
            "disable",
            json!({"action":"disable","account":"primary","expected_generation":2}),
        ),
        (
            "stale",
            json!({"action":"rename","account":"b","new_alias":"wrong","expected_generation":2}),
        ),
        (
            "enable",
            json!({"action":"enable","account":"primary","expected_generation":3}),
        ),
        (
            "default",
            json!({"action":"set_default","account":"primary","expected_generation":4}),
        ),
        (
            "auto-on",
            json!({"action":"set_auto_selection","mode":"enabled","expected_generation":5}),
        ),
        (
            "auto-off",
            json!({"action":"set_auto_selection","mode":"disabled","expected_generation":6}),
        ),
        (
            "priority",
            json!({"action":"set_priority","account":"b","priority":1100}),
        ),
        (
            "priorities",
            json!({"action":"set_all_priorities","priority":42,"expected_generation":8}),
        ),
        ("page", json!({"action":"list","offset":1,"limit":1})),
        (
            "future-default",
            json!({"action":"set_default","account":"b","expected_generation":9}),
        ),
        ("final-list", json!({"action":"list","limit":1})),
    ];
    let mut events = actions
        .iter()
        .map(|(id, action)| {
            responses::sse(vec![
                responses::ev_response_created(id),
                responses::ev_function_call(id, "account_management", &action.to_string()),
                responses::ev_completed(id),
            ])
        })
        .collect::<Vec<_>>();
    for id in ["done", "next"] {
        events.push(responses::sse(vec![
            responses::ev_response_created(id),
            responses::ev_assistant_message(id, "done"),
            responses::ev_completed(id),
        ]));
    }
    let mock = responses::mount_sse_sequence(&server, events).await;
    let test = test_codex()
        .with_home(Arc::clone(&home))
        .with_auth(CodexAuth::from_api_key("legacy"))
        .with_config(|config| {
            config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn_with_permission_profile(
        "Manage my existing profiles and credit permissions",
        PermissionProfile::read_only(),
    )
    .await?;
    let first = mock.requests();
    assert_eq!(first.len(), actions.len() + 1);
    assert!(
        first
            .iter()
            .all(|request| request.header("authorization").as_deref() == Some("Bearer a-key"))
    );
    let result = |id| {
        let position = actions
            .iter()
            .position(|(action, _)| *action == id)
            .expect("action position");
        management_output(&first[position + 1], id)
    };
    assert_eq!(result("list")["nextOffset"], 1);
    assert_eq!(result("rename")["routedAccount"], "primary");
    assert_eq!(result("credits")["account"]["creditUsageEnabled"], true);
    assert_eq!(result("disable")["defaultAccount"], "b");
    assert_eq!(result("enable")["account"]["isCurrentTurn"], true);
    assert_eq!(result("auto-on")["autoSelectionEnabled"], true);
    assert_eq!(result("auto-off")["autoSelectionEnabled"], false);
    assert_eq!(result("priorities")["changedCount"], 2);
    assert_eq!(result("page")["accounts"][0]["alias"], "primary");
    assert_eq!(result("page")["nextOffset"], Value::Null);
    assert_eq!(result("final-list")["accounts"][0]["isCurrentTurn"], false);
    let stale_position = actions
        .iter()
        .position(|(id, _)| *id == "stale")
        .expect("stale action");
    assert_eq!(
        first[stale_position + 1]
            .function_call_output_text("stale")
            .as_deref(),
        Some("account registry changed concurrently")
    );
    let expected = codex_login::read_managed_accounts(&test.config.auth_config())?;
    assert_eq!(expected.generation, 10);
    assert_eq!(
        expected
            .accounts
            .iter()
            .map(|account| (
                account.alias.as_str(),
                account.priority,
                account.credit_usage_enabled
            ))
            .collect::<Vec<_>>(),
        vec![("b", 42, false), ("primary", 42, true)]
    );
    let registry = RegistryStore::new(home.path()).read()?;
    assert_eq!(registry.default_account_id, Some(accounts[1].id.clone()));
    let output = actions
        .iter()
        .enumerate()
        .filter_map(|(index, (id, _))| first[index + 1].function_call_output_text(id))
        .collect::<Vec<_>>()
        .join("");
    assert!(!output.contains("a-key") && !output.contains("b-key") && !output.contains("email"));
    test.submit_turn("Use the new default on this turn").await?;
    assert_eq!(
        mock.requests()
            .last()
            .expect("next turn request")
            .header("authorization")
            .as_deref(),
        Some("Bearer b-key")
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_tool_refreshes_only_managed_profiles_and_redacts_probe_failures() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let home = Arc::new(tempfile::tempdir()?);
    seed_profiles(home.path(), AuthMode::Chatgpt)?;
    let api = AccountMetadata::new("c".parse()?, AuthMode::ApiKey, chrono::Utc::now());
    ProfileAuthStorage::new(
        home.path(),
        api.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?
    .save(&AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("private-c-key".into()),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    })?;
    let mut disabled = AccountMetadata::new("d".parse()?, AuthMode::Chatgpt, chrono::Utc::now());
    disabled.enabled = false;
    RegistryStore::new(home.path()).compare_and_swap(
        /*expected_generation*/ 0,
        |registry| {
            registry.accounts[0].email = Some("private@example.test".into());
            registry.accounts[0].note = Some("private-note".into());
            registry.accounts.extend([api.clone(), disabled.clone()]);
        },
    )?;
    let reset = chrono::Utc::now().timestamp() + 7_200;
    Mock::given(method("GET")).and(path("/backend-api/wham/usage"))
        .and(header("chatgpt-account-id", "a-workspace"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plan_type":"pro", "account_id":"private-service-id", "user_id":"private-user-id",
            "credits":{"has_credits":true,"unlimited":false,"balance":"private-balance"},
            "rate_limit":{"allowed":false,"limit_reached":true,
                "primary_window":{"used_percent":100,"limit_window_seconds":18000,"reset_after_seconds":7200,"reset_at":reset},
                "secondary_window":{"used_percent":20,"limit_window_seconds":604800,"reset_after_seconds":3600,"reset_at":reset-3600}},
            "additional_rate_limits":[{"limit_name":"o3","metered_feature":"o3",
                "rate_limit":{"allowed":false,"limit_reached":true,"primary_window":{"used_percent":100,"limit_window_seconds":300,"reset_after_seconds":10,"reset_at":reset-7190}}}]
        }))).expect(1).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("chatgpt-account-id", "b-workspace"))
        .respond_with(ResponseTemplate::new(500).set_body_string("private backend failure details"))
        .expect(1)
        .mount(&server)
        .await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("inspect"),
                responses::ev_function_call(
                    "inspect",
                    "account_management",
                    &json!({"action":"list","refresh_service_usage":true}).to_string(),
                ),
                responses::ev_completed("inspect"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("done"),
                responses::ev_assistant_message("done-message", "done"),
                responses::ev_completed("done"),
            ]),
        ],
    )
    .await;
    let backend = format!("{}/backend-api", server.uri());
    let test = test_codex()
        .with_home(home)
        .with_auth(CodexAuth::from_api_key("legacy"))
        .with_model_info_override("account-management-output-model", |model| {
            model.use_responses_lite = false;
            model.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
            model.experimental_supported_tools.clear();
            model.truncation_policy =
                codex_protocol::openai_models::TruncationPolicyConfig::bytes(/*limit*/ 256);
        })
        .with_config(move |config| {
            config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
            config.chatgpt_base_url = backend;
            config
                .features
                .disable(Feature::UseAgentIdentity)
                .expect("disable agent identity for the account fixture");
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Inspect current included limits without exposing account identities")
        .await?;
    let output = management_output(&mock.requests()[1], "inspect");
    assert_eq!(
        (
            &output["routedAccount"],
            &output["accounts"][0]["serviceUsage"]["nextResetAt"],
            &output["accounts"][0]["serviceUsage"]["nextResetScope"]
        ),
        (&json!("a"), &json!(reset), &json!("codex.primary"))
    );
    assert_eq!(
        output["accounts"]
            .as_array()
            .expect("profile rows")
            .iter()
            .map(|row| (
                row["alias"].as_str(),
                row["serviceUsage"]["state"].as_str(),
                row["serviceUsage"]["reason"].as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            (Some("a"), Some("observed"), None),
            (Some("b"), Some("unavailable"), Some("requestFailed")),
            (
                Some("c"),
                Some("unavailable"),
                Some("notEligibleForAutomaticSelection")
            ),
            (Some("d"), Some("unavailable"), Some("disabled")),
        ]
    );
    assert_eq!(
        output["accounts"][0]["serviceUsage"]["buckets"][0],
        json!([
            "codex",
            100.0,
            reset,
            20.0,
            reset - 3600,
            true,
            false,
            null,
            null,
            null,
            null
        ])
    );
    let encoded = output.to_string();
    for private in [
        "a-workspace",
        "a-access",
        "private-service-id",
        "private-user-id",
        "private-balance",
        "private@example.test",
        "private-note",
        "private backend failure details",
    ] {
        assert!(!encoded.contains(private));
    }
    let usage_requests = server
        .received_requests()
        .await
        .expect("recorded requests")
        .into_iter()
        .filter(|request| request.url.path() == "/backend-api/wham/usage")
        .count();
    assert_eq!(usage_requests, 2);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
