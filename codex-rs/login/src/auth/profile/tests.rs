use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use chrono::Duration;
use chrono::Utc;
use codex_account_registry::AccountId;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::DEFAULT_ACCOUNT_PRIORITY;
use codex_account_registry::LegacyMigrationStage;
use codex_account_registry::RegistryStore;
use codex_config::types::AuthCredentialsStoreMode;
use codex_keyring_store::tests::MockKeyringStore;
use codex_protocol::auth::AuthMode;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

use super::migration::MigrationJournalRecord;
use super::migration::migrate_legacy_auth_with_store;
use super::migration::registry_for;
use super::*;
use crate::auth::storage::AuthStorage;
use crate::auth::storage::PersistentAuthBackendKind;
use crate::auth::storage::create_auth_storage_with_store;
use crate::token_data::TokenData;
use crate::token_data::parse_chatgpt_jwt_claims;

fn api_auth(marker: &str) -> AuthDotJson {
    AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some(format!("{marker}-api-key")),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    }
}

fn chatgpt_auth(marker: &str, email: &str) -> AuthDotJson {
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let payload = json!({
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "pro",
            "chatgpt_user_id": format!("{marker}-user"),
            "chatgpt_account_id": format!("{marker}-workspace"),
        }
    });
    let jwt = format!(
        "{}.{}.{}",
        encode(br#"{"alg":"none","typ":"JWT"}"#),
        encode(&serde_json::to_vec(&payload).expect("serialize JWT payload")),
        encode(b"signature")
    );
    AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: parse_chatgpt_jwt_claims(&jwt).expect("parse synthetic JWT"),
            access_token: format!("{marker}-access"),
            refresh_token: format!("{marker}-refresh"),
            account_id: Some(format!("{marker}-account")),
        }),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    }
}

fn personal_access_token_auth(marker: &str) -> AuthDotJson {
    AuthDotJson {
        auth_mode: Some(AuthMode::PersonalAccessToken),
        openai_api_key: None,
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: Some(format!("at-{marker}")),
        bedrock_api_key: None,
        bedrock_access_keys: None,
    }
}

fn external_chatgpt_tokens_auth(marker: &str) -> AuthDotJson {
    let mut auth = chatgpt_auth(marker, "external@example.com");
    auth.auth_mode = Some(AuthMode::ChatgptAuthTokens);
    auth
}

fn legacy_storage(
    codex_home: &Path,
    mode: AuthCredentialsStoreMode,
    keyring: &MockKeyringStore,
) -> Arc<AuthStorage> {
    create_auth_storage_with_store(
        codex_home.to_path_buf(),
        mode,
        Arc::new(keyring.clone()),
        AuthKeyringBackendKind::Direct,
    )
}

fn write_journal(codex_home: &Path, journal: &MigrationJournalRecord) {
    let accounts = codex_home.join("accounts");
    std::fs::create_dir_all(&accounts).expect("create accounts directory");
    std::fs::write(
        accounts.join(".legacy-auth-migration.json"),
        serde_json::to_vec_pretty(journal).expect("serialize migration journal"),
    )
    .expect("write migration journal fixture");
}

fn read_journal(codex_home: &Path) -> MigrationJournalRecord {
    serde_json::from_slice(
        &std::fs::read(
            codex_home
                .join("accounts")
                .join(".legacy-auth-migration.json"),
        )
        .expect("read migration journal"),
    )
    .expect("parse migration journal")
}

