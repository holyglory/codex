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
use super::ManagedAccountUpdate;
use super::ProfileAuthStorage;
use super::read_managed_accounts;
use super::set_all_managed_account_priorities;
use super::set_managed_account_priority;
use super::update_managed_account;
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
fn existing_profile_changes_are_atomic_and_preserve_credentials() {
    let (_home, config) = fixture();
    let initial = read_managed_accounts(&config).unwrap();
    let renamed = update_managed_account(
        &config,
        ManagedAccountUpdate::Rename {
            account: "alpha".into(),
            alias: "primary".into(),
        },
        initial.generation,
    )
    .unwrap();
    let mut expected = initial.clone();
    expected.generation += 1;
    expected.accounts[1].alias = "primary".into();
    assert_eq!(renamed.snapshot, expected);
    assert_eq!(
        update_managed_account(
            &config,
            ManagedAccountUpdate::Enable {
                account: "beta".into()
            },
            initial.generation
        )
        .unwrap_err(),
        AccountManagementError::GenerationConflict
    );
    let enabled = update_managed_account(
        &config,
        ManagedAccountUpdate::Enable {
            account: "beta".into(),
        },
        expected.generation,
    )
    .unwrap();
    assert_eq!(
        update_managed_account(
            &config,
            ManagedAccountUpdate::SetDefault {
                account: "beta".into()
            },
            enabled.snapshot.generation
        )
        .unwrap_err(),
        AccountManagementError::AccountUnavailable
    );
    assert_eq!(read_managed_accounts(&config).unwrap(), enabled.snapshot);
    let disabled = update_managed_account(
        &config,
        ManagedAccountUpdate::Disable {
            account: "primary".into(),
        },
        enabled.snapshot.generation,
    )
    .unwrap();
    assert!(
        disabled
            .snapshot
            .accounts
            .iter()
            .all(|account| !account.is_default)
    );
    let restored = update_managed_account(
        &config,
        ManagedAccountUpdate::Enable {
            account: "primary".into(),
        },
        disabled.snapshot.generation,
    )
    .unwrap();
    assert_eq!(
        restored
            .snapshot
            .accounts
            .iter()
            .filter(|account| account.is_default)
            .map(|account| account.alias.as_str())
            .collect::<Vec<_>>(),
        ["primary"]
    );
    let unchanged = update_managed_account(
        &config,
        ManagedAccountUpdate::SetDefault {
            account: "primary".into(),
        },
        restored.snapshot.generation,
    )
    .unwrap();
    assert!(!unchanged.changed);
    assert_eq!(unchanged.snapshot, restored.snapshot);
    assert_eq!(
        update_managed_account(
            &config,
            ManagedAccountUpdate::Rename {
                account: "primary".into(),
                alias: "beta".into()
            },
            restored.snapshot.generation
        )
        .unwrap_err(),
        AccountManagementError::InvalidUpdate
    );
    assert_eq!(read_managed_accounts(&config).unwrap(), restored.snapshot);
    assert!(!format!("{restored:?}").contains("must-not-appear"));
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
