use chrono::Utc;
use codex_account_registry::AccountAlias;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_config::ManagedAuthPolicy;
use codex_config::types::AuthCredentialsStoreMode;
use codex_protocol::auth::AuthMode;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::AccountManagementError;
use super::ProfileAuthStorage;
use super::read_managed_accounts;
use super::set_all_managed_account_priorities;
use super::set_managed_account_priority;
use crate::AuthConfig;
use crate::AuthDotJson;
use crate::AuthKeyringBackendKind;

fn config(home: &TempDir) -> AuthConfig {
    AuthConfig {
        codex_home: home.path().to_path_buf(),
        auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        keyring_backend_kind: AuthKeyringBackendKind::Direct,
        forced_login_method: None,
        chatgpt_base_url: None,
        forced_chatgpt_workspace_id: None,
        managed_auth_policy: ManagedAuthPolicy::default(),
        auth_route_config: crate::test_support::transport_default_auth_route_config(),
    }
}

fn api_key_auth(secret: &str) -> AuthDotJson {
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

fn fixture() -> (TempDir, AuthConfig) {
    let home = TempDir::new().expect("temporary home");
    let config = config(&home);
    let mut alpha = AccountMetadata::new(
        "alpha".parse::<AccountAlias>().expect("alias"),
        AuthMode::ApiKey,
        Utc::now(),
    );
    alpha.priority = 1;
    let mut beta = AccountMetadata::new(
        "beta".parse::<AccountAlias>().expect("alias"),
        AuthMode::ApiKey,
        Utc::now(),
    );
    beta.priority = 2;
    beta.enabled = false;
    ProfileAuthStorage::new(
        home.path(),
        alpha.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("profile storage")
    .save(&api_key_auth("must-not-appear"))
    .expect("save auth");
    RegistryStore::new(home.path())
        .create(&AccountRegistry {
            default_account_id: Some(alpha.id.clone()),
            accounts: vec![alpha, beta],
            ..AccountRegistry::default()
        })
        .expect("registry");
    (home, config)
}

#[test]
fn snapshots_are_higher_first_and_credential_free() {
    let (_home, config) = fixture();
    let snapshot = read_managed_accounts(&config).expect("snapshot");
    assert_eq!(
        snapshot
            .accounts
            .iter()
            .map(|account| {
                (
                    account.alias.as_str(),
                    account.priority,
                    account.enabled,
                    account.authenticated,
                    account.is_default,
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("beta", 2, false, false, false),
            ("alpha", 1, true, true, true),
        ]
    );
    assert!(!format!("{snapshot:?}").contains("must-not-appear"));
}

#[test]
fn priority_mutations_honor_generation_and_preserve_idempotence() {
    let (_home, config) = fixture();
    let initial = read_managed_accounts(&config).expect("initial");
    let changed = set_managed_account_priority(
        &config,
        "alpha",
        /*priority*/ 3,
        Some(initial.generation),
    )
    .expect("set priority");
    assert_eq!(changed.changed_count, 1);
    assert_eq!(changed.snapshot.accounts[0].alias, "alpha");
    assert_eq!(changed.snapshot.accounts[0].priority, 3);
    assert_eq!(
        set_managed_account_priority(
            &config,
            "alpha",
            /*priority*/ 4,
            Some(initial.generation)
        ),
        Err(AccountManagementError::GenerationConflict)
    );

    let normalized = set_all_managed_account_priorities(
        &config,
        /*priority*/ 1000,
        Some(changed.snapshot.generation),
    )
    .expect("normalize priorities");
    assert_eq!(normalized.changed_count, 2);
    assert!(
        normalized
            .snapshot
            .accounts
            .iter()
            .all(|account| account.priority == 1000)
    );
    let unchanged = set_all_managed_account_priorities(
        &config,
        /*priority*/ 1000,
        Some(normalized.snapshot.generation),
    )
    .expect("idempotent normalization");
    assert_eq!(unchanged.changed_count, 0);
    assert_eq!(
        unchanged.snapshot.generation,
        normalized.snapshot.generation
    );
}
