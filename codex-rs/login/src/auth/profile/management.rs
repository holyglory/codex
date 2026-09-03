use codex_account_registry::AccountId;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_protocol::auth::AuthMode;
use thiserror::Error;

use super::ProfileAuthStorage;
use crate::AuthConfig;

const MAX_MUTATION_ATTEMPTS: usize = 3;

/// Credential-free account metadata intended for local management surfaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedAccountSummary {
    /// Internal local profile identity. Management surfaces should prefer `alias` and must not
    /// confuse this value with an opaque service or workspace identifier.
    pub account_id: String,
    pub alias: String,
    pub auth_mode: AuthMode,
    pub enabled: bool,
    pub authenticated: bool,
    pub priority: u32,
    pub is_default: bool,
}

/// One generation-consistent view of the local account registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedAccountSnapshot {
    pub generation: u64,
    pub auto_selection_enabled: bool,
    pub accounts: Vec<ManagedAccountSummary>,
}

/// Result of one priority mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedAccountPriorityMutation {
    pub changed_count: usize,
    pub snapshot: ManagedAccountSnapshot,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AccountManagementError {
    #[error("account registry is unavailable")]
    RegistryUnavailable,
    #[error("account credential backend is unavailable")]
    CredentialStoreUnavailable,
    #[error("account profile was not found")]
    UnknownAccount,
    #[error("account profile reference is ambiguous")]
    AmbiguousAccount,
    #[error("account registry changed concurrently")]
    GenerationConflict,
}

/// Reads only nonsecret account metadata and credential presence.
pub fn read_managed_accounts(
    config: &AuthConfig,
) -> Result<ManagedAccountSnapshot, AccountManagementError> {
    let registry = read_or_empty(&RegistryStore::new(&config.codex_home))?;
    snapshot(config, registry)
}

/// Sets one profile priority using an optional generation precondition.
pub fn set_managed_account_priority(
    config: &AuthConfig,
    reference: &str,
    priority: u32,
    expected_generation: Option<u64>,
) -> Result<ManagedAccountPriorityMutation, AccountManagementError> {
    let store = RegistryStore::new(&config.codex_home);
    let (registry, changed_count) = mutate_registry(&store, expected_generation, |current| {
        let id = resolve_account_id(current, reference)?;
        let mut planned = current.clone();
        let account = planned
            .accounts
            .iter_mut()
            .find(|account| account.id == id)
            .ok_or(AccountManagementError::UnknownAccount)?;
        let changed = usize::from(account.priority != priority);
        account.priority = priority;
        Ok((planned, changed))
    })?;
    Ok(ManagedAccountPriorityMutation {
        changed_count,
        snapshot: snapshot(config, registry)?,
    })
}

/// Atomically sets every configured profile to one priority.
pub fn set_all_managed_account_priorities(
    config: &AuthConfig,
    priority: u32,
    expected_generation: Option<u64>,
) -> Result<ManagedAccountPriorityMutation, AccountManagementError> {
    let store = RegistryStore::new(&config.codex_home);
    let (registry, changed_count) = mutate_registry(&store, expected_generation, |current| {
        let mut planned = current.clone();
        let mut changed_count = 0;
        for account in &mut planned.accounts {
            if account.priority != priority {
                account.priority = priority;
                changed_count += 1;
            }
        }
        Ok((planned, changed_count))
    })?;
    Ok(ManagedAccountPriorityMutation {
        changed_count,
        snapshot: snapshot(config, registry)?,
    })
}

fn snapshot(
    config: &AuthConfig,
    registry: AccountRegistry,
) -> Result<ManagedAccountSnapshot, AccountManagementError> {
    let mut accounts = registry
        .accounts
        .iter()
        .map(|account| summary(config, &registry, account))
        .collect::<Result<Vec<_>, _>>()?;
    accounts.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.alias.cmp(&right.alias))
            .then_with(|| left.account_id.cmp(&right.account_id))
    });
    Ok(ManagedAccountSnapshot {
        generation: registry.generation,
        auto_selection_enabled: registry.auto_selection.enabled,
        accounts,
    })
}

