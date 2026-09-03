use std::io;

use app_test_support::ChatGptIdTokenClaims;
use app_test_support::encode_id_token;
use codex_account_registry::AccountAlias;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::DEFAULT_ACCOUNT_PRIORITY;
use codex_account_registry::OpaqueServiceId;
use codex_account_registry::RegistryStore;
use codex_config::types::AuthCredentialsStoreMode;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::ProfileAuthStorage;
use codex_login::token_data::TokenData;
use codex_login::token_data::parse_chatgpt_jwt_claims;
use codex_protocol::auth::AuthMode;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::super::AccountErrorKind;
use super::super::AddArgs;
use super::add_with_authorizer;

async fn config(home: &TempDir) -> Config {
    std::fs::write(
        home.path().join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )
    .expect("write config");
    ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .build()
        .await
        .expect("load config")
}

fn pending_journal_count(home: &TempDir) -> usize {
    std::fs::read_dir(home.path().join("accounts"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join(".pending-profile-login-v1.json").exists())
        .count()
}

fn chatgpt_auth(user: &str, workspace: &str) -> AuthDotJson {
    let token = encode_id_token(
        &ChatGptIdTokenClaims::new()
            .email("new@example.test")
            .plan_type("plus")
            .chatgpt_user_id(user)
            .chatgpt_account_id(workspace),
    )
    .expect("encode token");
    AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: parse_chatgpt_jwt_claims(&token).expect("parse token"),
            access_token: "access-token".to_string(),
            refresh_token: "refresh-token".to_string(),
            account_id: Some(workspace.to_string()),
        }),
        last_refresh: Some(chrono::Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    }
}

#[tokio::test]
async fn successful_authorization_commits_verified_profile() {
    let home = TempDir::new().expect("temporary home");
    let config = config(&home).await;
    let store = RegistryStore::new(home.path());
    add_with_authorizer(
        &config,
        &store,
        AddArgs {
            alias: "new".parse().expect("alias"),
            device_auth: false,
        },
        /*json*/ false,
        |profile| async move { profile.save(&chatgpt_auth("user-1", "workspace-1")) },
    )
    .await
    .expect("add profile");

    let registry = store.read().expect("read registry");
    assert_eq!(registry.accounts.len(), 1);
    let account = &registry.accounts[0];
    assert_eq!(registry.default_account_id.as_ref(), Some(&account.id));
    assert_eq!(account.alias.as_str(), "new");
    assert_eq!(account.priority, DEFAULT_ACCOUNT_PRIORITY);
    assert!(
        ProfileAuthStorage::new(
            home.path(),
            account.id.clone(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )
        .expect("open profile")
        .load()
        .expect("load profile")
        .is_some()
    );
    assert_eq!(pending_journal_count(&home), 0);
}

#[tokio::test]
async fn cancelled_authorization_removes_pending_state() {
    let home = TempDir::new().expect("temporary home");
    let config = config(&home).await;
    let store = RegistryStore::new(home.path());
    let error = add_with_authorizer(
        &config,
        &store,
        AddArgs {
            alias: "cancelled".parse().expect("alias"),
            device_auth: false,
        },
        /*json*/ false,
        |_profile| async {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cancelled fixture",
            ))
        },
    )
    .await
    .expect_err("cancelled login must fail");
    assert_eq!(error.kind(), AccountErrorKind::LoginCancelled);
    assert!(matches!(
        store.read(),
        Err(codex_account_registry::RegistryStoreError::NotFound)
    ));
    assert_eq!(pending_journal_count(&home), 0);
    let profile_directories = std::fs::read_dir(home.path().join("accounts"))
        .expect("read accounts")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .count();
    assert_eq!(profile_directories, 0);
}

#[tokio::test]
async fn duplicate_service_identity_cleans_pending_profile() {
    let home = TempDir::new().expect("temporary home");
    let config = config(&home).await;
    let store = RegistryStore::new(home.path());
    let mut existing = AccountMetadata::new(
        "existing".parse::<AccountAlias>().expect("alias"),
        AuthMode::Chatgpt,
        chrono::Utc::now(),
    );
    existing.service_account_id = Some(OpaqueServiceId::new("user-1").expect("service id"));
    existing.service_workspace_id =
        Some(OpaqueServiceId::new("workspace-1").expect("workspace id"));
    store
        .create(&AccountRegistry {
            default_account_id: Some(existing.id.clone()),
            accounts: vec![existing.clone()],
            ..AccountRegistry::default()
        })
        .expect("create registry");

    let error = add_with_authorizer(
        &config,
        &store,
        AddArgs {
            alias: "duplicate".parse().expect("alias"),
            device_auth: false,
        },
        /*json*/ false,
        |profile| async move { profile.save(&chatgpt_auth("user-1", "workspace-1")) },
    )
    .await
    .expect_err("duplicate identity must fail");
    assert_eq!(error.kind(), AccountErrorKind::DuplicateAccount);
    assert_eq!(
        store.read().expect("read registry").accounts,
        vec![existing]
    );
    assert_eq!(pending_journal_count(&home), 0);
}
