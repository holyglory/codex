//! Public account-profile journeys over the app-server JSON-RPC surface.

use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use codex_account_registry::RegistryStore;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::InitializeResponse;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::MultiAccountCapability;
use codex_app_server_protocol::RequestId;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use serial_test::serial;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::MockServer;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_ISSUER_ENV_VAR: &str = "CODEX_APP_SERVER_LOGIN_ISSUER";

async fn raw_request(app_server: &mut TestAppServer, method: &str, params: Value) -> Result<Value> {
    let request_id = app_server.send_raw_request(method, Some(params)).await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    Ok(response.result)
}

async fn raw_error(
    app_server: &mut TestAppServer,
    method: &str,
    params: Value,
) -> Result<JSONRPCError> {
    let request_id = app_server.send_raw_request(method, Some(params)).await?;
    Ok(timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??)
}

fn write_profile_config(home: &TempDir, forced_login_method: &str) -> Result<()> {
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "cli_auth_credentials_store = \"file\"\nforced_login_method = \"{forced_login_method}\"\n"
        ),
    )?;
    Ok(())
}

#[tokio::test]
async fn account_profile_journey_persists_mutations_and_orders_notifications() -> Result<()> {
    let home = TempDir::new()?;
    write_profile_config(&home, "api")?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;

    let initialized = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.initialize_with_client_info(ClientInfo {
            name: "account-profile-journey".into(),
            title: None,
            version: "test".into(),
        }),
    )
    .await??;
    let JSONRPCMessage::Response(response) = initialized else {
        anyhow::bail!("initialize did not return a response");
    };
    let initialize: InitializeResponse = to_response(response)?;
    let capability = initialize
        .multi_account
        .as_ref()
        .expect("persistent homes advertise multi-account support");
    assert_eq!(
        capability,
        &MultiAccountCapability {
            version: 2,
            supports_managed_login: true,
            supports_auto_selection: true,
            supports_credit_usage: true,
        }
    );

    let alpha = raw_request(
        &mut app_server,
        "accountProfileLogin/start",
        json!({
            "alias": "alpha",
            "activate": true,
            "login": {"type": "apiKey", "apiKey": "sk-profile-alpha"}
        }),
    )
    .await?;
    let alpha_id = alpha["accountId"]
        .as_str()
        .expect("profile login returns an account id")
        .to_string();
    assert_eq!(alpha["login"]["type"], "apiKey");
    let notifications = app_server.pending_notification_methods();
    let login_index = notifications
        .iter()
        .position(|method| method == "account/login/completed")
        .expect("login completion notification");
    let updated_index = notifications
        .iter()
        .position(|method| method == "account/updated")
        .expect("account update notification");
    assert!(login_index < updated_index);
    app_server.clear_message_buffer();

    let listed = raw_request(&mut app_server, "accountProfile/list", json!({"limit": 1})).await?;
    assert_eq!(listed["data"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["data"][0]["id"], alpha_id);
    assert_eq!(listed["data"][0]["alias"], "alpha");
    assert_eq!(listed["data"][0]["isDefault"], true);
    assert_eq!(listed["data"][0]["isActive"], true);
    assert_eq!(listed["data"][0]["creditUsageEnabled"], false);
    assert_eq!(listed["nextCursor"], Value::Null);

    let read = raw_request(
        &mut app_server,
        "accountProfile/read",
        json!({"accountId": alpha_id}),
    )
    .await?;
    assert_eq!(read["profile"]["authenticated"], true);
    assert_eq!(read["profile"]["authMode"], "apikey");
    assert_eq!(read["profile"]["creditUsageEnabled"], false);

    let beta = raw_request(
        &mut app_server,
        "accountProfileLogin/start",
        json!({
            "alias": "beta",
            "login": {"type": "apiKey", "apiKey": "sk-profile-beta"}
        }),
    )
    .await?;
    let beta_id = beta["accountId"]
        .as_str()
        .expect("profile login returns an account id")
        .to_string();
    app_server.clear_message_buffer();
    let updated = raw_request(
        &mut app_server,
        "accountProfile/update",
        json!({"accountId": beta_id, "alias": "renamed", "note": "temporary fixture"}),
    )
    .await?;
    assert_eq!(updated["profile"]["alias"], "renamed");
    assert_eq!(updated["profile"]["note"], "temporary fixture");

    let store = RegistryStore::new(home.path());
    let mut expected_registry = store.read()?;
    let mut expected_profile = updated["profile"].clone();
    for (fields, enabled, credit_usage_enabled) in [
        (json!({"creditUsageEnabled": true}), true, true),
        (json!({}), true, true),
        (json!({"creditUsageEnabled": null}), true, true),
        (json!({"enabled": false}), false, true),
        (
            json!({"enabled": true, "creditUsageEnabled": false}),
            true,
            false,
        ),
        (json!({"creditUsageEnabled": true}), true, true),
    ] {
        let mut params = fields;
        params["accountId"] = beta_id.clone().into();
        let changed = raw_request(&mut app_server, "accountProfile/update", params).await?;
        expected_profile["enabled"] = enabled.into();
        expected_profile["creditUsageEnabled"] = credit_usage_enabled.into();
        assert_eq!(changed, json!({"profile": expected_profile}));

        expected_registry.generation += 1;
        let expected_account = expected_registry
            .accounts
            .iter_mut()
            .find(|account| account.id.as_str() == beta_id)
            .expect("updated profile remains in the registry");
        expected_account.enabled = enabled;
        expected_account.credit_usage_enabled = credit_usage_enabled;
        assert_eq!(store.read()?, expected_registry);
        let read = raw_request(
            &mut app_server,
            "accountProfile/read",
            json!({"accountId": beta_id}),
        )
        .await?;
        assert_eq!(read, changed);
    }
    assert!(app_server.pending_notification_methods().is_empty());

    let activated = raw_request(
        &mut app_server,
        "accountProfile/activate",
        json!({"accountId": beta["accountId"]}),
    )
    .await?;
    assert_eq!(activated["profile"]["isDefault"], true);
    let notifications = app_server.pending_notification_methods();
    let active_changed = notifications
        .iter()
        .position(|method| method == "accountProfile/activeChanged")
        .expect("activation emits activeChanged");
    let account_updated = notifications
        .iter()
        .position(|method| method == "account/updated")
        .expect("activation emits account/updated");
    assert!(account_updated < active_changed);

    let auto = raw_request(
        &mut app_server,
        "accountAutoSelection/write",
        json!({"enabled": false, "policy": "priority"}),
    )
    .await?;
    assert_eq!(auto["autoSelection"]["enabled"], false);
    let auto_read = raw_request(&mut app_server, "accountAutoSelection/read", json!({})).await?;
    assert_eq!(auto_read["autoSelection"]["enabled"], false);

    let limits_error = raw_error(
        &mut app_server,
        "accountProfileRateLimit/read",
        json!({"accountId": beta_id}),
    )
    .await?;
    assert_eq!(limits_error.error.code, -32602);
    assert_eq!(
        limits_error.error.message,
        "account profile does not support service rate limits"
    );

    let duplicate = raw_error(
        &mut app_server,
        "accountProfileLogin/start",
        json!({
            "alias": "renamed",
            "login": {"type": "apiKey", "apiKey": "sk-duplicate"}
        }),
    )
    .await?;
    assert_eq!(duplicate.error.code, -32602);
    assert_eq!(duplicate.error.message, "account alias is already in use");

    let invalid_cursor = raw_error(
        &mut app_server,
        "accountProfile/list",
        json!({"cursor": "v1|0|bad|bad"}),
    )
    .await?;
    assert_eq!(invalid_cursor.error.code, -32602);
    assert_eq!(invalid_cursor.error.message, "cursor is invalid");

    let removed = raw_request(
        &mut app_server,
        "accountProfile/remove",
        json!({"accountId": alpha_id}),
    )
    .await?;
    assert_eq!(removed["accountId"], alpha_id);
    let remaining = raw_request(&mut app_server, "accountProfile/list", json!({})).await?;
    assert_eq!(remaining["data"].as_array().map(Vec::len), Some(1));
    assert_eq!(remaining["data"][0]["alias"], "renamed");
    assert_eq!(remaining["data"][0]["creditUsageEnabled"], true);
    app_server.shutdown_gracefully().await?;
    let mut restarted = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let persisted = raw_request(&mut restarted, "accountProfile/list", json!({})).await?;
    assert_eq!(persisted["data"][0]["alias"], "renamed");
    assert_eq!(persisted["data"][0]["creditUsageEnabled"], true);

    let revoked = raw_request(
        &mut restarted,
        "accountProfile/update",
        json!({"accountId": beta_id, "creditUsageEnabled": false}),
    )
    .await?;
    let mut expected = persisted["data"][0].clone();
    expected["creditUsageEnabled"] = false.into();
    assert_eq!(revoked, json!({"profile": expected}));
    assert!(!store.read()?.accounts[0].credit_usage_enabled);
    Ok(())
}

