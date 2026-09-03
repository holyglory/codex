use super::*;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_keyring_store::tests::MockKeyringStore;
use codex_protocol::auth::AuthMode;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

fn api_auth(marker: &str) -> crate::auth::AuthDotJson {
    crate::auth::AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some(format!("{marker}-secret")),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    }
}

fn committed_account(
    pending: &PendingProfileLogin,
    auth: &crate::auth::AuthDotJson,
) -> AccountMetadata {
    let identity = auth.profile_metadata();
    AccountMetadata {
        id: pending.account_id().clone(),
        alias: pending.alias().clone(),
        auth_mode: identity.auth_mode,
        email: identity.email,
        plan_type: identity.plan_type,
        enabled: true,
        priority: 0,
        created_at: pending.started_at(),
        last_used_at: None,
        note: None,
        service_account_id: None,
        service_workspace_id: None,
    }
}

#[test]
fn stale_uncommitted_file_profile_is_rolled_back() {
    let home = tempdir().expect("temporary home");
    let keyring = MockKeyringStore::default();
    let pending = PendingProfileLogin::begin_with_store(
        home.path(),
        "stale".parse().expect("alias"),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring.clone()),
    )
    .expect("begin pending login");
    let profile_home = pending.storage().profile_home().to_path_buf();
    pending
        .storage()
        .save(&api_auth("stale"))
        .expect("save pending auth");
    let recovery_time = pending.started_at() + PENDING_LOGIN_STALE_AFTER + Duration::seconds(1);
    drop(pending);

    recover_pending_profile_logins_with_store(
        home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring),
        recovery_time,
    )
    .expect("recover stale pending login");

    assert!(!profile_home.exists());
}

#[test]
fn committed_keyring_profile_is_verified_and_journal_is_finished() {
    let home = tempdir().expect("temporary home");
    let keyring = MockKeyringStore::default();
    let pending = PendingProfileLogin::begin_with_store(
        home.path(),
        "committed".parse().expect("alias"),
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring.clone()),
    )
    .expect("begin pending login");
    let auth = api_auth("committed");
    pending.storage().save(&auth).expect("save pending auth");
    let storage = pending.storage().clone();
    let account = committed_account(&pending, &auth);
    RegistryStore::new(home.path())
        .create(&AccountRegistry {
            default_account_id: Some(account.id.clone()),
            accounts: vec![account],
            ..AccountRegistry::default()
        })
        .expect("commit registry");
    drop(pending);

    recover_pending_profile_logins_with_store(
        home.path(),
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring),
        Utc::now(),
    )
    .expect("finish committed pending login");

    assert_eq!(storage.load().expect("load committed auth"), Some(auth));
    assert!(!storage.profile_home().join(PENDING_LOGIN_FILE).exists());
}

#[test]
fn recent_pending_profile_does_not_block_an_independent_login() {
    let home = tempdir().expect("temporary home");
    let keyring = MockKeyringStore::default();
    let first = PendingProfileLogin::begin_with_store(
        home.path(),
        "first".parse().expect("alias"),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring.clone()),
    )
    .expect("begin first login");
    let second = PendingProfileLogin::begin_with_store(
        home.path(),
        "second".parse().expect("alias"),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring),
    )
    .expect("begin independent login");

    assert_ne!(first.account_id(), second.account_id());
    first.cleanup().expect("clean first login");
    second.cleanup().expect("clean second login");
}

#[test]
fn committed_profile_recovery_rejects_backend_drift_without_deleting_auth() {
    let home = tempdir().expect("temporary home");
    let keyring = MockKeyringStore::default();
    let pending = PendingProfileLogin::begin_with_store(
        home.path(),
        "drift".parse().expect("alias"),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring.clone()),
    )
    .expect("begin pending login");
    let auth = api_auth("drift");
    pending.storage().save(&auth).expect("save pending auth");
    let storage = pending.storage().clone();
    let account = committed_account(&pending, &auth);
    RegistryStore::new(home.path())
        .create(&AccountRegistry {
            default_account_id: Some(account.id.clone()),
            accounts: vec![account],
            ..AccountRegistry::default()
        })
        .expect("commit registry");
    drop(pending);

    assert!(matches!(
        recover_pending_profile_logins_with_store(
            home.path(),
            AuthCredentialsStoreMode::Keyring,
            AuthKeyringBackendKind::Direct,
            Arc::new(keyring),
            Utc::now(),
        ),
        Err(PendingProfileLoginError::ConfigurationDrift)
    ));
    assert_eq!(storage.load().expect("auth remains"), Some(auth));
    assert!(storage.profile_home().join(PENDING_LOGIN_FILE).exists());
}

#[test]
fn corrupt_pending_journal_error_is_content_and_path_free() {
    let home = tempdir().expect("temporary home");
    let keyring = MockKeyringStore::default();
    let pending = PendingProfileLogin::begin_with_store(
        home.path(),
        "corrupt".parse().expect("alias"),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring.clone()),
    )
    .expect("begin pending login");
    let journal_path = pending.storage().profile_home().join(PENDING_LOGIN_FILE);
    let secret = "journal-secret-content";
    write_file_atomically(
        &journal_path,
        format!("{{invalid:{secret}").as_bytes(),
        AtomicWriteMode::Replace,
    )
    .expect("replace journal with corrupt fixture");
    drop(pending);

    let error = recover_pending_profile_logins_with_store(
        home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring),
        Utc::now(),
    )
    .expect_err("corrupt journal must fail closed");
    let display = error.to_string();
    assert!(!display.contains(secret));
    assert!(!display.contains(home.path().to_string_lossy().as_ref()));
}
