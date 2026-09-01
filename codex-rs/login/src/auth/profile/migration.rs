use std::fs::File;
use std::io;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::DateTime;
use chrono::Utc;
use codex_account_registry::AccountAlias;
use codex_account_registry::AccountId;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::DEFAULT_ACCOUNT_PRIORITY;
use codex_account_registry::LegacyMigrationJournal;
use codex_account_registry::LegacyMigrationStage;
use codex_account_registry::RegistryLockGuard;
use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_config::types::AuthCredentialsStoreMode;
use codex_keyring_store::KeyringStore;
use codex_protocol::auth::AuthMode;
use serde::Deserialize;
use serde::Serialize;

use super::LegacyAuthMigrationError;
use super::LegacyAuthMigrationOutcome;
use super::ProfileAuthStorage;
use super::storage_error;
use super::sync_directory;
use crate::auth::AuthDotJson;
use crate::auth::AuthFileDurabilityError;
use crate::auth::AuthKeyringBackendKind;
use crate::auth::atomic_file::repair_parent_directory_durability;
use crate::auth::atomic_file::replace_atomically;
use crate::auth::credential_lock::CredentialLockGuard;
use crate::auth::storage::AuthStorage;
use crate::auth::storage::AuthStorageNamespace;
use crate::auth::storage::PersistentAuthBackendKind;
use crate::auth::storage::create_auth_storage_for_backend;
use crate::auth::storage::create_auth_storage_with_store;