fn summary(
    config: &AuthConfig,
    registry: &AccountRegistry,
    account: &AccountMetadata,
) -> Result<ManagedAccountSummary, AccountManagementError> {
    let storage = ProfileAuthStorage::new(
        &config.codex_home,
        account.id.clone(),
        config.auth_credentials_store_mode,
        config.keyring_backend_kind,
    )
    .map_err(|_| AccountManagementError::CredentialStoreUnavailable)?;
    let auth = storage
        .load()
        .map_err(|_| AccountManagementError::CredentialStoreUnavailable)?;
    if auth
        .as_ref()
        .is_some_and(|auth| auth.resolved_mode() != account.auth_mode)
    {
        return Err(AccountManagementError::RegistryUnavailable);
    }
    Ok(ManagedAccountSummary {
        account_id: account.id.to_string(),
        alias: account.alias.to_string(),
        auth_mode: account.auth_mode,
        enabled: account.enabled,
        authenticated: auth.is_some(),
        priority: account.priority,
        is_default: registry.default_account_id.as_ref() == Some(&account.id),
    })
}

fn mutate_registry<T>(
    store: &RegistryStore,
    expected_generation: Option<u64>,
    mut plan: impl FnMut(&AccountRegistry) -> Result<(AccountRegistry, T), AccountManagementError>,
) -> Result<(AccountRegistry, T), AccountManagementError> {
    for _ in 0..MAX_MUTATION_ATTEMPTS {
        let current = store.read().map_err(map_read_error)?;
        if expected_generation.is_some_and(|expected| expected != current.generation) {
            return Err(AccountManagementError::GenerationConflict);
        }
        let (planned, value) = plan(&current)?;
        if planned == current {
            return Ok((current, value));
        }
        planned
            .validate()
            .map_err(|_| AccountManagementError::RegistryUnavailable)?;
        let guard = store
            .acquire_lock()
            .map_err(|_| AccountManagementError::RegistryUnavailable)?;
        match store.compare_and_swap_with_guard(&guard, current.generation, |registry| {
            *registry = planned.clone();
        }) {
            Ok(updated) => return Ok((updated, value)),
            Err(RegistryStoreError::GenerationConflict { .. }) if expected_generation.is_none() => {
            }
            Err(RegistryStoreError::GenerationConflict { .. }) => {
                return Err(AccountManagementError::GenerationConflict);
            }
            Err(RegistryStoreError::CommittedDurabilityUncertain { .. }) => {
                store
                    .repair_committed_durability_with_guard(&guard)
                    .map_err(|_| AccountManagementError::RegistryUnavailable)?;
                return Ok((store.read().map_err(map_read_error)?, value));
            }
            Err(_) => return Err(AccountManagementError::RegistryUnavailable),
        }
    }
    Err(AccountManagementError::GenerationConflict)
}

fn read_or_empty(store: &RegistryStore) -> Result<AccountRegistry, AccountManagementError> {
    match store.read() {
        Ok(registry) => Ok(registry),
        Err(RegistryStoreError::NotFound) => Ok(AccountRegistry::default()),
        Err(error) => Err(map_read_error(error)),
    }
}

fn map_read_error(error: RegistryStoreError) -> AccountManagementError {
    match error {
        RegistryStoreError::NotFound => AccountManagementError::UnknownAccount,
        RegistryStoreError::GenerationConflict { .. } => AccountManagementError::GenerationConflict,
        RegistryStoreError::Io { .. }
        | RegistryStoreError::Parse(_)
        | RegistryStoreError::Validation(_)
        | RegistryStoreError::AlreadyExists
        | RegistryStoreError::GenerationOverflow
        | RegistryStoreError::LockBusy
        | RegistryStoreError::GuardMismatch
        | RegistryStoreError::CommittedDurabilityUncertain { .. }
        | RegistryStoreError::UnsupportedSecurityPlatform => {
            AccountManagementError::RegistryUnavailable
        }
    }
}

fn resolve_account_id(
    registry: &AccountRegistry,
    reference: &str,
) -> Result<AccountId, AccountManagementError> {
    let alias_match = registry
        .accounts
        .iter()
        .find(|account| account.alias.as_str() == reference);
    let id_match = reference
        .parse::<AccountId>()
        .ok()
        .and_then(|id| registry.accounts.iter().find(|account| account.id == id));
    match (alias_match, id_match) {
        (Some(alias), Some(id)) if alias.id != id.id => {
            Err(AccountManagementError::AmbiguousAccount)
        }
        (Some(account), _) | (_, Some(account)) => Ok(account.id.clone()),
        (None, None) => Err(AccountManagementError::UnknownAccount),
    }
}

#[cfg(test)]
#[path = "management_tests.rs"]
mod tests;
