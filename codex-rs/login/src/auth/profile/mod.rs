use std::fmt;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_account_registry::AccountId;
use codex_account_registry::MigrationJournalError;
use codex_account_registry::OpaqueServiceId;
use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_account_registry::RegistryValidationError;
use codex_config::types::AuthCredentialsStoreMode;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use codex_private_storage::ensure_private_directory;
use codex_private_storage::sync_parent_directory;
use thiserror::Error;

use super::AuthDotJson;
use super::AuthFileDurabilityError;
use super::AuthKeyringBackendKind;
use super::credential_lock::CredentialLockGuard;
use super::storage::AuthStorage;
use super::storage::AuthStorageNamespace;
use super::storage::PersistentAuthBackendKind;
use super::storage::create_auth_storage_for_backend;
use super::storage::create_auth_storage_with_store_and_namespace;

mod management;
mod migration;
mod pending;
mod router;
mod use_lock;

pub use management::AccountManagementError;
pub use management::ManagedAccountPriorityMutation;
pub use management::ManagedAccountSnapshot;
pub use management::ManagedAccountSummary;
pub use management::read_managed_accounts;
pub use management::set_all_managed_account_priorities;
pub use management::set_managed_account_priority;
pub use pending::PendingProfileLogin;
pub use pending::PendingProfileLoginError;
pub use router::AccountLease;
pub use router::AuthManagerLease;
pub use router::ExternalAuthConflictSource;
pub use router::ProfileAuthRouter;
pub use router::ProfileAuthRouterConfig;
pub use router::ProfileAuthRouterError;
pub use router::ProfileRemovalOutcome;
pub use router::RouterExternalAuthState;
pub use router::SharedProfileAuthRouter;

const ACCOUNTS_DIRECTORY: &str = "accounts";

/// Profile-scoped access to an existing Codex credential backend.
#[derive(Clone)]
pub struct ProfileAuthStorage {
    account_id: AccountId,
    profile_home: PathBuf,
    backend: Arc<AuthStorage>,
}

#[derive(Debug, Error)]
pub enum ProfileAuthCommitError {
    #[error("account profile metadata is unavailable")]
    UnknownAccount,
    #[error("account profile service identity is already registered")]
    DuplicateServiceIdentity,
    #[error("account profile is in use by an active operation")]
    AccountInUse,
    #[error("account profile authentication is not eligible for persistent storage")]
    UnsupportedAuth,
    #[error("account profile credential update failed")]
    Storage(#[source] io::Error),
    #[error("account profile registry update failed")]
    Registry(#[source] RegistryStoreError),
    #[error("account profile registry update committed, but durability remains uncertain")]
    CommittedDurabilityUncertain,
    #[error("account profile credential rollback failed")]
    RollbackFailed,
}

impl fmt::Debug for ProfileAuthStorage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProfileAuthStorage")
            .finish_non_exhaustive()
    }
}