#[test]
fn profile_file_storage_is_independent_and_debug_redacted() {
    let directory = tempdir().expect("create temporary directory");
    let first_id = AccountId::generate();
    let second_id = AccountId::generate();
    let first = ProfileAuthStorage::new(
        directory.path(),
        first_id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("open first profile");
    let second = ProfileAuthStorage::new(
        directory.path(),
        second_id,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("open second profile");
    let first_auth = api_auth("first-secret");
    let second_auth = api_auth("second-secret");

    first.save(&first_auth).expect("save first profile");
    second.save(&second_auth).expect("save second profile");

    assert_eq!(first.load().expect("load first profile"), Some(first_auth));
    assert_eq!(
        second.load().expect("load second profile"),
        Some(second_auth)
    );
    assert!(
        directory
            .path()
            .join("accounts")
            .join(first_id.as_str())
            .join("auth.json")
            .exists()
    );
    let debug = format!("{first:?}");
    let private_path = directory.path().to_string_lossy();
    assert!(!debug.contains(private_path.as_ref()));
    assert!(!debug.contains("first-secret"));
}

#[test]
fn profile_auth_replacement_updates_registry_identity_atomically() {
    let directory = tempdir().expect("create temporary directory");
    let account_id = AccountId::generate();
    let old_auth = api_auth("old");
    let new_auth = chatgpt_auth("new", "new@example.test");
    let profile = ProfileAuthStorage::new(
        directory.path(),
        account_id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("open profile");
    profile.save(&old_auth).expect("save old auth");
    let account = AccountMetadata {
        id: account_id.clone(),
        alias: "active".parse().expect("alias"),
        auth_mode: AuthMode::ApiKey,
        email: None,
        plan_type: None,
        enabled: true,
        priority: 0,
        created_at: Utc::now(),
        last_used_at: None,
        note: None,
        service_account_id: None,
        service_workspace_id: None,
    };
    RegistryStore::new(directory.path())
        .create(&AccountRegistry {
            default_account_id: Some(account_id),
            accounts: vec![account],
            ..AccountRegistry::default()
        })
        .expect("create registry");

    let generation = profile
        .replace_auth_and_metadata(&new_auth)
        .expect("replace auth and metadata");

    let registry = RegistryStore::new(directory.path())
        .read()
        .expect("read registry");
    let identity = new_auth.profile_metadata();
    let updated = &registry.accounts[0];
    assert_eq!(generation, registry.generation);
    assert_eq!(updated.auth_mode, identity.auth_mode);
    assert_eq!(updated.email, identity.email);
    assert_eq!(updated.plan_type, identity.plan_type);
    assert_eq!(
        updated
            .service_account_id
            .as_ref()
            .map(codex_account_registry::OpaqueServiceId::expose),
        identity.service_account_id.as_deref()
    );
    assert_eq!(
        updated
            .service_workspace_id
            .as_ref()
            .map(codex_account_registry::OpaqueServiceId::expose),
        identity.service_workspace_id.as_deref()
    );
    assert_eq!(profile.load().expect("load new auth"), Some(new_auth));
}

#[test]
fn duplicate_service_identity_rejects_profile_auth_before_credential_mutation() {
    let directory = tempdir().expect("create temporary directory");
    let target_id = AccountId::generate();
    let existing_id = AccountId::generate();
    let old_auth = api_auth("old");
    let duplicate_auth = chatgpt_auth("duplicate", "duplicate@example.test");
    let identity = duplicate_auth.profile_metadata();
    let profile = ProfileAuthStorage::new(
        directory.path(),
        target_id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("open profile");
    profile.save(&old_auth).expect("save old auth");
    let target = AccountMetadata {
        id: target_id.clone(),
        alias: "target".parse().expect("alias"),
        auth_mode: AuthMode::ApiKey,
        email: None,
        plan_type: None,
        enabled: true,
        priority: 0,
        created_at: Utc::now(),
        last_used_at: None,
        note: None,
        service_account_id: None,
        service_workspace_id: None,
    };
    let existing = AccountMetadata {
        id: existing_id,
        alias: "existing".parse().expect("alias"),
        auth_mode: AuthMode::Chatgpt,
        email: None,
        plan_type: None,
        enabled: true,
        priority: 1,
        created_at: Utc::now(),
        last_used_at: None,
        note: None,
        service_account_id: identity
            .service_account_id
            .map(|id| codex_account_registry::OpaqueServiceId::new(id).expect("service id")),
        service_workspace_id: identity
            .service_workspace_id
            .map(|id| codex_account_registry::OpaqueServiceId::new(id).expect("workspace id")),
    };
    RegistryStore::new(directory.path())
        .create(&AccountRegistry {
            default_account_id: Some(target_id),
            accounts: vec![target, existing],
            ..AccountRegistry::default()
        })
        .expect("create registry");

    assert!(matches!(
        profile.replace_auth_and_metadata(&duplicate_auth),
        Err(ProfileAuthCommitError::DuplicateServiceIdentity)
    ));
    assert_eq!(profile.load().expect("load original auth"), Some(old_auth));
}

#[test]
fn profile_keyring_storage_is_canonical_and_namespaced() {
    let directory = tempdir().expect("create temporary directory");
    let keyring = MockKeyringStore::default();
    let first_id = AccountId::generate();
    let second_id = AccountId::generate();
    let first = ProfileAuthStorage::new_with_store(
        directory.path(),
        first_id.clone(),
        AuthCredentialsStoreMode::Keyring,
        Arc::new(keyring.clone()),
        AuthKeyringBackendKind::Direct,
    )
    .expect("open first keyring profile");
    let second = ProfileAuthStorage::new_with_store(
        directory.path(),
        second_id,
        AuthCredentialsStoreMode::Keyring,
        Arc::new(keyring.clone()),
        AuthKeyringBackendKind::Direct,
    )
    .expect("open second keyring profile");
    let first_auth = api_auth("keyring-first");
    let second_auth = api_auth("keyring-second");
    first.save(&first_auth).expect("save first keyring profile");
    second
        .save(&second_auth)
        .expect("save second keyring profile");

    let reopened = ProfileAuthStorage::new_with_store(
        &directory.path().join("."),
        first_id,
        AuthCredentialsStoreMode::Keyring,
        Arc::new(keyring),
        AuthKeyringBackendKind::Direct,
    )
    .expect("reopen canonical profile");
    assert_eq!(
        reopened.load().expect("load reopened profile"),
        Some(first_auth)
    );
    assert_eq!(
        second.load().expect("load second profile"),
        Some(second_auth)
    );
}

#[cfg(unix)]
#[test]
fn file_migration_completes_then_refuses_reimport() {
    let directory = tempdir().expect("create temporary directory");
    let keyring = MockKeyringStore::default();
    let original = chatgpt_auth("original-secret", "User.Name+codex@example.com");
    let legacy = legacy_storage(directory.path(), AuthCredentialsStoreMode::File, &keyring);
    legacy.save(&original).expect("save legacy auth");

    let outcome = migrate_legacy_auth_with_store(
        directory.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring.clone()),
    )
    .expect("migrate file auth");
    let LegacyAuthMigrationOutcome::Migrated { account_id } = outcome else {
        panic!("expected a completed migration")
    };
    let profile = ProfileAuthStorage::new_with_store(
        directory.path(),
        account_id.clone(),
        AuthCredentialsStoreMode::File,
        Arc::new(keyring.clone()),
        AuthKeyringBackendKind::Direct,
    )
    .expect("open migrated profile");
    assert_eq!(
        profile.load().expect("load migrated auth"),
        Some(original.clone())
    );
    assert_eq!(legacy.load().expect("load deleted legacy auth"), None);
    let registry = RegistryStore::new(directory.path())
        .read()
        .expect("read migrated registry");
    assert_eq!(registry.default_account_id, Some(account_id.clone()));
    assert_eq!(registry.accounts[0].priority, DEFAULT_ACCOUNT_PRIORITY);
    assert_eq!(
        registry.accounts[0].alias.as_str(),
        "user-name-codex-example-com"
    );
    let journal_bytes = std::fs::read(
        directory
            .path()
            .join("accounts")
            .join(".legacy-auth-migration.json"),
    )
    .expect("read durable journal");
    assert!(!String::from_utf8_lossy(&journal_bytes).contains("original-secret"));
    RegistryStore::new(directory.path())
        .compare_and_swap(registry.generation, |registry| {
            registry.accounts[0].priority = 777;
        })
        .expect("change priority after migration");

    let replacement = api_auth("must-not-import");
    legacy.save(&replacement).expect("recreate legacy auth");
    let second = migrate_legacy_auth_with_store(
        directory.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring.clone()),
    )
    .expect("check completed migration");
    assert_eq!(
        second,
        LegacyAuthMigrationOutcome::AlreadyCompleted {
            account_id: account_id.clone()
        }
    );
    assert_eq!(
        profile.load().expect("reload migrated profile"),
        Some(original)
    );
    assert_eq!(
        legacy.load().expect("legacy auth should be untouched"),
        Some(replacement.clone())
    );
    assert_eq!(
        RegistryStore::new(directory.path())
            .read()
            .expect("read updated registry")
            .accounts[0]
            .priority,
        777
    );
    profile
        .delete()
        .expect("remove migrated profile credentials");
    std::fs::remove_dir_all(profile.profile_home()).expect("remove migrated profile directory");
    let store = RegistryStore::new(directory.path());
    let registry = store.read().expect("read registry before removal");
    store
        .compare_and_swap(registry.generation, |registry| {
            registry.accounts.clear();
            registry.default_account_id = None;
        })
        .expect("remove migrated profile metadata");

    let third = migrate_legacy_auth_with_store(
        directory.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring),
    )
    .expect("completed migration remains terminal after profile removal");
    assert_eq!(
        third,
        LegacyAuthMigrationOutcome::AlreadyCompleted { account_id }
    );
    assert!(!profile.profile_home().exists());
    assert_eq!(
        legacy.load().expect("legacy auth remains untouched"),
        Some(replacement)
    );
}

#[cfg(unix)]
#[test]
fn migration_resumes_after_profile_write_and_preserves_legacy_until_commit() {
    let directory = tempdir().expect("create temporary directory");
    let keyring = MockKeyringStore::default();
    let auth = api_auth("resume-secret");
    let legacy = legacy_storage(directory.path(), AuthCredentialsStoreMode::File, &keyring);
    legacy.save(&auth).expect("save legacy auth");
    let legacy_path = directory.path().join("auth.json");
    let original_bytes = std::fs::read(&legacy_path).expect("read untouched legacy bytes");
    let now = Utc::now();
    let mut journal = MigrationJournalRecord::new(
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        PersistentAuthBackendKind::File,
        PersistentAuthBackendKind::File,
    );
    journal
        .transition(
            LegacyMigrationStage::LegacyBackupPreserved,
            now + Duration::seconds(1),
        )
        .expect("preserve legacy source");
    let profile = ProfileAuthStorage::new_with_store(
        directory.path(),
        journal.target_account_id().clone(),
        AuthCredentialsStoreMode::File,
        Arc::new(keyring.clone()),
        AuthKeyringBackendKind::Direct,
    )
    .expect("open interrupted profile");
    profile.save(&auth).expect("save interrupted profile");
    journal
        .transition(
            LegacyMigrationStage::CredentialStored,
            now + Duration::seconds(2),
        )
        .expect("record profile write");
    write_journal(directory.path(), &journal);
    assert_eq!(
        std::fs::read(&legacy_path).expect("read legacy bytes"),
        original_bytes
    );

    let outcome = migrate_legacy_auth_with_store(
        directory.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring),
    )
    .expect("resume interrupted migration");

    assert!(matches!(
        outcome,
        LegacyAuthMigrationOutcome::Migrated { .. }
    ));
    assert!(!legacy_path.exists());
    assert_eq!(
        read_journal(directory.path()).stage(),
        LegacyMigrationStage::Completed
    );
}

