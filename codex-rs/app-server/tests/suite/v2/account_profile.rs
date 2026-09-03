use anyhow::Result;
use app_test_support::ChatGptIdTokenClaims;
use app_test_support::TestAppServer;
use app_test_support::encode_id_token;
use codex_account_registry::AccountAlias;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::DEFAULT_ACCOUNT_PRIORITY;
use codex_account_registry::OpaqueServiceId;
use codex_account_registry::RegistryStore;
use codex_app_server_protocol::AccountAutoSelectionPolicy;
use codex_app_server_protocol::AccountAutoSelectionReadParams;
use codex_app_server_protocol::AccountAutoSelectionReadResponse;
use codex_app_server_protocol::AccountAutoSelectionWriteParams;
use codex_app_server_protocol::AccountAutoSelectionWriteResponse;
use codex_app_server_protocol::AccountPriorityOrder;
use codex_app_server_protocol::AccountProfileActivateParams;
use codex_app_server_protocol::AccountProfileActivateResponse;
use codex_app_server_protocol::AccountProfileActiveChangedNotification;
use codex_app_server_protocol::AccountProfileListParams;
use codex_app_server_protocol::AccountProfileListResponse;
use codex_app_server_protocol::AccountProfileLogin;
use codex_app_server_protocol::AccountProfileLoginCancelParams;
use codex_app_server_protocol::AccountProfileLoginCancelResponse;
use codex_app_server_protocol::AccountProfileLoginMethod;
use codex_app_server_protocol::AccountProfileLoginStartParams;
use codex_app_server_protocol::AccountProfileLoginStartResponse;
use codex_app_server_protocol::AccountProfileRateLimitReadParams;
use codex_app_server_protocol::AccountProfileRateLimitReadResponse;
use codex_app_server_protocol::AccountProfileReadParams;
use codex_app_server_protocol::AccountProfileReadResponse;
use codex_app_server_protocol::AccountProfileRemoveParams;
use codex_app_server_protocol::AccountProfileRemoveResponse;
use codex_app_server_protocol::AccountProfileUpdateParams;
use codex_app_server_protocol::AccountProfileUpdateResponse;
use codex_app_server_protocol::CancelLoginAccountStatus;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::GetAuthStatusParams;
use codex_app_server_protocol::GetAuthStatusResponse;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::LogoutAccountResponse;
use codex_app_server_protocol::RequestId;
use codex_config::types::AuthCredentialsStoreMode;
use codex_core::config::ConfigBuilder;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::ProfileAuthRouter;
use codex_login::ProfileAuthStorage;
use codex_login::login_with_api_key_to_profile;
use codex_login::token_data::TokenData;
use codex_login::token_data::parse_chatgpt_jwt_claims;
use codex_protocol::auth::AuthMode;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::str::FromStr;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test]
async fn profile_crud_auto_and_generation_survive_restart() -> Result<()> {
    let codex_home = TempDir::new()?;
    let (first_id, second_id) = seed_api_profiles(codex_home.path())?;
    let mut server = initialized_server(codex_home.path()).await?;

    let listed: AccountProfileListResponse = server
        .request(|request_id| ClientRequest::AccountProfileList {
            request_id,
            params: AccountProfileListParams {
                cursor: None,
                limit: Some(10),
            },
        })
        .await?;
    assert_eq!(listed.data.len(), 2);
    assert_eq!(
        listed
            .data
            .iter()
            .map(|profile| profile.alias.as_str())
            .collect::<Vec<_>>(),
        vec!["secondary", "primary"]
    );
    assert!(listed.data.iter().all(|profile| profile.authenticated));
    assert!(
        listed
            .data
            .iter()
            .any(|profile| profile.id == first_id && profile.is_active)
    );

    let updated: AccountProfileUpdateResponse = server
        .request(|request_id| ClientRequest::AccountProfileUpdate {
            request_id,
            params: AccountProfileUpdateParams {
                account_id: second_id.clone(),
                alias: Some("secondary-renamed".to_string()),
                enabled: None,
                priority: Some(7),
                note: Some("backup".to_string()),
                clear_note: false,
            },
        })
        .await?;
    assert_eq!(updated.profile.alias, "secondary-renamed");
    assert_eq!(updated.profile.note.as_deref(), Some("backup"));

    let activated: AccountProfileActivateResponse = server
        .request(|request_id| ClientRequest::AccountProfileActivate {
            request_id,
            params: AccountProfileActivateParams {
                account_id: second_id.clone(),
            },
        })
        .await?;
    assert!(activated.profile.is_active);
    let first_change: AccountProfileActiveChangedNotification = server
        .read_notification("accountProfile/activeChanged")
        .await?;
    assert_eq!(
        first_change.previous_account_id.as_deref(),
        Some(first_id.as_str())
    );
    assert_eq!(first_change.account_id, second_id);

    let auto: AccountAutoSelectionWriteResponse = server
        .request(|request_id| ClientRequest::AccountAutoSelectionWrite {
            request_id,
            params: AccountAutoSelectionWriteParams {
                enabled: true,
                policy: AccountAutoSelectionPolicy::Priority,
            },
        })
        .await?;
    assert!(auto.auto_selection.enabled);
    assert_eq!(
        auto.auto_selection.priority_order,
        AccountPriorityOrder::HigherFirst
    );
    let auto_read: AccountAutoSelectionReadResponse = server
        .request(|request_id| ClientRequest::AccountAutoSelectionRead {
            request_id,
            params: AccountAutoSelectionReadParams {},
        })
        .await?;
    assert_eq!(auto_read.auto_selection, auto.auto_selection);

    drop(server);
    let mut server = initialized_server(codex_home.path()).await?;
    let read: AccountProfileReadResponse = server
        .request(|request_id| ClientRequest::AccountProfileRead {
            request_id,
            params: AccountProfileReadParams {
                account_id: second_id.clone(),
            },
        })
        .await?;
    assert!(read.profile.is_active);
    assert_eq!(read.profile.alias, "secondary-renamed");
    let _: AccountProfileActivateResponse = server
        .request(|request_id| ClientRequest::AccountProfileActivate {
            request_id,
            params: AccountProfileActivateParams {
                account_id: first_id.clone(),
            },
        })
        .await?;
    let second_change: AccountProfileActiveChangedNotification = server
        .read_notification("accountProfile/activeChanged")
        .await?;
    assert!(second_change.generation > first_change.generation);

    let removed: AccountProfileRemoveResponse = server
        .request(|request_id| ClientRequest::AccountProfileRemove {
            request_id,
            params: AccountProfileRemoveParams {
                account_id: second_id.clone(),
            },
        })
        .await?;
    assert_eq!(removed.account_id, second_id);
    Ok(())
}

