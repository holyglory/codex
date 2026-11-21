use super::*;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_keyring_store::CredentialStoreError;
use codex_keyring_store::tests::MockKeyringStore;
use codex_protocol::auth::AuthMode;
use codex_secrets::LocalSecretsBackend;
use codex_secrets::LocalSecretsNamespace;
use codex_secrets::SecretName;
use codex_secrets::SecretScope;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[derive(Debug, Default)]
struct RetirementKeyring {
    inner: MockKeyringStore,
    fail_delete: std::sync::Mutex<Option<String>>,
}

impl KeyringStore for RetirementKeyring {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError> {
        self.inner.load(service, account)
    }

    fn save(&self, service: &str, account: &str, value: &str) -> Result<(), CredentialStoreError> {
        self.inner.save(service, account, value)
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialStoreError> {
        let mut fail_delete = self.fail_delete.lock().expect("failure fixture lock");
        if fail_delete.as_deref() == Some(account) {
            *fail_delete = None;
            return Err(CredentialStoreError::new(keyring::Error::NoStorageAccess(
                Box::new(io::Error::other("synthetic retirement failure")),
            )));
        }
        self.inner.delete(service, account)
    }
}

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
        credit_usage_enabled: false,
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
    let auth = api_auth("stale");
    pending.storage().save(&auth).expect("save pending auth");
    {
        let backend = pending.storage().auth_backend();
        let _write_guard = backend.lock_for_write().expect("upstream write lock");
    }
    let recovery_time = pending.started_at() + PENDING_LOGIN_STALE_AFTER + Duration::seconds(1);
    recover_pending_profile_logins_with_store(
        home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring.clone()),
        recovery_time,
    )
    .expect("active authorization is not abandoned");
    assert_eq!(pending.storage().load().expect("active auth"), Some(auth));
    assert!(profile_home.join(PENDING_LOGIN_FILE).exists());
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
fn cleanup_preserves_unknown_files_and_journal_for_retry() {
    let home = tempdir().expect("temporary home");
    let keyring = MockKeyringStore::default();
    let pending = PendingProfileLogin::begin_with_store(
        home.path(),
        "retry".parse().expect("alias"),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring.clone()),
    )
    .expect("begin pending login");
    let profile_home = pending.storage().profile_home().to_path_buf();
    let recovery_time = pending.started_at() + PENDING_LOGIN_STALE_AFTER + Duration::seconds(1);
    pending
        .storage()
        .save(&api_auth("retry"))
        .expect("save auth");
    let unknown_file = profile_home.join("keep.txt");
    std::fs::write(&unknown_file, "unrelated fixture").expect("write unrelated file");

    assert!(matches!(
        pending.cleanup(),
        Err(PendingProfileLoginError::Storage(error))
            if error.kind() == io::ErrorKind::DirectoryNotEmpty
    ));
    assert_eq!(
        std::fs::read(&unknown_file).expect("unknown file remains"),
        b"unrelated fixture"
    );
    assert!(!profile_home.join("auth.json").exists());
    assert!(profile_home.join(PENDING_LOGIN_FILE).exists());
    std::fs::remove_file(unknown_file).expect("resolve unrelated file");
    recover_pending_profile_logins_with_store(
        home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring),
        recovery_time,
    )
    .expect("retry cleanup");
    assert!(!profile_home.exists());
}