#[cfg(unix)]
#[test]
fn keyring_migration_resume_accepts_only_supported_priority_registries() {
    for (resume_stage, priority, should_succeed) in [
        (LegacyMigrationStage::Verified, 0, true),
        (
            LegacyMigrationStage::Verified,
            DEFAULT_ACCOUNT_PRIORITY,
            true,
        ),
        (LegacyMigrationStage::Verified, 777, false),
        (LegacyMigrationStage::RegistryStored, 0, true),
    ] {
        let directory = tempdir().expect("create temporary directory");
        let keyring = MockKeyringStore::default();
        let auth = api_auth("keyring-resume-secret");
        let legacy = legacy_storage(
            directory.path(),
            AuthCredentialsStoreMode::Keyring,
            &keyring,
        );
        legacy.save(&auth).expect("save legacy keyring auth");
        let now = Utc::now();
        let mut journal = MigrationJournalRecord::new(
            AuthCredentialsStoreMode::Keyring,
            AuthKeyringBackendKind::Direct,
            PersistentAuthBackendKind::DirectKeyring,
            PersistentAuthBackendKind::DirectKeyring,
        );
        for (stage, seconds) in [
            (LegacyMigrationStage::LegacyBackupPreserved, 1),
            (LegacyMigrationStage::CredentialStored, 2),
            (LegacyMigrationStage::Verified, 3),
        ] {
            journal
                .transition(stage, now + Duration::seconds(seconds))
                .expect("advance journal fixture");
        }
        if resume_stage == LegacyMigrationStage::RegistryStored {
            journal
                .transition(
                    LegacyMigrationStage::RegistryStored,
                    now + Duration::seconds(4),
                )
                .expect("record registry write");
        }
        let profile = ProfileAuthStorage::new_with_store(
            directory.path(),
            journal.target_account_id().clone(),
            AuthCredentialsStoreMode::Keyring,
            Arc::new(keyring.clone()),
            AuthKeyringBackendKind::Direct,
        )
        .expect("open profile keyring");
        profile.save(&auth).expect("save profile keyring auth");
        let mut registry = registry_for(&journal.journal, &auth).expect("build migration registry");
        registry.accounts[0].priority = priority;
        RegistryStore::new(directory.path())
            .create(&registry)
            .expect("save migration registry");
        write_journal(directory.path(), &journal);

        let outcome = migrate_legacy_auth_with_store(
            directory.path(),
            AuthCredentialsStoreMode::Keyring,
            AuthKeyringBackendKind::Direct,
            Arc::new(keyring),
        );
        if !should_succeed {
            assert!(matches!(
                outcome,
                Err(LegacyAuthMigrationError::RegistryConflict)
            ));
            assert_eq!(
                read_journal(directory.path()).stage(),
                LegacyMigrationStage::Verified
            );
            continue;
        }

        assert!(matches!(
            outcome,
            Ok(LegacyAuthMigrationOutcome::Migrated { .. })
        ));
        assert_eq!(legacy.load().expect("load deleted legacy keyring"), None);
        assert_eq!(profile.load().expect("load profile keyring"), Some(auth));
        assert_eq!(
            read_journal(directory.path()).stage(),
            LegacyMigrationStage::Completed
        );
    }
}