#[tokio::test]
async fn account_profile_capability_is_absent_for_ephemeral_homes() -> Result<()> {
    let home = TempDir::new()?;
    std::fs::write(
        home.path().join("config.toml"),
        "cli_auth_credentials_store = \"ephemeral\"\n",
    )?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    let initialized = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.initialize_with_client_info(ClientInfo {
            name: "account-profile-ephemeral".into(),
            title: None,
            version: "test".into(),
        }),
    )
    .await??;
    let JSONRPCMessage::Response(response) = initialized else {
        anyhow::bail!("initialize did not return a response");
    };
    let initialize: InitializeResponse = to_response(response)?;
    assert_eq!(initialize.multi_account, None);
    Ok(())
}

#[tokio::test]
#[serial(login_port)]
async fn account_profile_chatgpt_login_can_be_cancelled() -> Result<()> {
    let home = TempDir::new()?;
    write_profile_config(&home, "chatgpt")?;
    let issuer = MockServer::start().await;
    let issuer_url = issuer.uri();
    let mut app_server = TestAppServer::builder()
        .with_mock_chatgpt_backend()
        .with_codex_home(home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("OPENAI_API_KEY", None),
            (LOGIN_ISSUER_ENV_VAR, Some(issuer_url.as_str())),
        ])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let started = raw_request(
        &mut app_server,
        "accountProfileLogin/start",
        json!({
            "alias": "browser",
            "login": {"type": "chatgpt"}
        }),
    )
    .await?;
    let login_id = started["login"]["loginId"]
        .as_str()
        .expect("browser login returns a login id")
        .to_string();
    assert!(
        started["login"]["authUrl"]
            .as_str()
            .expect("browser login returns an auth URL")
            .contains("redirect_uri")
    );

    let cancelled = raw_request(
        &mut app_server,
        "accountProfileLogin/cancel",
        json!({"loginId": login_id}),
    )
    .await?;
    assert_eq!(cancelled["status"], "canceled");
    let notification = app_server
        .read_notification::<Value>("account/login/completed")
        .await?;
    assert_eq!(notification["loginId"], login_id);
    assert_eq!(notification["success"], false);
    assert!(!home.path().join("auth.json").exists());
    Ok(())
}