const MIGRATION_JOURNAL_FILE: &str = ".legacy-auth-migration.json";
const PROFILE_MIGRATION_JOURNAL_VERSION: u32 = 1;
const LEGACY_MIGRATION_PRIORITY: u32 = 0;
const MULTI_6_MIGRATION_PRIORITY: u32 = 1000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MigrationBackendDescriptor {
    kind: PersistentAuthBackendKind,
    namespace: AuthStorageNamespace,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MigrationJournalRecord {
    version: u32,
    requested_mode: AuthCredentialsStoreMode,
    requested_keyring_backend: AuthKeyringBackendKind,
    source: MigrationBackendDescriptor,
    target: MigrationBackendDescriptor,
    pub(super) journal: LegacyMigrationJournal,
}

impl MigrationJournalRecord {
    pub(super) fn new(
        requested_mode: AuthCredentialsStoreMode,
        requested_keyring_backend: AuthKeyringBackendKind,
        source_kind: PersistentAuthBackendKind,
        target_kind: PersistentAuthBackendKind,
    ) -> Self {
        Self {
            version: PROFILE_MIGRATION_JOURNAL_VERSION,
            requested_mode,
            requested_keyring_backend,
            source: MigrationBackendDescriptor {
                kind: source_kind,
                namespace: AuthStorageNamespace::LegacyV0,
            },
            target: MigrationBackendDescriptor {
                kind: target_kind,
                namespace: AuthStorageNamespace::ProfileV1,
            },
            journal: LegacyMigrationJournal::new(AccountId::generate(), Utc::now()),
        }
    }

    fn validate_configuration(
        &self,
        mode: AuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> Result<(), LegacyAuthMigrationError> {
        if self.version != PROFILE_MIGRATION_JOURNAL_VERSION {
            return Err(LegacyAuthMigrationError::UnsupportedJournalVersion);
        }
        if self.requested_mode != mode
            || self.requested_keyring_backend != keyring_backend_kind
            || self.source.namespace != AuthStorageNamespace::LegacyV0
            || self.target.namespace != AuthStorageNamespace::ProfileV1
        {
            return Err(LegacyAuthMigrationError::BackendConfigurationDrift);
        }
        Ok(())
    }

    pub(super) fn target_account_id(&self) -> &AccountId {
        self.journal.target_account_id()
    }

    pub(super) fn stage(&self) -> LegacyMigrationStage {
        self.journal.stage()
    }

    fn updated_at(&self) -> DateTime<Utc> {
        self.journal.updated_at()
    }

    pub(super) fn transition(
        &mut self,
        stage: LegacyMigrationStage,
        occurred_at: DateTime<Utc>,
    ) -> Result<(), codex_account_registry::MigrationJournalError> {
        self.journal.transition(stage, occurred_at)
    }

    fn rollback(
        &mut self,
        occurred_at: DateTime<Utc>,
    ) -> Result<(), codex_account_registry::MigrationJournalError> {
        self.journal.rollback(occurred_at)
    }
}

pub(super) fn migrate_legacy_auth_with_store(
    codex_home: &Path,
    mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    keyring_store: Arc<dyn KeyringStore>,
) -> Result<LegacyAuthMigrationOutcome, LegacyAuthMigrationError> {
    if mode == AuthCredentialsStoreMode::Ephemeral {
        return Ok(LegacyAuthMigrationOutcome::NoPersistentAuth);
    }

    let registry_store = RegistryStore::new(codex_home);
    let registry_guard = registry_store.acquire_lock()?;
    let journal_store = MigrationJournalStore::new(codex_home);
    let stored_journal = journal_store.read()?;
    if let Some(journal) = &stored_journal {
        journal.validate_configuration(mode, keyring_backend_kind)?;
        if journal.stage() == LegacyMigrationStage::Completed {
            return Ok(LegacyAuthMigrationOutcome::AlreadyCompleted {
                account_id: journal.target_account_id().clone(),
            });
        }
    }
    let legacy_storage = match &stored_journal {
        Some(journal) => create_auth_storage_for_backend(
            codex_home.to_path_buf(),
            journal.source.kind,
            Arc::clone(&keyring_store),
            journal.source.namespace,
        ),
        None => create_auth_storage_with_store(
            codex_home.to_path_buf(),
            mode,
            Arc::clone(&keyring_store),
            keyring_backend_kind,
        ),
    };
    let legacy_guard = legacy_storage
        .acquire_lock()
        .map_err(|source| storage_error("lock legacy credential storage", source))?;
    let mut journal = match stored_journal {
        Some(journal) if journal.stage() == LegacyMigrationStage::RolledBack => {
            let Some(restarted) = restart_rolled_back_migration(
                codex_home,
                &registry_store,
                &legacy_storage,
                &legacy_guard,
                Arc::clone(&keyring_store),
                &journal_store,
                journal,
            )?
            else {
                return Ok(LegacyAuthMigrationOutcome::NoPersistentAuth);
            };
            restarted
        }
        Some(journal) => journal,
        None => match registry_store.read() {
            Ok(_) => return Ok(LegacyAuthMigrationOutcome::RegistryAlreadyInitialized),
            Err(RegistryStoreError::NotFound) => {
                let Some(source) = load_auth(
                    &legacy_storage,
                    &legacy_guard,
                    "load persistent legacy auth",
                )?
                else {
                    return Ok(LegacyAuthMigrationOutcome::NoPersistentAuth);
                };
                validate_source(&source)?;
                let source_kind = legacy_storage
                    .resolved_backend_kind_with_guard(&legacy_guard)
                    .map_err(|source| storage_error("resolve legacy auth backend", source))?
                    .ok_or(LegacyAuthMigrationError::MissingSource)?;
                let target_kind = target_backend_kind(mode, keyring_backend_kind, source_kind)?;
                let journal = MigrationJournalRecord::new(
                    mode,
                    keyring_backend_kind,
                    source_kind,
                    target_kind,
                );
                journal_store.save(&journal)?;
                journal
            }
            Err(error) => return Err(error.into()),
        },
    };
    if matches!(
        journal.stage(),
        LegacyMigrationStage::Prepared
            | LegacyMigrationStage::LegacyBackupPreserved
            | LegacyMigrationStage::CredentialStored
            | LegacyMigrationStage::Verified
    ) && legacy_storage
        .resolved_backend_kind_with_guard(&legacy_guard)
        .map_err(|source| storage_error("verify legacy auth backend", source))?
        != Some(journal.source.kind)
    {
        return Err(LegacyAuthMigrationError::BackendConfigurationDrift);
    }
    let profile_storage = ProfileAuthStorage::new_with_backend(
        codex_home,
        journal.target_account_id().clone(),
        journal.target.kind,
        keyring_store,
    )
    .map_err(|source| storage_error("open profile credential storage", source))?;
    let profile_guard = profile_storage
        .acquire_lock()
        .map_err(|source| storage_error("lock profile credential storage", source))?;

    loop {
        match journal.stage() {
            LegacyMigrationStage::Prepared => {
                let source = require_auth(&legacy_storage, &legacy_guard)?;
                let preserved = require_auth(&legacy_storage, &legacy_guard)?;
                if source != preserved {
                    return Err(LegacyAuthMigrationError::VerificationFailed);
                }
                transition_and_save(
                    &mut journal,
                    LegacyMigrationStage::LegacyBackupPreserved,
                    &journal_store,
                )?;
            }
            LegacyMigrationStage::LegacyBackupPreserved => {
                let source = require_auth(&legacy_storage, &legacy_guard)?;
                save_profile_exact(&profile_storage, &profile_guard, &source)?;
                transition_and_save(
                    &mut journal,
                    LegacyMigrationStage::CredentialStored,
                    &journal_store,
                )?;
            }
            LegacyMigrationStage::CredentialStored => {
                let source = require_auth(&legacy_storage, &legacy_guard)?;
                if profile_storage
                    .load_with_guard(&profile_guard)
                    .map_err(|source| {
                        storage_error("reload migrated profile credentials", source)
                    })?
                    != Some(source)
                {
                    rollback_profile(
                        &profile_storage,
                        &profile_guard,
                        &mut journal,
                        &journal_store,
                    )?;
                    return Err(LegacyAuthMigrationError::VerificationFailed);
                }
                transition_and_save(&mut journal, LegacyMigrationStage::Verified, &journal_store)?;
            }
            LegacyMigrationStage::Verified => {
                let source =
                    match load_auth(&legacy_storage, &legacy_guard, "load verified legacy auth")? {
                        Some(auth) => {
                            if profile_storage.load_with_guard(&profile_guard).map_err(
                                |source| storage_error("recheck verified profile auth", source),
                            )? != Some(auth.clone())
                            {
                                rollback_profile(
                                    &profile_storage,
                                    &profile_guard,
                                    &mut journal,
                                    &journal_store,
                                )?;
                                return Err(LegacyAuthMigrationError::SourceChanged);
                            }
                            auth
                        }
                        None => return Err(LegacyAuthMigrationError::MissingSource),
                    };
                validate_source(&source)?;
                ensure_registry_committed(&registry_store, &registry_guard, &journal, &source)?;
                transition_and_save(
                    &mut journal,
                    LegacyMigrationStage::RegistryStored,
                    &journal_store,
                )?;
            }
            LegacyMigrationStage::RegistryStored => {
                let current_legacy = load_auth(
                    &legacy_storage,
                    &legacy_guard,
                    "load legacy auth before final deletion",
                )?;
                if let Some(source) = current_legacy.as_ref() {
                    validate_source(source)?;
                    save_profile_exact(&profile_storage, &profile_guard, source)?;
                }
                ensure_committed_state(
                    &registry_store,
                    &journal,
                    &profile_storage,
                    &profile_guard,
                    current_legacy.as_ref(),
                )?;
                delete_legacy_auth(
                    &legacy_storage,
                    &legacy_guard,
                    codex_home,
                    current_legacy.as_ref(),
                )?;
                transition_and_save(
                    &mut journal,
                    LegacyMigrationStage::Completed,
                    &journal_store,
                )?;
                return Ok(LegacyAuthMigrationOutcome::Migrated {
                    account_id: journal.target_account_id().clone(),
                });
            }
            LegacyMigrationStage::Completed => {
                return Ok(LegacyAuthMigrationOutcome::AlreadyCompleted {
                    account_id: journal.target_account_id().clone(),
                });
            }
            LegacyMigrationStage::RolledBack => {
                return Err(LegacyAuthMigrationError::RegistryConflict);
            }
        }
    }
}

fn target_backend_kind(
    mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    source_kind: PersistentAuthBackendKind,
) -> Result<PersistentAuthBackendKind, LegacyAuthMigrationError> {
    match mode {
        AuthCredentialsStoreMode::File => Ok(PersistentAuthBackendKind::File),
        AuthCredentialsStoreMode::Keyring => Ok(match keyring_backend_kind {
            AuthKeyringBackendKind::Direct => PersistentAuthBackendKind::DirectKeyring,
            AuthKeyringBackendKind::Secrets => PersistentAuthBackendKind::Secrets,
        }),
        AuthCredentialsStoreMode::Auto => Ok(source_kind),
        AuthCredentialsStoreMode::Ephemeral => {
            Err(LegacyAuthMigrationError::BackendConfigurationDrift)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn restart_rolled_back_migration(
    codex_home: &Path,
    registry_store: &RegistryStore,
    legacy_storage: &Arc<AuthStorage>,
    legacy_guard: &CredentialLockGuard,
    keyring_store: Arc<dyn KeyringStore>,
    journal_store: &MigrationJournalStore,
    prior: MigrationJournalRecord,
) -> Result<Option<MigrationJournalRecord>, LegacyAuthMigrationError> {
    match registry_store.read() {
        Ok(_) => return Err(LegacyAuthMigrationError::RegistryConflict),
        Err(RegistryStoreError::NotFound) => {}
        Err(error) => return Err(error.into()),
    }
    let prior_profile = ProfileAuthStorage::new_with_backend(
        codex_home,
        prior.target_account_id().clone(),
        prior.target.kind,
        keyring_store,
    )
    .map_err(|source| storage_error("open rolled-back profile storage", source))?;
    let prior_profile_guard = prior_profile
        .acquire_lock()
        .map_err(|source| storage_error("lock rolled-back profile storage", source))?;
    let _ = prior_profile
        .delete_with_guard(&prior_profile_guard)
        .map_err(|source| storage_error("clean rolled-back profile credentials", source))?;
    if load_auth(
        legacy_storage,
        legacy_guard,
        "load legacy auth after rollback",
    )?
    .is_none()
    {
        return Ok(None);
    }
    let restarted = MigrationJournalRecord::new(
        prior.requested_mode,
        prior.requested_keyring_backend,
        prior.source.kind,
        prior.target.kind,
    );
    journal_store.save(&restarted)?;
    Ok(Some(restarted))
}

fn require_auth(
    storage: &Arc<AuthStorage>,
    guard: &CredentialLockGuard,
) -> Result<AuthDotJson, LegacyAuthMigrationError> {
    let auth = load_auth(storage, guard, "load persistent legacy auth")?
        .ok_or(LegacyAuthMigrationError::MissingSource)?;
    validate_source(&auth)?;
    Ok(auth)
}

fn validate_source(auth: &AuthDotJson) -> Result<(), LegacyAuthMigrationError> {
    let has_material = match auth.resolved_mode() {
        AuthMode::ApiKey => auth
            .openai_api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty()),
        AuthMode::Chatgpt => auth
            .tokens
            .as_ref()
            .is_some_and(|tokens| !tokens.access_token.trim().is_empty()),
        AuthMode::ChatgptAuthTokens => {
            return Err(LegacyAuthMigrationError::ExternalAuthUnsupported);
        }
        AuthMode::AgentIdentity => auth
            .agent_identity
            .as_ref()
            .is_some_and(super::super::storage::AgentIdentityStorage::has_auth_material),
        AuthMode::PersonalAccessToken => auth
            .personal_access_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty()),
        AuthMode::BedrockApiKey => auth.bedrock_api_key.as_ref().is_some_and(|bedrock| {
            !bedrock.api_key.trim().is_empty() && !bedrock.region.trim().is_empty()
        }),
        AuthMode::BedrockAccessKeys => auth.bedrock_access_keys.as_ref().is_some_and(|bedrock| {
            !bedrock.access_key_id.trim().is_empty() && !bedrock.secret_access_key.trim().is_empty()
        }),
        AuthMode::Headers => false,
    };
    if !has_material {
        return Err(LegacyAuthMigrationError::InvalidPersistentAuth);
    }
    Ok(())
}

fn load_auth(
    storage: &Arc<AuthStorage>,
    guard: &CredentialLockGuard,
    operation: &'static str,
) -> Result<Option<AuthDotJson>, LegacyAuthMigrationError> {
    storage
        .load_with_guard(guard)
        .map_err(|source| storage_error(operation, source))
}

fn save_profile_exact(
    profile: &ProfileAuthStorage,
    guard: &CredentialLockGuard,
    auth: &AuthDotJson,
) -> Result<(), LegacyAuthMigrationError> {
    if let Err(source) = profile.save_with_guard(guard, auth) {
        if AuthFileDurabilityError::from_io_error(&source).is_none() {
            return Err(storage_error("save profile credentials", source));
        }
        profile
            .backend
            .repair_durability_with_guard(guard)
            .map_err(|repair| storage_error("repair profile credential durability", repair))?;
    }
    if profile
        .load_with_guard(guard)
        .map_err(|reload| storage_error("reload saved profile credentials", reload))?
        != Some(auth.clone())
    {
        return Err(LegacyAuthMigrationError::VerificationFailed);
    }
    Ok(())
}

fn rollback_profile(
    profile: &ProfileAuthStorage,
    guard: &CredentialLockGuard,
    journal: &mut MigrationJournalRecord,
    journal_store: &MigrationJournalStore,
) -> Result<(), LegacyAuthMigrationError> {
    let _ = profile
        .delete_with_guard(guard)
        .map_err(|source| storage_error("remove unverified profile credentials", source))?;
    if profile
        .load_with_guard(guard)
        .map_err(|source| storage_error("verify profile credential rollback", source))?
        .is_some()
    {
        return Err(LegacyAuthMigrationError::VerificationFailed);
    }
    let timestamp = next_timestamp(journal);
    journal.rollback(timestamp)?;
    journal_store.save(journal)
}

fn ensure_registry_committed(
    store: &RegistryStore,
    guard: &RegistryLockGuard,
    journal: &MigrationJournalRecord,
    auth: &AuthDotJson,
) -> Result<(), LegacyAuthMigrationError> {
    match store.read() {
        Ok(registry) => return verify_registry(&registry, journal, auth),
        Err(RegistryStoreError::NotFound) => {}
        Err(error) => return Err(error.into()),
    }
    let registry = registry_for(&journal.journal, auth)?;
    match store.create_with_guard(guard, &registry) {
        Ok(()) => Ok(()),
        Err(RegistryStoreError::CommittedDurabilityUncertain { .. }) => {
            store.repair_committed_durability_with_guard(guard)?;
            let stored = store.read()?;
            verify_registry(&stored, journal, auth)
        }
        Err(error) => Err(error.into()),
    }
}

fn ensure_committed_state(
    store: &RegistryStore,
    journal: &MigrationJournalRecord,
    profile: &ProfileAuthStorage,
    profile_guard: &CredentialLockGuard,
    expected_auth: Option<&AuthDotJson>,
) -> Result<(), LegacyAuthMigrationError> {
    let registry = store.read()?;
    let stored_auth = profile
        .load_with_guard(profile_guard)
        .map_err(|source| storage_error("verify committed profile credentials", source))?
        .ok_or(LegacyAuthMigrationError::MissingSource)?;
    if expected_auth.is_some_and(|expected| expected != &stored_auth) {
        return Err(LegacyAuthMigrationError::SourceChanged);
    }
    verify_registry(&registry, journal, &stored_auth)?;
    Ok(())
}

fn verify_registry(
    registry: &AccountRegistry,
    journal: &MigrationJournalRecord,
    auth: &AuthDotJson,
) -> Result<(), LegacyAuthMigrationError> {
    let target = journal.target_account_id();
    let expected = registry_for(&journal.journal, auth)?;
    let expected_account = &expected.accounts[0];
    let stored_account = registry
        .accounts
        .iter()
        .find(|account| &account.id == target)
        .ok_or(LegacyAuthMigrationError::RegistryConflict)?;
    let priority_is_compatible = [
        LEGACY_MIGRATION_PRIORITY,
        MULTI_6_MIGRATION_PRIORITY,
        DEFAULT_ACCOUNT_PRIORITY,
    ]
    .contains(&stored_account.priority);
    let stored_account_without_priority = AccountMetadata {
        priority: expected_account.priority,
        ..stored_account.clone()
    };
    if registry.default_account_id.as_ref() != Some(target)
        || !priority_is_compatible
        || &stored_account_without_priority != expected_account
    {
        return Err(LegacyAuthMigrationError::RegistryConflict);
    }
    Ok(())
}

pub(super) fn registry_for(
    journal: &LegacyMigrationJournal,
    auth: &AuthDotJson,
) -> Result<AccountRegistry, LegacyAuthMigrationError> {
    let email = unambiguous_email(auth);
    let alias = match email.as_deref().and_then(sanitize_email_alias) {
        Some(alias) => alias,
        None => "default".parse::<AccountAlias>().map_err(|error| {
            storage_error("construct default account alias", io::Error::other(error))
        })?,
    };
    let plan_type = auth
        .tokens
        .as_ref()
        .and_then(|tokens| tokens.id_token.chatgpt_plan_type.clone());
    let account = AccountMetadata {
        id: journal.target_account_id().clone(),
        alias,
        auth_mode: auth.resolved_mode(),
        email,
        plan_type,
        enabled: true,
        priority: DEFAULT_ACCOUNT_PRIORITY,
        created_at: journal.started_at(),
        last_used_at: None,
        note: None,
        service_account_id: None,
        service_workspace_id: None,
    };
    let mut registry = AccountRegistry {
        default_account_id: Some(account.id.clone()),
        ..AccountRegistry::default()
    };
    registry.add_account(account)?;
    Ok(registry)
}

fn unambiguous_email(auth: &AuthDotJson) -> Option<String> {
    let mut emails = Vec::new();
    if let Some(email) = auth
        .tokens
        .as_ref()
        .and_then(|tokens| tokens.id_token.email.as_deref())
    {
        emails.push(email);
    }
    if let Some(email) = auth
        .agent_identity
        .as_ref()
        .and_then(|identity| identity.as_record())
        .and_then(|record| record.email.as_deref())
    {
        emails.push(email);
    }
    let mut normalized = emails
        .into_iter()
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    match normalized.as_slice() {
        [email] => Some(email.clone()),
        _ => None,
    }
}

fn sanitize_email_alias(email: &str) -> Option<AccountAlias> {
    if !email.is_ascii() {
        return None;
    }
    let mut alias = String::new();
    let mut last_was_separator = false;
    for character in email
        .chars()
        .map(|character| character.to_ascii_lowercase())
    {
        if character.is_ascii_alphanumeric() {
            alias.push(character);
            last_was_separator = false;
        } else if !alias.is_empty() && !last_was_separator {
            alias.push('-');
            last_was_separator = true;
        }
        if alias.len() > 64 {
            return None;
        }
    }
    while alias.ends_with('-') {
        alias.pop();
    }
    alias.parse().ok()
}

fn delete_legacy_auth(
    storage: &Arc<AuthStorage>,
    guard: &CredentialLockGuard,
    codex_home: &Path,
    expected: Option<&AuthDotJson>,
) -> Result<(), LegacyAuthMigrationError> {
    if load_auth(storage, guard, "recheck legacy auth before deletion")?.as_ref() != expected {
        return Err(LegacyAuthMigrationError::SourceChanged);
    }
    if let Err(source) = storage.delete_with_guard(guard)
        && load_auth(storage, guard, "reload legacy auth after delete failure")?.is_some()
    {
        return Err(storage_error("delete legacy auth", source));
    }
    if load_auth(storage, guard, "verify legacy auth deletion")?.is_some() {
        return Err(LegacyAuthMigrationError::RegistryConflict);
    }
    sync_directory(codex_home)
        .map_err(|source| storage_error("synchronize legacy auth deletion", source))
}

fn transition_and_save(
    journal: &mut MigrationJournalRecord,
    stage: LegacyMigrationStage,
    store: &MigrationJournalStore,
) -> Result<(), LegacyAuthMigrationError> {
    let timestamp = next_timestamp(journal);
    journal.transition(stage, timestamp)?;
    store.save(journal)
}

fn next_timestamp(journal: &MigrationJournalRecord) -> DateTime<Utc> {
    Utc::now().max(journal.updated_at())
}

struct MigrationJournalStore {
    path: PathBuf,
}

impl MigrationJournalStore {
    fn new(codex_home: &Path) -> Self {
        Self {
            path: codex_home.join("accounts").join(MIGRATION_JOURNAL_FILE),
        }
    }

    fn read(&self) -> Result<Option<MigrationJournalRecord>, LegacyAuthMigrationError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(storage_error("open legacy migration journal", source)),
        };
        serde_json::from_reader(BufReader::new(file))
            .map(Some)
            .map_err(|error| {
                storage_error(
                    "parse legacy migration journal",
                    io::Error::new(io::ErrorKind::InvalidData, error),
                )
            })
    }

    fn save(&self, journal: &MigrationJournalRecord) -> Result<(), LegacyAuthMigrationError> {
        let mut contents = serde_json::to_vec_pretty(journal).map_err(|error| {
            storage_error(
                "serialize legacy migration journal",
                io::Error::other(error),
            )
        })?;
        contents.push(b'\n');
        match replace_atomically(&self.path, &contents) {
            Ok(()) => Ok(()),
            Err(error) if AuthFileDurabilityError::from_io_error(&error).is_some() => {
                repair_parent_directory_durability(&self.path).map_err(|repair| {
                    storage_error("repair legacy migration journal durability", repair)
                })?;
                if self.read()? == Some(journal.clone()) {
                    Ok(())
                } else {
                    Err(storage_error(
                        "durably save legacy migration journal",
                        error,
                    ))
                }
            }
            Err(source) => Err(storage_error("save legacy migration journal", source)),
        }
    }
}