#[tokio::test]
async fn profile_list_paginates_by_higher_priority_then_alias() -> Result<()> {
    let codex_home = TempDir::new()?;
    let _ = seed_api_profiles(codex_home.path())?;
    let store = RegistryStore::new(codex_home.path());
    let registry = store.read()?;
    let mut alpha = AccountMetadata::new(
        AccountAlias::from_str("alpha")?,
        AuthMode::ApiKey,
        chrono::Utc::now(),
    );
    alpha.priority = 1;
    login_with_api_key_to_profile(
        &ProfileAuthStorage::new(
            codex_home.path(),
            alpha.id.clone(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )?,
        "sk-alpha",
    )?;
    store.compare_and_swap(registry.generation, |registry| {
        registry.add_account(alpha.clone()).expect("unique account")
    })?;

    let mut server = initialized_server(codex_home.path()).await?;
    let mut cursor = None;
    let mut ordered = Vec::new();
    loop {
        let page: AccountProfileListResponse = server
            .request(|request_id| ClientRequest::AccountProfileList {
                request_id,
                params: AccountProfileListParams {
                    cursor: cursor.clone(),
                    limit: Some(1),
                },
            })
            .await?;
        assert_eq!(page.data.len(), 1);
        assert!(page.data[0].authenticated);
        ordered.push((page.data[0].alias.clone(), page.data[0].priority));
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        cursor = Some(next_cursor);
    }
    assert_eq!(
        ordered,
        vec![
            ("alpha".to_string(), 1),
            ("secondary".to_string(), 1),
            ("primary".to_string(), 0),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn pinned_server_reports_and_logs_out_only_the_process_active_profile() -> Result<()> {
    let codex_home = TempDir::new()?;
    let (first_id, second_id) = seed_api_profiles(codex_home.path())?;
    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .with_args(&["--account", "secondary"])
        .build_initialized()
        .await?;

    let listed: AccountProfileListResponse = server
        .request(|request_id| ClientRequest::AccountProfileList {
            request_id,
            params: AccountProfileListParams {
                cursor: None,
                limit: Some(10),
            },
        })
        .await?;
    let primary = listed
        .data
        .iter()
        .find(|profile| profile.id == first_id)
        .expect("primary profile");
    let secondary = listed
        .data
        .iter()
        .find(|profile| profile.id == second_id)
        .expect("secondary profile");
    assert!(primary.is_default);
    assert!(!primary.is_active);
    assert!(!secondary.is_default);
    assert!(secondary.is_active);

    let logout_id = server.send_logout_account_request().await?;
    let _: LogoutAccountResponse =
        timeout(Duration::from_secs(10), server.read_response(logout_id)).await??;
    let primary_storage = ProfileAuthStorage::new(
        codex_home.path(),
        first_id.parse()?,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?;
    let secondary_storage = ProfileAuthStorage::new(
        codex_home.path(),
        second_id.parse()?,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?;
    assert!(primary_storage.load()?.is_some());
    assert_eq!(secondary_storage.load()?, None);
    Ok(())
}

#[tokio::test]
async fn activation_preserves_existing_lease_and_removal_rejects_in_use() -> Result<()> {
    let codex_home = TempDir::new()?;
    let (first_id, second_id) = seed_api_profiles(codex_home.path())?;
    let router =
        ProfileAuthRouter::open_for_management(test_auth_config(codex_home.path()).await?).await?;
    let old_lease = router.lease_for_turn().await?;
    let mut server = initialized_server(codex_home.path()).await?;

    let _: AccountProfileActivateResponse = server
        .request(|request_id| ClientRequest::AccountProfileActivate {
            request_id,
            params: AccountProfileActivateParams {
                account_id: second_id.clone(),
            },
        })
        .await?;
    assert_eq!(old_lease.account_id().as_str(), first_id);
    let new_lease = router.lease_for_turn().await?;
    assert_eq!(new_lease.account_id().as_str(), second_id);

    let in_use = raw_error(
        &mut server,
        "accountProfile/remove",
        serde_json::to_value(AccountProfileRemoveParams {
            account_id: first_id.clone(),
        })?,
    )
    .await?;
    assert_eq!(in_use.error.code, -32602);
    assert_eq!(in_use.error.message, "account profile is in use");
    drop(old_lease);
    let _: AccountProfileRemoveResponse = server
        .request(|request_id| ClientRequest::AccountProfileRemove {
            request_id,
            params: AccountProfileRemoveParams {
                account_id: first_id,
            },
        })
        .await?;
    Ok(())
}

#[tokio::test]
async fn api_key_profile_login_is_inactive_by_default_and_duplicate_alias_is_rejected() -> Result<()>
{
    let codex_home = TempDir::new()?;
    let mut server = initialized_server(codex_home.path()).await?;
    let login: AccountProfileLoginStartResponse = server
        .request(|request_id| ClientRequest::AccountProfileLoginStart {
            request_id,
            params: AccountProfileLoginStartParams {
                alias: Some("work".to_string()),
                activate: false,
                login: AccountProfileLoginMethod::ApiKey {
                    api_key: "sk-profile-one".to_string(),
                },
            },
        })
        .await?;
    assert_eq!(login.login, AccountProfileLogin::ApiKey {});
    let listed: AccountProfileListResponse = server
        .request(|request_id| ClientRequest::AccountProfileList {
            request_id,
            params: AccountProfileListParams {
                cursor: None,
                limit: Some(10),
            },
        })
        .await?;
    assert_eq!(listed.data.len(), 1);
    assert_eq!(listed.data[0].id, login.account_id);
    assert_eq!(listed.data[0].priority, DEFAULT_ACCOUNT_PRIORITY);
    assert!(listed.data[0].authenticated);
    assert!(!listed.data[0].is_default);
    assert!(!listed.data[0].is_active);
    assert_eq!(
        RegistryStore::new(codex_home.path())
            .read()?
            .default_account_id,
        None
    );
    assert!(
        ProfileAuthStorage::new(
            codex_home.path(),
            login.account_id.parse()?,
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )?
        .load()?
        .is_some()
    );

    let duplicate = raw_error(
        &mut server,
        "accountProfileLogin/start",
        serde_json::to_value(AccountProfileLoginStartParams {
            alias: Some("work".to_string()),
            activate: false,
            login: AccountProfileLoginMethod::ApiKey {
                api_key: "must-not-replace".to_string(),
            },
        })?,
    )
    .await?;
    assert_eq!(duplicate.error.code, -32602);
    assert_eq!(duplicate.error.message, "account alias is already in use");

    let canceled: AccountProfileLoginCancelResponse = server
        .request(|request_id| ClientRequest::AccountProfileLoginCancel {
            request_id,
            params: AccountProfileLoginCancelParams {
                login_id: uuid::Uuid::now_v7().to_string(),
            },
        })
        .await?;
    assert_eq!(canceled.status, CancelLoginAccountStatus::NotFound);
    Ok(())
}

#[tokio::test]
async fn api_key_profile_login_activates_only_when_requested() -> Result<()> {
    let codex_home = TempDir::new()?;
    let (first_id, _) = seed_api_profiles(codex_home.path())?;
    let mut server = initialized_server(codex_home.path()).await?;

    let inactive: AccountProfileLoginStartResponse = server
        .request(|request_id| ClientRequest::AccountProfileLoginStart {
            request_id,
            params: AccountProfileLoginStartParams {
                alias: Some("inactive".to_string()),
                activate: false,
                login: AccountProfileLoginMethod::ApiKey {
                    api_key: "sk-inactive-profile".to_string(),
                },
            },
        })
        .await?;
    let _: codex_app_server_protocol::AccountLoginCompletedNotification =
        server.read_notification("account/login/completed").await?;
    assert_eq!(
        RegistryStore::new(codex_home.path())
            .read()?
            .default_account_id
            .as_ref()
            .map(ToString::to_string),
        Some(first_id.clone())
    );
    assert!(
        !server
            .pending_notification_methods()
            .iter()
            .any(|method| method == "accountProfile/activeChanged")
    );

    let login: AccountProfileLoginStartResponse = server
        .request(|request_id| ClientRequest::AccountProfileLoginStart {
            request_id,
            params: AccountProfileLoginStartParams {
                alias: Some("activated".to_string()),
                activate: true,
                login: AccountProfileLoginMethod::ApiKey {
                    api_key: "sk-activated-profile".to_string(),
                },
            },
        })
        .await?;
    let completed: codex_app_server_protocol::AccountLoginCompletedNotification =
        server.read_notification("account/login/completed").await?;
    assert_eq!(completed.login_id, None);
    assert!(completed.success);
    assert_eq!(completed.error, None);
    let changed: AccountProfileActiveChangedNotification = server
        .read_notification("accountProfile/activeChanged")
        .await?;
    assert_eq!(
        changed.previous_account_id.as_deref(),
        Some(first_id.as_str())
    );
    assert_eq!(changed.account_id, login.account_id);

    let registry = RegistryStore::new(codex_home.path()).read()?;
    assert_eq!(
        registry
            .default_account_id
            .as_ref()
            .map(ToString::to_string),
        Some(login.account_id.clone())
    );
    let listed: AccountProfileListResponse = server
        .request(|request_id| ClientRequest::AccountProfileList {
            request_id,
            params: AccountProfileListParams {
                cursor: None,
                limit: Some(10),
            },
        })
        .await?;
    let activated = listed
        .data
        .iter()
        .find(|profile| profile.id == login.account_id)
        .expect("activated profile");
    let inactive = listed
        .data
        .iter()
        .find(|profile| profile.id == inactive.account_id)
        .expect("inactive profile");
    assert!(activated.is_default);
    assert!(activated.is_active);
    assert!(!inactive.is_default);
    assert!(!inactive.is_active);
    Ok(())
}

#[tokio::test]
async fn registry_profile_rejects_external_chatgpt_tokens_without_mutation_or_echo() -> Result<()> {
    let codex_home = TempDir::new()?;
    let (first_id, _) = seed_api_profiles(codex_home.path())?;
    let profile = ProfileAuthStorage::new(
        codex_home.path(),
        first_id.parse()?,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?;
    let before = profile.load()?;
    let mut server = initialized_server(codex_home.path()).await?;
    let access_secret = "external-access-secret";
    let workspace_secret = "external-workspace-secret";

    let error = raw_error(
        &mut server,
        "account/login/start",
        serde_json::json!({
            "type": "chatgptAuthTokens",
            "accessToken": access_secret,
            "chatgptAccountId": workspace_secret,
            "chatgptPlanType": "pro"
        }),
    )
    .await?;

    assert_eq!(error.error.code, -32600);
    assert!(error.error.message.contains("cannot be attached"));
    assert!(!error.error.message.contains(access_secret));
    assert!(!error.error.message.contains(workspace_secret));
    assert_eq!(profile.load()?, before);
    Ok(())
}

#[tokio::test]
async fn singular_auth_status_tracks_the_activated_profile() -> Result<()> {
    let codex_home = TempDir::new()?;
    let (_first_id, second_id) = seed_api_profiles(codex_home.path())?;
    let mut server = initialized_server(codex_home.path()).await?;
    let first: GetAuthStatusResponse = server
        .request(|request_id| ClientRequest::GetAuthStatus {
            request_id,
            params: GetAuthStatusParams {
                include_token: Some(true),
                refresh_token: Some(false),
            },
        })
        .await?;
    assert_eq!(first.auth_token.as_deref(), Some("sk-first"));

    let _: AccountProfileActivateResponse = server
        .request(|request_id| ClientRequest::AccountProfileActivate {
            request_id,
            params: AccountProfileActivateParams {
                account_id: second_id,
            },
        })
        .await?;
    let second: GetAuthStatusResponse = server
        .request(|request_id| ClientRequest::GetAuthStatus {
            request_id,
            params: GetAuthStatusParams {
                include_token: Some(true),
                refresh_token: Some(false),
            },
        })
        .await?;
    assert_eq!(second.auth_token.as_deref(), Some("sk-second"));
    Ok(())
}

#[tokio::test]
async fn profile_device_login_cancel_cleans_pending_state_and_preserves_active_profile()
-> Result<()> {
    let codex_home = TempDir::new()?;
    let (first_id, _) = seed_api_profiles(codex_home.path())?;
    let before = RegistryStore::new(codex_home.path()).read()?;
    let issuer = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "device_auth_id": "device-auth-profile",
            "user_code": "CODE-PROFILE",
            "interval": "1"
        })))
        .mount(&issuer)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&issuer)
        .await;
    let issuer_url = issuer.uri();
    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("OPENAI_API_KEY", None),
            ("CODEX_APP_SERVER_LOGIN_ISSUER", Some(issuer_url.as_str())),
        ])
        .build_initialized()
        .await?;
    let started: AccountProfileLoginStartResponse = server
        .request(|request_id| ClientRequest::AccountProfileLoginStart {
            request_id,
            params: AccountProfileLoginStartParams {
                alias: Some("pending".to_string()),
                activate: false,
                login: AccountProfileLoginMethod::ChatgptDeviceCode,
            },
        })
        .await?;
    let AccountProfileLogin::ChatgptDeviceCode { login_id, .. } = started.login else {
        anyhow::bail!("expected device-code login")
    };
    let canceled: AccountProfileLoginCancelResponse = server
        .request(|request_id| ClientRequest::AccountProfileLoginCancel {
            request_id,
            params: AccountProfileLoginCancelParams { login_id },
        })
        .await?;
    assert_eq!(canceled.status, CancelLoginAccountStatus::Canceled);
    let completed: codex_app_server_protocol::AccountLoginCompletedNotification =
        server.read_notification("account/login/completed").await?;
    assert!(!completed.success);
    let listed: AccountProfileListResponse = server
        .request(|request_id| ClientRequest::AccountProfileList {
            request_id,
            params: AccountProfileListParams {
                cursor: None,
                limit: Some(10),
            },
        })
        .await?;
    assert_eq!(RegistryStore::new(codex_home.path()).read()?, before);
    assert_eq!(listed.data.len(), 2);
    assert!(
        listed
            .data
            .iter()
            .any(|profile| profile.id == first_id && profile.is_active && profile.is_default)
    );
    let pending_journals = std::fs::read_dir(codex_home.path().join("accounts"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join(".pending-profile-login-v1.json").exists())
        .count();
    assert_eq!(pending_journals, 0);
    Ok(())
}

#[tokio::test]
async fn profile_device_login_duplicate_service_identity_rolls_back_credentials_and_activation()
-> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = "duplicate-workspace";
    let existing_id = seed_chatgpt_profile(codex_home.path(), "existing-token", workspace)?;
    let store = RegistryStore::new(codex_home.path());
    let before_registry = store.read()?;
    let existing_storage = ProfileAuthStorage::new(
        codex_home.path(),
        existing_id.parse()?,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?;
    let before_credentials = existing_storage.load()?;
    let issuer = MockServer::start().await;
    let duplicate_id_token = encode_id_token(
        &ChatGptIdTokenClaims::new()
            .email("duplicate@example.com")
            .plan_type("pro")
            .chatgpt_user_id("service-user")
            .chatgpt_account_id(workspace),
    )?;
    mount_successful_profile_device_login(&issuer, duplicate_id_token).await;
    let issuer_url = issuer.uri();
    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("OPENAI_API_KEY", None),
            ("CODEX_APP_SERVER_LOGIN_ISSUER", Some(issuer_url.as_str())),
        ])
        .build_initialized()
        .await?;

    let started: AccountProfileLoginStartResponse = server
        .request(|request_id| ClientRequest::AccountProfileLoginStart {
            request_id,
            params: AccountProfileLoginStartParams {
                alias: Some("duplicate".to_string()),
                activate: true,
                login: AccountProfileLoginMethod::ChatgptDeviceCode,
            },
        })
        .await?;
    let completed: codex_app_server_protocol::AccountLoginCompletedNotification =
        server.read_notification("account/login/completed").await?;

    let AccountProfileLogin::ChatgptDeviceCode { login_id, .. } = &started.login else {
        anyhow::bail!("expected device-code login")
    };
    assert!(!completed.success);
    assert_eq!(completed.login_id.as_deref(), Some(login_id.as_str()));
    assert_eq!(
        completed.error.as_deref(),
        Some("account service identity is already registered")
    );
    assert_eq!(store.read()?, before_registry);
    assert_eq!(existing_storage.load()?, before_credentials);
    assert!(
        !codex_home
            .path()
            .join("accounts")
            .join(started.account_id)
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn completed_profile_device_login_cannot_be_cancelled_or_rolled_back() -> Result<()> {
    let codex_home = TempDir::new()?;
    let issuer = MockServer::start().await;
    let id_token = encode_id_token(
        &ChatGptIdTokenClaims::new()
            .email("committed@example.com")
            .plan_type("pro")
            .chatgpt_user_id("committed-service-user")
            .chatgpt_account_id("committed-workspace"),
    )?;
    mount_successful_profile_device_login(&issuer, id_token).await;
    let issuer_url = issuer.uri();
    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("OPENAI_API_KEY", None),
            ("CODEX_APP_SERVER_LOGIN_ISSUER", Some(issuer_url.as_str())),
        ])
        .build_initialized()
        .await?;

    let started: AccountProfileLoginStartResponse = server
        .request(|request_id| ClientRequest::AccountProfileLoginStart {
            request_id,
            params: AccountProfileLoginStartParams {
                alias: Some("committed".to_string()),
                activate: false,
                login: AccountProfileLoginMethod::ChatgptDeviceCode,
            },
        })
        .await?;
    let AccountProfileLogin::ChatgptDeviceCode { login_id, .. } = &started.login else {
        anyhow::bail!("expected device-code login")
    };
    let completed: codex_app_server_protocol::AccountLoginCompletedNotification =
        server.read_notification("account/login/completed").await?;
    assert_eq!(completed.login_id.as_deref(), Some(login_id.as_str()));
    assert!(completed.success);
    assert_eq!(completed.error, None);

    let canceled: AccountProfileLoginCancelResponse = server
        .request(|request_id| ClientRequest::AccountProfileLoginCancel {
            request_id,
            params: AccountProfileLoginCancelParams {
                login_id: login_id.clone(),
            },
        })
        .await?;
    assert_eq!(canceled.status, CancelLoginAccountStatus::NotFound);
    let duplicate_completion = timeout(
        Duration::from_millis(200),
        server.read_notification::<codex_app_server_protocol::AccountLoginCompletedNotification>(
            "account/login/completed",
        ),
    )
    .await;
    assert!(
        duplicate_completion.is_err(),
        "committed login must emit exactly one completion notification"
    );

    let registry = RegistryStore::new(codex_home.path()).read()?;
    assert_eq!(registry.default_account_id, None);
    assert!(
        registry
            .accounts
            .iter()
            .any(|account| account.id.as_str() == started.account_id)
    );
    let storage = ProfileAuthStorage::new(
        codex_home.path(),
        started.account_id.parse()?,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?;
    assert!(storage.load()?.is_some());
    assert!(
        !codex_home
            .path()
            .join("accounts")
            .join(&started.account_id)
            .join(".pending-profile-login-v1.json")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn profile_rate_limits_preserve_multiple_buckets() -> Result<()> {
    let codex_home = TempDir::new()?;
    let backend = MockServer::start().await;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!("chatgpt_base_url = \"{}\"\n", backend.uri()),
    )?;
    let account_id = seed_chatgpt_profile(codex_home.path(), "chatgpt-token", "workspace-1")?;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .and(header("authorization", "Bearer chatgpt-token"))
        .and(header("chatgpt-account-id", "workspace-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 10,
                    "limit_window_seconds": 3600,
                    "reset_after_seconds": 1800,
                    "reset_at": 1_800_000_000
                }
            },
            "additional_rate_limits": [{
                "limit_name": "codex_other",
                "metered_feature": "codex_other",
                "rate_limit": {
                    "allowed": true,
                    "limit_reached": false,
                    "primary_window": {
                        "used_percent": 20,
                        "limit_window_seconds": 1800,
                        "reset_after_seconds": 900,
                        "reset_at": 1_800_000_100
                    }
                }
            }]
        })))
        .mount(&backend)
        .await;
    let mut server = initialized_server(codex_home.path()).await?;
    let limits: AccountProfileRateLimitReadResponse = server
        .request(|request_id| ClientRequest::AccountProfileRateLimitRead {
            request_id,
            params: AccountProfileRateLimitReadParams {
                account_id: account_id.clone(),
                limit_id: None,
            },
        })
        .await?;
    assert_eq!(limits.account_id, account_id);
    assert_eq!(limits.data.len(), 2);
    assert_eq!(limits.data[0].limit_id.as_deref(), Some("codex"));
    assert_eq!(limits.data[1].limit_id.as_deref(), Some("codex_other"));
    Ok(())
}

