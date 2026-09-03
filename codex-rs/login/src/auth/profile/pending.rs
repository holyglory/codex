use std::fmt;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use codex_account_registry::AccountAlias;
use codex_account_registry::AccountId;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_config::types::AuthCredentialsStoreMode;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use codex_private_storage::AtomicWriteMode;
use codex_private_storage::ensure_private_file;
use codex_private_storage::sync_parent_directory;
use codex_private_storage::write_file_atomically;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use super::ProfileAuthStorage;
use crate::auth::AuthKeyringBackendKind;

const PENDING_LOGIN_VERSION: u32 = 1;
const PENDING_LOGIN_FILE: &str = ".pending-profile-login-v1.json";
const PENDING_LOGIN_NAMESPACE: &str = "profile-v1";
const PENDING_LOGIN_STALE_AFTER: Duration = Duration::minutes(15);

#[derive(Debug, Error)]
pub enum PendingProfileLoginError {
    #[error("pending account profile storage failed")]
    Storage(#[source] io::Error),
    #[error("pending account profile journal encoding failed")]
    Encoding(#[source] serde_json::Error),
    #[error("pending account profile journal is unsupported")]
    UnsupportedJournal,
    #[error("pending account profile configuration changed while login was interrupted")]
    ConfigurationDrift,
    #[error("pending account profile recovery found conflicting committed state")]
    RecoveryConflict,
    #[error("pending account profile registry is unavailable")]
    Registry(#[source] RegistryStoreError),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingProfileLoginJournal {
    version: u32,
    account_id: AccountId,
    alias: AccountAlias,
    started_at: DateTime<Utc>,
    storage_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    keyring_namespace: String,
}

pub struct PendingProfileLogin {
    journal_path: PathBuf,
    journal: PendingProfileLoginJournal,
    storage: ProfileAuthStorage,
}

impl fmt::Debug for PendingProfileLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingProfileLogin")
            .field("version", &self.journal.version)
            .field("account_id", &self.journal.account_id)
            .field("alias", &self.journal.alias)
            .finish_non_exhaustive()
    }
}

impl PendingProfileLogin {
    /// Creates an invisible profile and its durable, nonsecret journal. Callers serialize this
    /// operation with the registry lock, but release that lock before interactive authorization.
    pub fn begin(
        codex_home: &Path,
        alias: AccountAlias,
        mode: AuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> Result<Self, PendingProfileLoginError> {
        Self::begin_with_store(
            codex_home,
            alias,
            mode,
            keyring_backend_kind,
            Arc::new(DefaultKeyringStore),
        )
    }

    fn begin_with_store(
        codex_home: &Path,
        alias: AccountAlias,
        mode: AuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
        keyring_store: Arc<dyn KeyringStore>,
    ) -> Result<Self, PendingProfileLoginError> {
        recover_pending_profile_logins_with_store(
            codex_home,
            mode,
            keyring_backend_kind,
            Arc::clone(&keyring_store),
            Utc::now(),
        )?;
        let account_id = AccountId::generate();
        let storage = ProfileAuthStorage::new_with_store(
            codex_home,
            account_id.clone(),
            mode,
            keyring_store,
            keyring_backend_kind,
        )
        .map_err(PendingProfileLoginError::Storage)?;
        let journal_path = storage.profile_home().join(PENDING_LOGIN_FILE);
        let journal = PendingProfileLoginJournal {
            version: PENDING_LOGIN_VERSION,
            account_id,
            alias,
            started_at: Utc::now(),
            storage_mode: mode,
            keyring_backend_kind,
            keyring_namespace: PENDING_LOGIN_NAMESPACE.to_string(),
        };
        let mut encoded = match serde_json::to_vec_pretty(&journal) {
            Ok(encoded) => encoded,
            Err(error) => {
                let _ = cleanup_empty_profile(storage.profile_home());
                return Err(PendingProfileLoginError::Encoding(error));
            }
        };
        encoded.push(b'\n');
        if let Err(error) =
            write_file_atomically(&journal_path, &encoded, AtomicWriteMode::NoClobber)
        {
            let _ = remove_journal(&journal_path);
            let _ = cleanup_empty_profile(storage.profile_home());
            return Err(PendingProfileLoginError::Storage(io::Error::other(error)));
        }
        Ok(Self {
            journal_path,
            journal,
            storage,
        })
    }

    pub fn account_id(&self) -> &AccountId {
        &self.journal.account_id
    }

    pub fn alias(&self) -> &AccountAlias {
        &self.journal.alias
    }

    pub fn started_at(&self) -> DateTime<Utc> {
        self.journal.started_at
    }

    pub fn storage(&self) -> &ProfileAuthStorage {
        &self.storage
    }

    /// Removes credentials, the nonsecret journal, and empty pending directories after a failed
    /// or cancelled authorization. Credential deletion is verified by the configured backend.
    pub fn cleanup(self) -> Result<(), PendingProfileLoginError> {
        self.storage
            .delete()
            .map_err(PendingProfileLoginError::Storage)?;
        remove_journal(&self.journal_path)?;
        cleanup_empty_profile(self.storage.profile_home())?;
        Ok(())
    }

    /// Removes only the journal after registry commit. The authenticated profile remains owned by
    /// the newly visible account metadata.
    pub fn finish(self) -> Result<(), PendingProfileLoginError> {
        remove_journal(&self.journal_path)
    }
}

/// Reconciles completed pending logins and rolls back stale, uncommitted profiles.
///
/// Recent uncommitted journals are left untouched because another process may still be completing
/// interactive authorization. Per-profile journals do not prevent another profile from starting.
pub fn recover_pending_profile_logins(
    codex_home: &Path,
    mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<(), PendingProfileLoginError> {
    recover_pending_profile_logins_with_store(
        codex_home,
        mode,
        keyring_backend_kind,
        Arc::new(DefaultKeyringStore),
        Utc::now(),
    )
}

fn recover_pending_profile_logins_with_store(
    codex_home: &Path,
    mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    keyring_store: Arc<dyn KeyringStore>,
    now: DateTime<Utc>,
) -> Result<(), PendingProfileLoginError> {
    let accounts_home = codex_home.join("accounts");
    let entries = match std::fs::read_dir(&accounts_home) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(PendingProfileLoginError::Storage(error)),
    };
    let registry = match RegistryStore::new(codex_home).read() {
        Ok(registry) => registry,
        Err(RegistryStoreError::NotFound) => AccountRegistry::default(),
        Err(error) => return Err(PendingProfileLoginError::Registry(error)),
    };
    for entry in entries {
        let entry = entry.map_err(PendingProfileLoginError::Storage)?;
        if !entry
            .file_type()
            .map_err(PendingProfileLoginError::Storage)?
            .is_dir()
        {
            continue;
        }
        let journal_path = entry.path().join(PENDING_LOGIN_FILE);
        if !journal_path.exists() {
            continue;
        }
        let journal = read_journal(&journal_path)?;
        if journal.version != PENDING_LOGIN_VERSION
            || journal.keyring_namespace != PENDING_LOGIN_NAMESPACE
            || entry.file_name().to_string_lossy() != journal.account_id.as_str()
        {
            return Err(PendingProfileLoginError::UnsupportedJournal);
        }
        let committed = registry
            .accounts
            .iter()
            .find(|account| account.id == journal.account_id);
        if committed.is_some()
            && (journal.storage_mode != mode
                || journal.keyring_backend_kind != keyring_backend_kind)
        {
            return Err(PendingProfileLoginError::ConfigurationDrift);
        }
        let storage = ProfileAuthStorage::new_with_store(
            codex_home,
            journal.account_id.clone(),
            journal.storage_mode,
            Arc::clone(&keyring_store),
            journal.keyring_backend_kind,
        )
        .map_err(PendingProfileLoginError::Storage)?;
        if let Some(account) = committed {
            let first = storage.load().map_err(PendingProfileLoginError::Storage)?;
            let second = storage.load().map_err(PendingProfileLoginError::Storage)?;
            if account.alias != journal.alias
                || first != second
                || first
                    .as_ref()
                    .is_none_or(|auth| !metadata_matches(account, auth))
            {
                return Err(PendingProfileLoginError::RecoveryConflict);
            }
            remove_journal(&journal_path)?;
        } else if now.signed_duration_since(journal.started_at) >= PENDING_LOGIN_STALE_AFTER {
            storage
                .delete()
                .map_err(PendingProfileLoginError::Storage)?;
            remove_journal(&journal_path)?;
            cleanup_empty_profile(storage.profile_home())?;
        }
    }
    Ok(())
}

fn read_journal(path: &Path) -> Result<PendingProfileLoginJournal, PendingProfileLoginError> {
    ensure_private_file(path)
        .map_err(|error| PendingProfileLoginError::Storage(io::Error::other(error)))?;
    let encoded = std::fs::read(path).map_err(PendingProfileLoginError::Storage)?;
    serde_json::from_slice(&encoded).map_err(PendingProfileLoginError::Encoding)
}

fn metadata_matches(account: &AccountMetadata, auth: &crate::auth::AuthDotJson) -> bool {
    let identity = auth.profile_metadata();
    account.auth_mode == identity.auth_mode
        && account.email == identity.email
        && account.plan_type == identity.plan_type
        && account
            .service_account_id
            .as_ref()
            .map(codex_account_registry::OpaqueServiceId::expose)
            == identity.service_account_id.as_deref()
        && account
            .service_workspace_id
            .as_ref()
            .map(codex_account_registry::OpaqueServiceId::expose)
            == identity.service_workspace_id.as_deref()
}

fn remove_journal(path: &Path) -> Result<(), PendingProfileLoginError> {
    match std::fs::remove_file(path) {
        Ok(()) => match sync_parent_directory(path) {
            Ok(()) => Ok(()),
            Err(_) => sync_parent_directory(path)
                .map_err(|error| PendingProfileLoginError::Storage(io::Error::other(error))),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PendingProfileLoginError::Storage(error)),
    }
}

fn cleanup_empty_profile(profile_home: &Path) -> Result<(), PendingProfileLoginError> {
    for file_name in [".auth-credential-lock-v1", ".profile-use-lock-v1"] {
        remove_if_present(&profile_home.join(file_name))?;
    }
    let secrets = profile_home.join("secrets");
    remove_empty_directory(&secrets)?;
    remove_empty_directory(profile_home)?;
    sync_parent_directory(profile_home)
        .map_err(|error| PendingProfileLoginError::Storage(io::Error::other(error)))
}

fn remove_if_present(path: &Path) -> Result<(), PendingProfileLoginError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PendingProfileLoginError::Storage(error)),
    }
}

fn remove_empty_directory(path: &Path) -> Result<(), PendingProfileLoginError> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PendingProfileLoginError::Storage(error)),
    }
}

#[cfg(test)]
#[path = "pending_tests.rs"]
mod tests;