impl ProfileAuthStorage {
    pub fn new(
        codex_home: &Path,
        account_id: AccountId,
        mode: AuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> io::Result<Self> {
        Self::new_with_store(
            codex_home,
            account_id,
            mode,
            Arc::new(DefaultKeyringStore),
            keyring_backend_kind,
        )
    }

    pub fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    pub fn load(&self) -> io::Result<Option<AuthDotJson>> {
        self.backend.load()
    }

    pub fn save(&self, auth: &AuthDotJson) -> io::Result<()> {
        self.backend.save(auth)
    }

    pub fn delete(&self) -> io::Result<bool> {
        let removed = self.backend.delete()?;
        if removed {
            sync_directory(&self.profile_home)?;
        }
        Ok(removed)
    }

    /// Replaces this profile's credentials and matching nonsecret registry identity as one
    /// guard-bound operation. Registry locking always precedes the profile credential lock.
    pub fn replace_auth_and_metadata(
        &self,
        auth: &AuthDotJson,
    ) -> Result<u64, ProfileAuthCommitError> {
        if matches!(
            auth.resolved_mode(),
            codex_protocol::auth::AuthMode::ChatgptAuthTokens
                | codex_protocol::auth::AuthMode::Headers
        ) {
            return Err(ProfileAuthCommitError::UnsupportedAuth);
        }
        let identity = auth.profile_metadata();
        let service_identity = match (
            identity.service_account_id.as_ref(),
            identity.service_workspace_id.as_ref(),
        ) {
            (Some(account), Some(workspace)) => Some((
                OpaqueServiceId::new(account.clone())
                    .map_err(|_| ProfileAuthCommitError::UnsupportedAuth)?,
                OpaqueServiceId::new(workspace.clone())
                    .map_err(|_| ProfileAuthCommitError::UnsupportedAuth)?,
            )),
            _ => None,
        };
        let root_home = self
            .profile_home
            .parent()
            .and_then(Path::parent)
            .ok_or(ProfileAuthCommitError::UnknownAccount)?;
        let store = RegistryStore::new(root_home);
        let registry_guard = store
            .acquire_lock()
            .map_err(ProfileAuthCommitError::Registry)?;
        let current = store.read().map_err(ProfileAuthCommitError::Registry)?;
        let mut planned = current.clone();
        let account = planned
            .accounts
            .iter_mut()
            .find(|account| account.id == self.account_id)
            .ok_or(ProfileAuthCommitError::UnknownAccount)?;
        account.auth_mode = identity.auth_mode;
        account.email = identity.email;
        account.plan_type = identity.plan_type;
        account.service_account_id = service_identity
            .as_ref()
            .map(|(account, _)| account.clone());
        account.service_workspace_id = service_identity.map(|(_, workspace)| workspace);
        if let Err(error) = planned.validate() {
            return Err(match error {
                RegistryValidationError::DuplicateServiceIdentity { .. } => {
                    ProfileAuthCommitError::DuplicateServiceIdentity
                }
                error => ProfileAuthCommitError::Registry(RegistryStoreError::Validation(error)),
            });
        }

        let Some(_use_guard) = use_lock::try_acquire_profile_removal(&self.profile_home)
            .map_err(ProfileAuthCommitError::Storage)?
        else {
            return Err(ProfileAuthCommitError::AccountInUse);
        };
        let credential_guard = self
            .acquire_lock()
            .map_err(ProfileAuthCommitError::Storage)?;
        let previous = self
            .load_with_guard(&credential_guard)
            .map_err(ProfileAuthCommitError::Storage)?;
        if let Err(error) = save_exact(self, &credential_guard, auth) {
            return Err(ProfileAuthCommitError::Storage(error));
        }
        if self
            .load_with_guard(&credential_guard)
            .map_err(ProfileAuthCommitError::Storage)?
            != Some(auth.clone())
        {
            restore_exact(self, &credential_guard, previous.as_ref())?;
            return Err(ProfileAuthCommitError::Storage(io::Error::other(
                "profile credential verification failed",
            )));
        }

        match store.compare_and_swap_with_guard(&registry_guard, current.generation, |registry| {
            *registry = planned.clone()
        }) {
            Ok(updated) => Ok(updated.generation),
            Err(RegistryStoreError::CommittedDurabilityUncertain { .. }) => {
                if store
                    .repair_committed_durability_with_guard(&registry_guard)
                    .or_else(|_| store.repair_committed_durability_with_guard(&registry_guard))
                    .is_err()
                {
                    return Err(ProfileAuthCommitError::CommittedDurabilityUncertain);
                }
                let installed = store.read().map_err(ProfileAuthCommitError::Registry)?;
                Ok(installed.generation)
            }
            Err(error) => {
                restore_exact(self, &credential_guard, previous.as_ref())?;
                Err(match error {
                    RegistryStoreError::Validation(
                        RegistryValidationError::DuplicateServiceIdentity { .. },
                    ) => ProfileAuthCommitError::DuplicateServiceIdentity,
                    error => ProfileAuthCommitError::Registry(error),
                })
            }
        }
    }

    pub(super) fn new_with_store(
        codex_home: &Path,
        account_id: AccountId,
        mode: AuthCredentialsStoreMode,
        keyring_store: Arc<dyn KeyringStore>,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> io::Result<Self> {
        if mode == AuthCredentialsStoreMode::Ephemeral {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile credential storage requires a persistent backend",
            ));
        }
        let accounts_home = codex_home.join(ACCOUNTS_DIRECTORY);
        ensure_private_directory(&accounts_home).map_err(private_storage_error)?;
        let profile_home = accounts_home.join(account_id.as_str());
        ensure_private_directory(&profile_home).map_err(private_storage_error)?;
        let profile_home = profile_home.canonicalize()?;
        let backend = create_auth_storage_with_store_and_namespace(
            profile_home.clone(),
            mode,
            keyring_store,
            keyring_backend_kind,
            AuthStorageNamespace::ProfileV1,
        );
        Ok(Self {
            account_id,
            profile_home,
            backend,
        })
    }