#[cfg(unix)]
#[test]
fn persistent_personal_access_token_is_migrated_from_configured_storage() {
    let directory = tempdir().expect("create temporary directory");
    let keyring = MockKeyringStore::default();
    let auth = personal_access_token_auth("persistent-secret");
    let legacy = legacy_storage(directory.path(), AuthCredentialsStoreMode::File, &keyring);
    legacy.save(&auth).expect("save persistent PAT");

    let outcome = migrate_legacy_auth_with_store(
        directory.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring.clone()),
    )
    .expect("migrate persistent PAT");
    let LegacyAuthMigrationOutcome::Migrated { account_id } = outcome else {
        panic!("expected persistent PAT migration")
    };
    let profile = ProfileAuthStorage::new_with_store(
        directory.path(),
        account_id,
        AuthCredentialsStoreMode::File,
        Arc::new(keyring),
        AuthKeyringBackendKind::Direct,
    )
    .expect("open PAT profile storage");

    assert_eq!(profile.load().expect("load migrated PAT"), Some(auth));
    assert_eq!(legacy.load().expect("load removed legacy PAT"), None);
}

#[cfg(unix)]
#[test]
fn verification_mismatch_rolls_back_then_retries_from_legacy() {
    let directory = tempdir().expect("create temporary directory");
    let keyring = MockKeyringStore::default();
    let legacy_auth = api_auth("legacy-secret");
    let legacy = legacy_storage(directory.path(), AuthCredentialsStoreMode::File, &keyring);
    legacy.save(&legacy_auth).expect("save legacy auth");
    let now = Utc::now();
    let mut journal = MigrationJournalRecord::new(
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        PersistentAuthBackendKind::File,
        PersistentAuthBackendKind::File,
    );
    journal
        .transition(
            LegacyMigrationStage::LegacyBackupPreserved,
            now + Duration::seconds(1),
        )
        .expect("preserve legacy");
    journal
        .transition(
            LegacyMigrationStage::CredentialStored,
            now + Duration::seconds(2),
        )
        .expect("record wrong profile");
    let profile = ProfileAuthStorage::new_with_store(
        directory.path(),
        journal.target_account_id().clone(),
        AuthCredentialsStoreMode::File,
        Arc::new(keyring.clone()),
        AuthKeyringBackendKind::Direct,
    )
    .expect("open profile storage");
    profile
        .save(&api_auth("different-secret"))
        .expect("save mismatched profile");
    write_journal(directory.path(), &journal);

    assert!(matches!(
        migrate_legacy_auth_with_store(
            directory.path(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
            Arc::new(keyring.clone()),
        ),
        Err(LegacyAuthMigrationError::VerificationFailed)
    ));
    assert_eq!(
        read_journal(directory.path()).stage(),
        LegacyMigrationStage::RolledBack
    );
    assert_eq!(profile.load().expect("load rolled-back profile"), None);
    assert_eq!(
        legacy.load().expect("legacy auth remains"),
        Some(legacy_auth)
    );

    let retried = migrate_legacy_auth_with_store(
        directory.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(keyring),
    )
    .expect("retry rolled-back migration");
    assert!(matches!(
        retried,
        LegacyAuthMigrationOutcome::Migrated { .. }
    ));
}

#[test]
fn ephemeral_mode_never_reads_or_creates_persistent_auth_state() {
    let directory = tempdir().expect("create temporary directory");

    let outcome = migrate_legacy_auth_with_store(
        directory.path(),
        AuthCredentialsStoreMode::Ephemeral,
        AuthKeyringBackendKind::Direct,
        Arc::new(MockKeyringStore::default()),
    )
    .expect("ephemeral migration should be skipped");

    assert_eq!(outcome, LegacyAuthMigrationOutcome::NoPersistentAuth);
    assert!(!directory.path().join("accounts").exists());
}

#[cfg(unix)]
#[test]
fn resume_rejects_backend_configuration_drift() {
    let directory = tempdir().expect("create temporary directory");
    let keyring = MockKeyringStore::default();
    let auth = api_auth("drift-secret");
    legacy_storage(directory.path(), AuthCredentialsStoreMode::File, &keyring)
        .save(&auth)
        .expect("save legacy auth");
    let mut journal = MigrationJournalRecord::new(
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        PersistentAuthBackendKind::File,
        PersistentAuthBackendKind::File,
    );
    let now = Utc::now();
    journal
        .transition(LegacyMigrationStage::LegacyBackupPreserved, now)
        .expect("advance journal");
    write_journal(directory.path(), &journal);

    assert!(matches!(
        migrate_legacy_auth_with_store(
            directory.path(),
            AuthCredentialsStoreMode::Keyring,
            AuthKeyringBackendKind::Direct,
            Arc::new(keyring),
        ),
        Err(LegacyAuthMigrationError::BackendConfigurationDrift)
    ));
}

#[cfg(unix)]
#[test]
fn persistent_external_chatgpt_tokens_are_rejected_without_mutation() {
    let directory = tempdir().expect("create temporary directory");
    let keyring = MockKeyringStore::default();
    let auth = external_chatgpt_tokens_auth("external-secret");
    let legacy = legacy_storage(directory.path(), AuthCredentialsStoreMode::File, &keyring);
    legacy.save(&auth).expect("save external auth fixture");

    assert!(matches!(
        migrate_legacy_auth_with_store(
            directory.path(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
            Arc::new(keyring),
        ),
        Err(LegacyAuthMigrationError::ExternalAuthUnsupported)
    ));
    assert_eq!(legacy.load().expect("legacy auth remains"), Some(auth));
    assert!(matches!(
        RegistryStore::new(directory.path()).read(),
        Err(codex_account_registry::RegistryStoreError::NotFound)
    ));
}

#[cfg(unix)]
#[test]
fn corrupt_journal_error_does_not_expose_path_or_contents() {
    let directory = tempdir().expect("create temporary directory");
    let accounts = directory.path().join("accounts");
    std::fs::create_dir_all(&accounts).expect("create accounts directory");
    let secret = "journal-content-secret";
    std::fs::write(
        accounts.join(".legacy-auth-migration.json"),
        format!("{{not-json:{secret}"),
    )
    .expect("write corrupt journal");

    let error = migrate_legacy_auth_with_store(
        directory.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Arc::new(MockKeyringStore::default()),
    )
    .expect_err("corrupt journal should fail");
    let display = error.to_string();
    assert!(!display.contains(secret));
    assert!(!display.contains(directory.path().to_string_lossy().as_ref()));
}

#[test]
fn public_profile_storage_creates_protected_directories() {
    let directory = tempdir().expect("create temporary directory");
    let account_id = AccountId::generate();
    let profile = ProfileAuthStorage::new(
        directory.path(),
        account_id,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("create protected profile storage");

    codex_private_storage::verify_private_directory(&directory.path().join("accounts"))
        .expect("accounts directory is private");
    codex_private_storage::verify_private_directory(profile.profile_home())
        .expect("profile directory is private");
}

#[test]
fn ephemeral_profile_storage_remains_rejected() {
    let directory = tempdir().expect("create temporary directory");
    let error = ProfileAuthStorage::new(
        directory.path(),
        AccountId::generate(),
        AuthCredentialsStoreMode::Ephemeral,
        AuthKeyringBackendKind::Direct,
    )
    .expect_err("ephemeral profile storage should fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}