async fn initialized_server(codex_home: &Path) -> Result<TestAppServer> {
    TestAppServer::builder()
        .with_codex_home(codex_home)
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized()
        .await
}

fn seed_api_profiles(codex_home: &Path) -> Result<(String, String)> {
    let mut first = AccountMetadata::new(
        AccountAlias::from_str("primary")?,
        AuthMode::ApiKey,
        chrono::Utc::now(),
    );
    first.priority = 0;
    let mut second = AccountMetadata::new(
        AccountAlias::from_str("secondary")?,
        AuthMode::ApiKey,
        chrono::Utc::now(),
    );
    second.priority = 1;
    for (account, key) in [(&first, "sk-first"), (&second, "sk-second")] {
        let storage = ProfileAuthStorage::new(
            codex_home,
            account.id.clone(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )?;
        login_with_api_key_to_profile(&storage, key)?;
    }
    let first_id = first.id.to_string();
    let second_id = second.id.to_string();
    let registry = AccountRegistry {
        generation: 1,
        default_account_id: Some(first.id.clone()),
        accounts: vec![first, second],
        ..AccountRegistry::default()
    };
    RegistryStore::new(codex_home).create(&registry)?;
    Ok((first_id, second_id))
}

fn seed_chatgpt_profile(codex_home: &Path, token: &str, workspace: &str) -> Result<String> {
    let claims = ChatGptIdTokenClaims::new()
        .email("profile@example.com")
        .plan_type("pro")
        .chatgpt_user_id("service-user")
        .chatgpt_account_id(workspace);
    let id_token = parse_chatgpt_jwt_claims(&encode_id_token(&claims)?)?;
    let auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token,
            access_token: token.to_string(),
            refresh_token: "refresh-token".to_string(),
            account_id: Some(workspace.to_string()),
        }),
        last_refresh: Some(chrono::Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    };
    let mut account = AccountMetadata::new(
        AccountAlias::from_str("chatgpt")?,
        AuthMode::Chatgpt,
        chrono::Utc::now(),
    );
    let identity = auth.profile_metadata();
    account.email = identity.email;
    account.plan_type = identity.plan_type;
    account.service_account_id = identity
        .service_account_id
        .map(OpaqueServiceId::new)
        .transpose()?;
    account.service_workspace_id = identity
        .service_workspace_id
        .map(OpaqueServiceId::new)
        .transpose()?;
    let storage = ProfileAuthStorage::new(
        codex_home,
        account.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    storage.save(&auth)?;
    let id = account.id.to_string();
    RegistryStore::new(codex_home).create(&AccountRegistry {
        generation: 1,
        default_account_id: Some(account.id.clone()),
        accounts: vec![account],
        ..AccountRegistry::default()
    })?;
    Ok(id)
}

async fn mount_successful_profile_device_login(issuer: &MockServer, id_token: String) {
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "device_auth_id": "profile-device-auth",
            "user_code": "PROFILE-CODE",
            "interval": "0"
        })))
        .mount(issuer)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "authorization_code": "profile-authorization-code",
            "code_challenge": "profile-code-challenge",
            "code_verifier": "profile-code-verifier"
        })))
        .mount(issuer)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id_token": id_token,
            "access_token": "profile-access-token",
            "refresh_token": "profile-refresh-token"
        })))
        .mount(issuer)
        .await;
}

async fn test_auth_config(codex_home: &Path) -> Result<codex_login::AuthConfig> {
    Ok(ConfigBuilder::default()
        .codex_home(codex_home.to_path_buf())
        .build()
        .await?
        .auth_config())
}

async fn raw_error(
    server: &mut TestAppServer,
    method: &str,
    params: serde_json::Value,
) -> Result<JSONRPCError> {
    let request_id = server.send_raw_request(method, Some(params)).await?;
    timeout(
        Duration::from_secs(10),
        server.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await?
}