    pub(super) fn new_with_backend(
        codex_home: &Path,
        account_id: AccountId,
        backend_kind: PersistentAuthBackendKind,
        keyring_store: Arc<dyn KeyringStore>,
    ) -> io::Result<Self> {
        if backend_kind == PersistentAuthBackendKind::Ephemeral {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile credential storage requires a persistent backend",
            ));
        }
        let accounts_home = codex_home.join(ACCOUNTS_DIRECTORY);
        ensure_private_directory(&accounts_home).map_err(private_storage_error)?;
        let profile_home = accounts_home.join(account_id.as_str());
        ensure_private_directory(&profile_home).map_err(private_storage_error)?;
        let profile_home = profile_home.canonicalize()?;
        let backend = create_auth_storage_for_backend(
            profile_home.clone(),
            backend_kind,
            keyring_store,
            AuthStorageNamespace::ProfileV1,
        );
        Ok(Self {
            account_id,
            profile_home,
            backend,
        })
    }

    pub(super) fn acquire_lock(&self) -> io::Result<CredentialLockGuard> {
        self.backend.acquire_lock()
    }

    pub(super) fn load_with_guard(
        &self,
        guard: &CredentialLockGuard,
    ) -> io::Result<Option<AuthDotJson>> {
        self.backend.load_with_guard(guard)
    }

    pub(super) fn save_with_guard(
        &self,
        guard: &CredentialLockGuard,
        auth: &AuthDotJson,
    ) -> io::Result<()> {
        self.backend.save_with_guard(guard, auth)
    }

    pub(super) fn delete_with_guard(&self, guard: &CredentialLockGuard) -> io::Result<bool> {
        let removed = self.backend.delete_with_guard(guard)?;
        if removed {
            sync_directory(&self.profile_home)?;
        }
        Ok(removed)
    }

    pub(super) fn auth_storage(&self) -> Arc<AuthStorage> {
        Arc::clone(&self.backend)
    }

    pub(super) fn profile_home(&self) -> &Path {
        &self.profile_home
    }
}

fn save_exact(
    storage: &ProfileAuthStorage,
    guard: &CredentialLockGuard,
    auth: &AuthDotJson,
) -> io::Result<()> {
    match storage.save_with_guard(guard, auth) {
        Ok(()) => Ok(()),
        Err(error) if AuthFileDurabilityError::from_io_error(&error).is_some() => storage
            .backend
            .repair_durability_with_guard(guard)
            .or_else(|_| storage.backend.repair_durability_with_guard(guard)),
        Err(error) => Err(error),
    }
}

fn restore_exact(
    storage: &ProfileAuthStorage,
    guard: &CredentialLockGuard,
    previous: Option<&AuthDotJson>,
) -> Result<(), ProfileAuthCommitError> {
    let result = match previous {
        Some(previous) => save_exact(storage, guard, previous),
        None => storage.delete_with_guard(guard).map(|_| ()),
    };
    result.map_err(|_| ProfileAuthCommitError::RollbackFailed)?;
    if storage
        .load_with_guard(guard)
        .map_err(|_| ProfileAuthCommitError::RollbackFailed)?
        != previous.cloned()
    {
        return Err(ProfileAuthCommitError::RollbackFailed);
    }
    Ok(())
}

/// Result of checking or completing first-run persistent-auth migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyAuthMigrationOutcome {
    NoPersistentAuth,
    RegistryAlreadyInitialized,
    Migrated { account_id: AccountId },
    AlreadyCompleted { account_id: AccountId },
}

#[derive(Debug, Error)]
pub enum LegacyAuthMigrationError {
    #[error("legacy auth migration failed while attempting to {operation}: {source}")]
    Storage {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Registry(#[from] RegistryStoreError),
    #[error(transparent)]
    Journal(#[from] MigrationJournalError),
    #[error(transparent)]
    Validation(#[from] RegistryValidationError),
    #[error("legacy auth migration source is unavailable")]
    MissingSource,
    #[error("configured persistent auth does not contain usable credential material")]
    InvalidPersistentAuth,
    #[error("legacy auth changed while migration was in progress")]
    SourceChanged,
    #[error("legacy auth migration configuration changed after it started")]
    BackendConfigurationDrift,
    #[error("unsupported profile migration journal version")]
    UnsupportedJournalVersion,
    #[error("externally managed ChatGPT tokens cannot be migrated as persistent auth")]
    ExternalAuthUnsupported,
    #[error("profile credentials did not exactly match the legacy credential record")]
    VerificationFailed,
    #[error("legacy auth migration registry state conflicts with its journal")]
    RegistryConflict,
}

pub fn migrate_legacy_auth_if_needed(
    codex_home: &Path,
    mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<LegacyAuthMigrationOutcome, LegacyAuthMigrationError> {
    migration::migrate_legacy_auth_with_store(
        codex_home,
        mode,
        keyring_backend_kind,
        Arc::new(DefaultKeyringStore),
    )
}

fn storage_error(operation: &'static str, source: io::Error) -> LegacyAuthMigrationError {
    LegacyAuthMigrationError::Storage { operation, source }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    sync_parent_directory(&path.join(".profile-directory-sync")).map_err(private_storage_error)
}

fn private_storage_error(error: codex_private_storage::PrivateStorageError) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