#[test]
fn pending_secrets_cleanup_preserves_other_secrets_and_retries_key_retirement() {
    let home = tempdir().expect("temporary home");
    let keyring = Arc::new(RetirementKeyring::default());
    let pending = PendingProfileLogin::begin_with_store(
        home.path(),
        "secrets".parse().expect("alias"),
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Secrets,
        keyring.clone(),
    )
    .expect("begin pending login");
    let profile_home = pending.storage().profile_home().to_path_buf();
    let journal_path = profile_home.join(PENDING_LOGIN_FILE);
    let recovery_time = pending.started_at() + PENDING_LOGIN_STALE_AFTER + Duration::seconds(1);
    let auth = api_auth("secrets");
    pending.storage().save(&auth).expect("save profile auth");
    assert_eq!(
        pending.storage().load().expect("read profile auth"),
        Some(auth)
    );
    assert!(
        profile_home
            .join("secrets")
            .join("codex_auth.age")
            .is_file()
    );
    let namespace = LocalSecretsNamespace::CodexProfileAuthV1;
    let secrets =
        LocalSecretsBackend::new_with_namespace(profile_home.clone(), keyring.clone(), namespace);
    let unrelated = SecretName::new("UNRELATED").expect("secret name");
    secrets
        .set(&SecretScope::Global, &unrelated, "synthetic unrelated data")
        .expect("save unrelated secret");
    let profile_key = codex_secrets::compute_keyring_account(&profile_home, namespace);
    assert!(matches!(
        pending.cleanup(),
        Err(PendingProfileLoginError::Storage(_))
    ));
    assert!(journal_path.exists());
    assert_eq!(
        secrets
            .get(&SecretScope::Global, &unrelated)
            .expect("unrelated secret remains"),
        Some("synthetic unrelated data".to_string())
    );
    assert!(keyring.inner.saved_value(&profile_key).is_some());

    secrets
        .delete(&SecretScope::Global, &unrelated)
        .expect("resolve unrelated secret");
    *keyring.fail_delete.lock().expect("failure fixture lock") = Some(profile_key.clone());
    assert!(matches!(
        recover_pending_profile_logins_with_store(
            home.path(),
            AuthCredentialsStoreMode::Keyring,
            AuthKeyringBackendKind::Secrets,
            keyring.clone(),
            recovery_time,
        ),
        Err(PendingProfileLoginError::Storage(_))
    ));
    assert!(journal_path.exists());
    assert!(!profile_home.join("secrets/codex_auth.age").exists());
    assert!(keyring.inner.saved_value(&profile_key).is_some());
    recover_pending_profile_logins_with_store(
        home.path(),
        AuthCredentialsStoreMode::Keyring,
        AuthKeyringBackendKind::Secrets,
        keyring.clone(),
        recovery_time,
    )
    .expect("retry key retirement");
    assert!(!profile_home.exists());
    assert_eq!(keyring.inner.saved_value(&profile_key), None);
}

#[test]
fn cleanup_rejects_changed_pending_identity() {
    let home = tempdir().expect("temporary home");
    let pending = PendingProfileLogin::begin_with_store(
        home.path(),
        "original".parse().expect("alias"),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(MockKeyringStore::default()),
    )
    .expect("begin pending login");
    let storage = pending.storage().clone();
    let auth = api_auth("owned");
    storage.save(&auth).expect("save auth");
    let journal_path = storage.profile_home().join(PENDING_LOGIN_FILE);
    let mut journal = read_journal(&journal_path).expect("read journal");
    journal.alias = "changed".parse().expect("changed alias");
    write_file_atomically(
        &journal_path,
        &serde_json::to_vec(&journal).expect("encode journal"),
        AtomicWriteMode::Replace,
    )
    .expect("replace journal identity");

    assert!(matches!(
        pending.cleanup(),
        Err(PendingProfileLoginError::RecoveryConflict)
    ));
    assert_eq!(storage.load().expect("owned auth remains"), Some(auth));
    assert!(journal_path.exists());
}

#[test]
fn concurrent_stale_recovery_removes_a_profile_once() {
    let home = tempdir().expect("temporary home");
    let pending = PendingProfileLogin::begin_with_store(
        home.path(),
        "stale".parse().expect("alias"),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(MockKeyringStore::default()),
    )
    .expect("begin pending login");
    let profile_home = pending.storage().profile_home().to_path_buf();
    pending
        .storage()
        .save(&api_auth("stale"))
        .expect("save auth");
    let journal_path = profile_home.join(PENDING_LOGIN_FILE);
    let mut journal = read_journal(&journal_path).expect("read journal");
    journal.started_at -= PENDING_LOGIN_STALE_AFTER + Duration::seconds(1);
    write_file_atomically(
        &journal_path,
        &serde_json::to_vec(&journal).expect("encode journal"),
        AtomicWriteMode::Replace,
    )
    .expect("mark fixture abandoned");
    drop(pending);

    let ready = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let recover = || {
            ready.wait();
            recover_pending_profile_logins(
                home.path(),
                AuthCredentialsStoreMode::File,
                AuthKeyringBackendKind::Direct,
            )
        };
        let first = scope.spawn(recover);
        let second = scope.spawn(recover);
        first
            .join()
            .expect("first recovery worker")
            .expect("first recovery");
        second
            .join()
            .expect("second recovery worker")
            .expect("second recovery");
    });
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
