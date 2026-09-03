use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::Weak;

use chrono::Utc;
use codex_account_registry::AccountId;
use codex_account_registry::AccountLookupError;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_account_selection::AccountLimitCache;
use codex_account_selection::CacheUpdate;
use codex_account_selection::LimitCacheError;
use codex_account_selection::SelectionError;
use codex_account_selection::SelectionReason;
use codex_account_selection::SelectionRequest;
use codex_account_selection::UnknownReason;
use codex_account_selection::select_account;
use codex_config::types::AuthCredentialsStoreMode;
use thiserror::Error;
use tokio::sync::RwLock as AsyncRwLock;
use tokio::sync::Semaphore;

mod selection_probe;

use super::ProfileAuthStorage;
use super::migrate_legacy_auth_if_needed;
use super::pending::recover_pending_profile_logins;
use super::use_lock::ProfileUseGuard;
use super::use_lock::acquire_profile_use;
use super::use_lock::try_acquire_profile_removal;
use crate::auth::AuthConfig;
use crate::auth::AuthManager;
use crate::auth::CodexAuth;
use crate::auth::agent_identity::agent_identity_authapi_base_url;
use crate::auth::is_workload_identity_selected;
use crate::auth::manager::CODEX_ACCESS_TOKEN_ENV_VAR;
use crate::auth::manager::CODEX_API_KEY_ENV_VAR;
use crate::auth::storage::create_auth_storage;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalAuthConflictSource {
    CodexApiKeyEnvironment,
    CodexAccessTokenEnvironment,
    ExternalChatgpt,
    WorkloadIdentity,
    HeaderOrHost,
    EphemeralStorage,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RouterExternalAuthState {
    pub codex_api_key_environment: bool,
    pub codex_access_token_environment: bool,
    pub external_chatgpt: bool,
    pub workload_identity: bool,
    pub header_or_host: bool,
}

impl RouterExternalAuthState {
    pub fn with_process_environment(mut self) -> Self {
        self.codex_api_key_environment |= environment_value_present(CODEX_API_KEY_ENV_VAR);
        self.codex_access_token_environment |=
            environment_value_present(CODEX_ACCESS_TOKEN_ENV_VAR);
        self.workload_identity |= is_workload_identity_selected();
        self
    }

    fn first_active(self) -> Option<ExternalAuthConflictSource> {
        if self.codex_api_key_environment {
            Some(ExternalAuthConflictSource::CodexApiKeyEnvironment)
        } else if self.codex_access_token_environment {
            Some(ExternalAuthConflictSource::CodexAccessTokenEnvironment)
        } else if self.external_chatgpt {
            Some(ExternalAuthConflictSource::ExternalChatgpt)
        } else if self.workload_identity {
            Some(ExternalAuthConflictSource::WorkloadIdentity)
        } else if self.header_or_host {
            Some(ExternalAuthConflictSource::HeaderOrHost)
        } else {
            None
        }
    }
}

fn environment_value_present(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

#[derive(Clone)]
pub struct ProfileAuthRouterConfig {
    pub auth_config: AuthConfig,
    pub process_pin: Option<AccountId>,
    pub external_auth: RouterExternalAuthState,
}

impl fmt::Debug for ProfileAuthRouterConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProfileAuthRouterConfig")
            .field("has_process_pin", &self.process_pin.is_some())
            .field("external_auth", &self.external_auth)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum ProfileAuthRouterError {
    #[error("account profile routing requires persistent authentication storage")]
    EphemeralStorageUnsupported,
    #[error("process account pin conflicts with an external authentication source: {conflict:?}")]
    PinConflict {
        conflict: ExternalAuthConflictSource,
    },
    #[error("external authentication must use the upstream singular auth path: {conflict:?}")]
    ExternalAuthRequiresSingularPath {
        conflict: ExternalAuthConflictSource,
    },
    #[error("account registry is unavailable")]
    Registry(#[source] RegistryStoreError),
    #[error("account profile migration failed")]
    Migration(#[source] super::LegacyAuthMigrationError),
    #[error("account profile authentication could not be loaded")]
    Authentication(#[source] std::io::Error),
    #[error("selected account profile is unknown")]
    UnknownAccount,
    #[error("selected account profile reference is ambiguous")]
    AmbiguousAccount,
    #[error("selected account profile is disabled")]
    DisabledAccount,
    #[error("selected account profile is not authenticated")]
    NotAuthenticated,
    #[error("account registry changed during turn-boundary reload")]
    RegistryChanged,
    #[error("account profile is in use by an active lease")]
    AccountInUse,
    #[error("account selection failed")]
    Selection(#[source] SelectionError),
}

impl From<RegistryStoreError> for ProfileAuthRouterError {
    fn from(source: RegistryStoreError) -> Self {
        Self::Registry(source)
    }
}

impl ProfileAuthRouterError {
    pub fn safe_message(&self) -> String {
        match self {
            Self::Selection(SelectionError::CapacityUnknown(UnknownReason::Stale))
            | Self::Selection(SelectionError::NoEligibleAccount {
                current: SelectionReason::CurrentUnknown(UnknownReason::Stale),
            }) => "automatic account selection failed: rate-limit state is stale".to_string(),
            Self::Selection(SelectionError::CapacityUnknown(_))
            | Self::Selection(SelectionError::NoEligibleAccount {
                current: SelectionReason::CurrentUnknown(_),
            }) => "automatic account selection failed: capacity is unknown".to_string(),
            Self::Selection(SelectionError::NoEligibleAccount { .. }) => {
                "automatic account selection failed: no eligible account is available".to_string()
            }
            Self::Selection(SelectionError::LimitReached(_)) => {
                "selected account has reached its limit".to_string()
            }
            error => error.to_string(),
        }
    }
}

struct RouterInner {
    root_home: PathBuf,
    auth_config: AuthConfig,
    registry_store: RegistryStore,
    registry: RwLock<AccountRegistry>,
    managers: RwLock<HashMap<AccountId, Arc<AuthManager>>>,
    process_pin: Option<AccountId>,
    active_leases: Mutex<HashMap<AccountId, usize>>,
}

#[derive(Clone)]
pub struct ProfileAuthRouter {
    inner: Arc<RouterInner>,
}

struct SharedRouterInner {
    auth_config: AuthConfig,
    router: AsyncRwLock<Option<ProfileAuthRouter>>,
    process_pin: Option<String>,
    external_auth: RouterExternalAuthState,
    upstream_auth_manager: Option<Arc<AuthManager>>,
    limit_cache: Mutex<AccountLimitCache>,
    selected_account: Mutex<SelectedAccountState>,
    selection_probe_lock: Semaphore,
}

#[derive(Default)]
struct SelectedAccountState {
    account_id: Option<AccountId>,
    observed_default_account_id: Option<AccountId>,
}

const MAX_LIMIT_AGE_SECONDS: i64 = 300;

/// Lazily shares one profile router between management RPCs and turn-boundary leases.
///
/// A missing registry preserves the upstream singular authentication path. Once a profile
/// registry exists, every caller observes the same router instance and its generation reloads.
#[derive(Clone)]
pub struct SharedProfileAuthRouter {
    inner: Arc<SharedRouterInner>,
}

impl fmt::Debug for SharedProfileAuthRouter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SharedProfileAuthRouter([private])")
    }
}

impl SharedProfileAuthRouter {
    pub fn new(auth_config: AuthConfig) -> Self {
        Self {
            inner: Arc::new(SharedRouterInner {
                auth_config,
                router: AsyncRwLock::new(None),
                process_pin: None,
                external_auth: RouterExternalAuthState::default(),
                upstream_auth_manager: None,
                limit_cache: Mutex::new(AccountLimitCache::default()),
                selected_account: Mutex::new(SelectedAccountState::default()),
                selection_probe_lock: Semaphore::new(1),
            }),
        }
    }

    pub fn new_with_external_auth(
        auth_config: AuthConfig,
        external_auth: RouterExternalAuthState,
        upstream_auth_manager: Arc<AuthManager>,
    ) -> Self {
        Self {
            inner: Arc::new(SharedRouterInner {
                auth_config,
                router: AsyncRwLock::new(None),
                process_pin: None,
                external_auth,
                upstream_auth_manager: Some(upstream_auth_manager),
                limit_cache: Mutex::new(AccountLimitCache::default()),
                selected_account: Mutex::new(SelectedAccountState::default()),
                selection_probe_lock: Semaphore::new(1),
            }),
        }
    }

    pub fn new_pinned(
        auth_config: AuthConfig,
        process_pin: String,
        external_auth: RouterExternalAuthState,
        upstream_auth_manager: Arc<AuthManager>,
    ) -> Self {
        Self {
            inner: Arc::new(SharedRouterInner {
                auth_config,
                router: AsyncRwLock::new(None),
                process_pin: Some(process_pin),
                external_auth,
                upstream_auth_manager: Some(upstream_auth_manager),
                limit_cache: Mutex::new(AccountLimitCache::default()),
                selected_account: Mutex::new(SelectedAccountState::default()),
                selection_probe_lock: Semaphore::new(1),
            }),
        }
    }

    pub fn auth_config(&self) -> &AuthConfig {
        &self.inner.auth_config
    }

    pub async fn router_if_configured(
        &self,
    ) -> Result<Option<ProfileAuthRouter>, ProfileAuthRouterError> {
        self.router_if_configured_with_external(RouterExternalAuthState::default())
            .await
    }

    async fn router_if_configured_with_external(
        &self,
        additional_external_auth: RouterExternalAuthState,
    ) -> Result<Option<ProfileAuthRouter>, ProfileAuthRouterError> {
        if let Some(conflict) = self.external_conflict(additional_external_auth)? {
            if self.inner.process_pin.is_some() {
                return Err(ProfileAuthRouterError::PinConflict { conflict });
            }
            return Ok(None);
        }
        if let Some(router) = self.inner.router.read().await.as_ref() {
            return Ok(Some(router.clone()));
        }
        let opened = match self.inner.process_pin.as_deref() {
            Some(reference) => {
                let account_id = self.resolve_process_pin(reference)?;
                Some(
                    ProfileAuthRouter::open(ProfileAuthRouterConfig {
                        auth_config: self.inner.auth_config.clone(),
                        process_pin: Some(account_id),
                        external_auth: self.inner.external_auth,
                    })
                    .await?,
                )
            }
            None => {
                match ProfileAuthRouter::open_for_management(self.inner.auth_config.clone()).await {
                    Ok(router) => Some(router),
                    Err(ProfileAuthRouterError::Registry(RegistryStoreError::NotFound)) => None,
                    Err(error) => return Err(error),
                }
            }
        };
        let Some(opened) = opened else {
            return Ok(None);
        };
        let mut router = self.inner.router.write().await;
        let router = router.get_or_insert(opened).clone();
        Ok(Some(router))
    }

    pub async fn require_router(&self) -> Result<ProfileAuthRouter, ProfileAuthRouterError> {
        self.router_if_configured()
            .await?
            .ok_or(ProfileAuthRouterError::UnknownAccount)
    }

    pub async fn lease_for_turn_if_configured(
        &self,
    ) -> Result<Option<AccountLease>, ProfileAuthRouterError> {
        match self.router_if_configured().await? {
            Some(router) => router.lease_for_turn().await.map(Some),
            None => Ok(None),
        }
    }

    pub async fn lease_for_operation(&self) -> Result<AuthManagerLease, ProfileAuthRouterError> {
        match self.router_if_configured().await? {
            Some(router) => match router.lease_for_turn().await {
                Ok(lease) => Ok(AuthManagerLease::profile(lease)),
                Err(ProfileAuthRouterError::UnknownAccount)
                    if self.inner.process_pin.is_none()
                        && router.registry_snapshot()?.accounts.is_empty() =>
                {
                    self.inner
                        .upstream_auth_manager
                        .as_ref()
                        .cloned()
                        .map(AuthManagerLease::legacy)
                        .ok_or(ProfileAuthRouterError::NotAuthenticated)
                }
                Err(error) => Err(error),
            },
            None => self
                .inner
                .upstream_auth_manager
                .as_ref()
                .cloned()
                .map(AuthManagerLease::legacy)
                .ok_or(ProfileAuthRouterError::NotAuthenticated),
        }
    }

    /// Acquires a lease for one persisted profile identity.
    pub async fn lease_for_account(
        &self,
        account_id: &AccountId,
    ) -> Result<AuthManagerLease, ProfileAuthRouterError> {
        self.require_router()
            .await?
            .lease_for_account(account_id)
            .await
            .map(AuthManagerLease::profile)
    }

    /// Parses and acquires a lease for one persisted profile identity.
    pub async fn lease_for_account_id(
        &self,
        account_id: &str,
    ) -> Result<AuthManagerLease, ProfileAuthRouterError> {
        let account_id = account_id
            .parse::<AccountId>()
            .map_err(|_| ProfileAuthRouterError::UnknownAccount)?;
        self.lease_for_account(&account_id).await
    }

    /// Returns legacy authority only when profile routing has never been configured.
    pub async fn legacy_lease_if_profiles_unconfigured(
        &self,
    ) -> Result<AuthManagerLease, ProfileAuthRouterError> {
        if self.router_if_configured().await?.is_some() {
            return Err(ProfileAuthRouterError::UnknownAccount);
        }
        self.inner
            .upstream_auth_manager
            .as_ref()
            .cloned()
            .map(AuthManagerLease::legacy)
            .ok_or(ProfileAuthRouterError::NotAuthenticated)
    }

    pub async fn lease_for_turn_with_external_auth(
        &self,
        external_auth: RouterExternalAuthState,
    ) -> Result<Option<AccountLease>, ProfileAuthRouterError> {
        let Some(router) = self
            .router_if_configured_with_external(external_auth)
            .await?
        else {
            return Ok(None);
        };
        if router.inner.process_pin.is_some() {
            return router.lease_for_turn().await.map(Some);
        }
        router.reload_at_turn_boundary().await?;
        let registry = router.registry_snapshot()?;
        if !registry.auto_selection.enabled {
            let account_id = registry
                .default_account_id
                .as_ref()
                .ok_or(ProfileAuthRouterError::UnknownAccount)?;
            let lease = router.lease(&registry, account_id)?;
            let mut selected = self
                .inner
                .selected_account
                .lock()
                .map_err(|_| router_state_error())?;
            selected.account_id = Some(lease.account_id().clone());
            selected.observed_default_account_id = registry.default_account_id;
            return Ok(Some(lease));
        }
        let current_account_id = {
            let selected = self
                .inner
                .selected_account
                .lock()
                .map_err(|_| router_state_error())?;
            if selected.observed_default_account_id != registry.default_account_id {
                registry.default_account_id.clone()
            } else {
                selected
                    .account_id
                    .clone()
                    .or_else(|| registry.default_account_id.clone())
            }
        };
        let cache = self
            .inner
            .limit_cache
            .lock()
            .map_err(|_| router_state_error())?
            .clone();
        let lease = router.lease_for_turn_with_loaded_selection(
            &cache,
            current_account_id.as_ref(),
            /*relevant_limit_id*/ None,
            Utc::now().timestamp(),
            MAX_LIMIT_AGE_SECONDS,
        )?;
        let mut selected = self
            .inner
            .selected_account
            .lock()
            .map_err(|_| router_state_error())?;
        selected.account_id = Some(lease.account_id().clone());
        selected.observed_default_account_id = registry.default_account_id;
        Ok(Some(lease))
    }

    pub fn record_rate_limits(
        &self,
        account_id: AccountId,
        observed_at: i64,
        snapshots: Vec<codex_protocol::protocol::RateLimitSnapshot>,
    ) -> Result<CacheUpdate, LimitCacheError> {
        self.inner
            .limit_cache
            .lock()
            .map_err(|_| LimitCacheError::ConflictingObservation)?
            .update(account_id, observed_at, snapshots)
    }

    pub fn observe_rate_limits(
        &self,
        account_id: AccountId,
        observed_at: i64,
        snapshots: Vec<codex_protocol::protocol::RateLimitSnapshot>,
    ) -> Result<CacheUpdate, LimitCacheError> {
        self.inner
            .limit_cache
            .lock()
            .map_err(|_| LimitCacheError::ConflictingObservation)?
            .observe(account_id, observed_at, snapshots)
    }

    pub fn remove_rate_limits(&self, account_id: &AccountId) {
        if let Ok(mut cache) = self.inner.limit_cache.lock() {
            cache.remove(account_id);
        }
        if let Ok(mut selected) = self.inner.selected_account.lock()
            && selected.account_id.as_ref() == Some(account_id)
        {
            selected.account_id = None;
        }
    }

    fn resolve_process_pin(&self, reference: &str) -> Result<AccountId, ProfileAuthRouterError> {
        migrate_legacy_auth_if_needed(
            &self.inner.auth_config.codex_home,
            self.inner.auth_config.auth_credentials_store_mode,
            self.inner.auth_config.keyring_backend_kind,
        )
        .map_err(ProfileAuthRouterError::Migration)?;
        let registry = match RegistryStore::new(&self.inner.auth_config.codex_home).read() {
            Ok(registry) => registry,
            Err(RegistryStoreError::NotFound) => {
                return Err(ProfileAuthRouterError::UnknownAccount);
            }
            Err(error) => return Err(ProfileAuthRouterError::Registry(error)),
        };
        registry
            .lookup(reference)
            .map(|account| account.id.clone())
            .map_err(|error| match error {
                AccountLookupError::Unknown { .. } => ProfileAuthRouterError::UnknownAccount,
                AccountLookupError::Ambiguous { .. } => ProfileAuthRouterError::AmbiguousAccount,
                AccountLookupError::Disabled { .. } => ProfileAuthRouterError::DisabledAccount,
            })
    }

    fn external_conflict(
        &self,
        additional: RouterExternalAuthState,
    ) -> Result<Option<ExternalAuthConflictSource>, ProfileAuthRouterError> {
        let mut state = self.inner.external_auth;
        state.codex_api_key_environment |= additional.codex_api_key_environment;
        state.codex_access_token_environment |= additional.codex_access_token_environment;
        state.external_chatgpt |= additional.external_chatgpt;
        state.workload_identity |= additional.workload_identity;
        state.header_or_host |= additional.header_or_host;
        state = state.with_process_environment();
        if let Some(manager) = &self.inner.upstream_auth_manager {
            state.workload_identity |= manager.is_workload_identity_selected();
            state.external_chatgpt |= manager.is_external_chatgpt_auth_active();
        }
        if let Some(conflict) = state.first_active() {
            return Ok(Some(conflict));
        }
        let ephemeral = create_auth_storage(
            self.inner.auth_config.codex_home.clone(),
            AuthCredentialsStoreMode::Ephemeral,
            self.inner.auth_config.keyring_backend_kind,
        )
        .load()
        .map_err(ProfileAuthRouterError::Authentication)?
        .is_some();
        Ok(ephemeral.then_some(ExternalAuthConflictSource::EphemeralStorage))
    }
}

impl fmt::Debug for ProfileAuthRouter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProfileAuthRouter")
            .field("has_process_pin", &self.inner.process_pin.is_some())
            .finish_non_exhaustive()
    }
}

pub struct AccountLease {
    account_id: AccountId,
    registry_generation: u64,
    auth_manager: Arc<AuthManager>,
    _registration: Arc<LeaseRegistration>,
    automatic_switch: bool,
}

#[derive(Clone)]
pub struct AuthManagerLease {
    auth_manager: Arc<AuthManager>,
    _account_lease: Option<AccountLease>,
}

impl fmt::Debug for AuthManagerLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthManagerLease")
            .field("profile_scoped", &self._account_lease.is_some())
            .finish_non_exhaustive()
    }
}

impl AuthManagerLease {
    pub fn profile(account_lease: AccountLease) -> Self {
        Self {
            auth_manager: Arc::clone(account_lease.auth_manager()),
            _account_lease: Some(account_lease),
        }
    }

    pub fn legacy(auth_manager: Arc<AuthManager>) -> Self {
        Self {
            auth_manager,
            _account_lease: None,
        }
    }

    pub fn auth_manager(&self) -> &Arc<AuthManager> {
        &self.auth_manager
    }

    pub fn is_profile_scoped(&self) -> bool {
        self._account_lease.is_some()
    }

    pub fn account_id(&self) -> Option<&AccountId> {
        self._account_lease.as_ref().map(AccountLease::account_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileRemovalOutcome {
    pub generation: u64,
    pub default_account_id: Option<AccountId>,
    pub credentials_removed: bool,
}

impl Clone for AccountLease {
    fn clone(&self) -> Self {
        Self {
            account_id: self.account_id.clone(),
            registry_generation: self.registry_generation,
            auth_manager: Arc::clone(&self.auth_manager),
            _registration: Arc::clone(&self._registration),
            automatic_switch: self.automatic_switch,
        }
    }
}

impl fmt::Debug for AccountLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountLease")
            .field("account_id", &self.account_id)
            .field("registry_generation", &self.registry_generation)
            .finish_non_exhaustive()
    }
}

impl AccountLease {
    pub fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    pub fn registry_generation(&self) -> u64 {
        self.registry_generation
    }

    pub fn auth_manager(&self) -> &Arc<AuthManager> {
        &self.auth_manager
    }

    pub fn automatic_switched(&self) -> bool {
        self.automatic_switch
    }
}

struct LeaseRegistration {
    router: Weak<RouterInner>,
    account_id: AccountId,
    _use_guard: ProfileUseGuard,
}

impl Drop for LeaseRegistration {
    fn drop(&mut self) {
        let Some(router) = self.router.upgrade() else {
            return;
        };
        if let Ok(mut leases) = router.active_leases.lock()
            && let Some(count) = leases.get_mut(&self.account_id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                leases.remove(&self.account_id);
            }
        }
    }
}

impl ProfileAuthRouter {
    pub async fn open(config: ProfileAuthRouterConfig) -> Result<Self, ProfileAuthRouterError> {
        if config.auth_config.auth_credentials_store_mode == AuthCredentialsStoreMode::Ephemeral {
            return Err(ProfileAuthRouterError::EphemeralStorageUnsupported);
        }
        let mut external = config
            .external_auth
            .with_process_environment()
            .first_active();
        if external.is_none()
            && create_auth_storage(
                config.auth_config.codex_home.clone(),
                AuthCredentialsStoreMode::Ephemeral,
                config.auth_config.keyring_backend_kind,
            )
            .load()
            .map_err(ProfileAuthRouterError::Authentication)?
            .is_some()
        {
            external = Some(ExternalAuthConflictSource::EphemeralStorage);
        }
        if let Some(conflict) = external {
            return Err(if config.process_pin.is_some() {
                ProfileAuthRouterError::PinConflict { conflict }
            } else {
                ProfileAuthRouterError::ExternalAuthRequiresSingularPath { conflict }
            });
        }

        Self::open_persistent(config.auth_config, config.process_pin).await
    }

    /// Opens profile routing for an explicit local management operation. Environment or host auth
    /// remains untouched and is not used by the returned profile managers.
    pub async fn open_for_management(
        auth_config: AuthConfig,
    ) -> Result<Self, ProfileAuthRouterError> {
        if auth_config.auth_credentials_store_mode == AuthCredentialsStoreMode::Ephemeral {
            return Err(ProfileAuthRouterError::EphemeralStorageUnsupported);
        }
        let root_home = auth_config.codex_home.clone();
        migrate_legacy_auth_if_needed(
            &root_home,
            auth_config.auth_credentials_store_mode,
            auth_config.keyring_backend_kind,
        )
        .map_err(ProfileAuthRouterError::Migration)?;
        recover_pending_profile_logins(
            &root_home,
            auth_config.auth_credentials_store_mode,
            auth_config.keyring_backend_kind,
        )
        .map_err(|error| ProfileAuthRouterError::Authentication(std::io::Error::other(error)))?;
        let registry_store = RegistryStore::new(&root_home);
        let registry = registry_store.read()?;
        Ok(Self {
            inner: Arc::new(RouterInner {
                root_home,
                auth_config,
                registry_store,
                registry: RwLock::new(registry),
                managers: RwLock::new(HashMap::new()),
                process_pin: None,
                active_leases: Mutex::new(HashMap::new()),
            }),
        })
    }

    async fn open_persistent(
        auth_config: AuthConfig,
        process_pin: Option<AccountId>,
    ) -> Result<Self, ProfileAuthRouterError> {
        let root_home = auth_config.codex_home.clone();
        migrate_legacy_auth_if_needed(
            &root_home,
            auth_config.auth_credentials_store_mode,
            auth_config.keyring_backend_kind,
        )
        .map_err(ProfileAuthRouterError::Migration)?;
        recover_pending_profile_logins(
            &root_home,
            auth_config.auth_credentials_store_mode,
            auth_config.keyring_backend_kind,
        )
        .map_err(|error| ProfileAuthRouterError::Authentication(std::io::Error::other(error)))?;
        let registry_store = RegistryStore::new(&root_home);
        let registry = registry_store.read()?;
        let managers = load_managers(&root_home, &auth_config, &registry).await?;
        if let Some(pin) = &process_pin {
            validate_target(&registry, &managers, pin)?;
        }
        Ok(Self {
            inner: Arc::new(RouterInner {
                root_home,
                auth_config,
                registry_store,
                registry: RwLock::new(registry),
                managers: RwLock::new(managers),
                process_pin,
                active_leases: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub async fn reload_at_turn_boundary(&self) -> Result<u64, ProfileAuthRouterError> {
        for _ in 0..3 {
            let snapshot = self.inner.registry_store.read()?;
            let previous = self
                .inner
                .managers
                .read()
                .map_err(|_| router_state_error())?
                .clone();
            let mut managers = HashMap::new();
            for account in &snapshot.accounts {
                if let Some(manager) = previous.get(&account.id) {
                    manager.reload().await;
                    managers.insert(account.id.clone(), Arc::clone(manager));
                } else {
                    managers.insert(
                        account.id.clone(),
                        load_manager(&self.inner.root_home, &self.inner.auth_config, account)
                            .await?,
                    );
                }
            }
            if self.inner.registry_store.read()?.generation != snapshot.generation {
                continue;
            }
            if let Some(pin) = &self.inner.process_pin {
                validate_target(&snapshot, &managers, pin)?;
            }
            *self
                .inner
                .registry
                .write()
                .map_err(|_| router_state_error())? = snapshot.clone();
            *self
                .inner
                .managers
                .write()
                .map_err(|_| router_state_error())? = managers;
            return Ok(snapshot.generation);
        }
        Err(ProfileAuthRouterError::RegistryChanged)
    }

    pub async fn lease_for_turn(&self) -> Result<AccountLease, ProfileAuthRouterError> {
        self.reload_at_turn_boundary().await?;
        let registry = self.registry_snapshot()?;
        let account_id = self
            .inner
            .process_pin
            .as_ref()
            .or(registry.default_account_id.as_ref())
            .ok_or(ProfileAuthRouterError::UnknownAccount)?
            .clone();
        self.lease(&registry, &account_id)
    }

    pub fn process_pin_account_id(&self) -> Option<AccountId> {
        self.inner.process_pin.clone()
    }

    pub async fn active_account_id_at_turn_boundary(
        &self,
    ) -> Result<AccountId, ProfileAuthRouterError> {
        self.reload_at_turn_boundary().await?;
        let registry = self.registry_snapshot()?;
        self.inner
            .process_pin
            .clone()
            .or(registry.default_account_id)
            .ok_or(ProfileAuthRouterError::UnknownAccount)
    }

    pub async fn lease_for_account(
        &self,
        account_id: &AccountId,
    ) -> Result<AccountLease, ProfileAuthRouterError> {
        for _ in 0..3 {
            let registry = self.inner.registry_store.read()?;
            let account = registry
                .accounts
                .iter()
                .find(|account| &account.id == account_id)
                .ok_or(ProfileAuthRouterError::UnknownAccount)?;
            let existing_manager = {
                self.inner
                    .managers
                    .read()
                    .map_err(|_| router_state_error())?
                    .get(account_id)
                    .cloned()
            };
            let manager = match existing_manager {
                Some(manager) => {
                    manager.reload().await;
                    manager
                }
                None => {
                    load_manager(&self.inner.root_home, &self.inner.auth_config, account).await?
                }
            };
            if self.inner.registry_store.read()?.generation != registry.generation {
                continue;
            }
            self.inner
                .managers
                .write()
                .map_err(|_| router_state_error())?
                .insert(account_id.clone(), manager);
            *self
                .inner
                .registry
                .write()
                .map_err(|_| router_state_error())? = registry.clone();
            return self.lease(&registry, account_id);
        }
        Err(ProfileAuthRouterError::RegistryChanged)
    }

    /// Returns the process-active profile manager for an explicit credential management operation.
    /// A process pin takes precedence over the global default. Unlike a turn lease, this permits a
    /// logged-out profile so it can be authorized again.
    pub async fn active_manager_for_management(
        &self,
    ) -> Result<(AccountId, Arc<AuthManager>), ProfileAuthRouterError> {
        self.reload_at_turn_boundary().await?;
        let registry = self.registry_snapshot()?;
        let account_id = self
            .inner
            .process_pin
            .clone()
            .or(registry.default_account_id)
            .ok_or(ProfileAuthRouterError::UnknownAccount)?;
        let account = registry
            .accounts
            .iter()
            .find(|account| account.id == account_id)
            .ok_or(ProfileAuthRouterError::UnknownAccount)?;
        if !account.enabled {
            return Err(ProfileAuthRouterError::DisabledAccount);
        }
        let manager = self
            .manager_snapshot()?
            .get(&account_id)
            .cloned()
            .ok_or(ProfileAuthRouterError::UnknownAccount)?;
        Ok((account_id, manager))
    }

    pub async fn lease_for_turn_with_selection(
        &self,
        cache: &AccountLimitCache,
        current_account_id: Option<&AccountId>,
        relevant_limit_id: Option<&str>,
        now: i64,
        max_limit_age_seconds: i64,
    ) -> Result<AccountLease, ProfileAuthRouterError> {
        if self.inner.process_pin.is_some() {
            return self.lease_for_turn().await;
        }
        self.reload_at_turn_boundary().await?;
        self.lease_for_turn_with_loaded_selection(
            cache,
            current_account_id,
            relevant_limit_id,
            now,
            max_limit_age_seconds,
        )
    }

    fn lease_for_turn_with_loaded_selection(
        &self,
        cache: &AccountLimitCache,
        current_account_id: Option<&AccountId>,
        relevant_limit_id: Option<&str>,
        now: i64,
        max_limit_age_seconds: i64,
    ) -> Result<AccountLease, ProfileAuthRouterError> {
        let managers = self.manager_snapshot()?;
        let authenticated_accounts = managers
            .iter()
            .filter(|(_, manager)| manager.auth_cached().is_some())
            .map(|(id, _)| id.clone())
            .collect::<HashSet<_>>();
        self.lease_for_turn_with_authenticated_accounts(
            cache,
            current_account_id,
            relevant_limit_id,
            now,
            max_limit_age_seconds,
            &authenticated_accounts,
        )
    }

    fn lease_for_turn_with_authenticated_accounts(
        &self,
        cache: &AccountLimitCache,
        current_account_id: Option<&AccountId>,
        relevant_limit_id: Option<&str>,
        now: i64,
        max_limit_age_seconds: i64,
        authenticated_accounts: &HashSet<AccountId>,
    ) -> Result<AccountLease, ProfileAuthRouterError> {
        let registry = self.registry_snapshot()?;
        let decision = select_account(
            &registry,
            cache,
            SelectionRequest {
                current_account_id,
                pinned_account_id: None,
                authenticated_accounts,
                relevant_limit_id,
                now,
                max_limit_age_seconds,
            },
        )
        .map_err(ProfileAuthRouterError::Selection)?;
        let mut lease = self.lease(&registry, &decision.account_id)?;
        lease.automatic_switch = decision.switched;
        Ok(lease)
    }

    pub async fn activate_default(
        &self,
        account_id: &AccountId,
        expected_generation: u64,
    ) -> Result<u64, ProfileAuthRouterError> {
        self.reload_at_turn_boundary().await?;
        let registry = self.registry_snapshot()?;
        let managers = self.manager_snapshot()?;
        validate_target(&registry, &managers, account_id)?;
        let guard = self.inner.registry_store.acquire_lock()?;
        let updated = self.inner.registry_store.compare_and_swap_with_guard(
            &guard,
            expected_generation,
            |registry| {
                registry.default_account_id = Some(account_id.clone());
                if let Some(account) = registry
                    .accounts
                    .iter_mut()
                    .find(|account| &account.id == account_id)
                {
                    account.last_used_at = Some(Utc::now());
                }
            },
        )?;
        *self
            .inner
            .registry
            .write()
            .map_err(|_| router_state_error())? = updated.clone();
        Ok(updated.generation)
    }

    pub fn check_removal_allowed(
        &self,
        account_id: &AccountId,
    ) -> Result<(), ProfileAuthRouterError> {
        if self.inner.process_pin.as_ref() == Some(account_id)
            || self
                .inner
                .active_leases
                .lock()
                .map_err(|_| router_state_error())?
                .get(account_id)
                .copied()
                .unwrap_or(0)
                > 0
        {
            return Err(ProfileAuthRouterError::AccountInUse);
        }
        Ok(())
    }

    pub fn check_credential_management_allowed(
        &self,
        account_id: &AccountId,
    ) -> Result<(), ProfileAuthRouterError> {
        if self
            .inner
            .active_leases
            .lock()
            .map_err(|_| router_state_error())?
            .get(account_id)
            .copied()
            .unwrap_or(0)
            > 0
        {
            return Err(ProfileAuthRouterError::AccountInUse);
        }
        Ok(())
    }

    /// Removes one persistent profile while holding the registry, profile-use, and credential
    /// locks in that order. Active turn leases hold a shared profile-use lock, including across
    /// processes, so removal cannot race an in-flight turn.
    pub fn remove_persistent_account(
        auth_config: &AuthConfig,
        account_id: &AccountId,
        expected_generation: u64,
    ) -> Result<ProfileRemovalOutcome, ProfileAuthRouterError> {
        let registry_store = RegistryStore::new(&auth_config.codex_home);
        let registry_guard = registry_store.acquire_lock()?;
        let current = registry_store.read()?;
        if current.generation != expected_generation {
            return Err(ProfileAuthRouterError::Registry(
                RegistryStoreError::GenerationConflict {
                    expected: expected_generation,
                    actual: current.generation,
                },
            ));
        }
        if !current
            .accounts
            .iter()
            .any(|account| &account.id == account_id)
        {
            return Err(ProfileAuthRouterError::UnknownAccount);
        }

        let mut planned = current;
        planned.accounts.retain(|account| &account.id != account_id);
        if planned.default_account_id.as_ref() == Some(account_id) {
            planned.default_account_id = authenticated_fallback(auth_config, &planned)?;
        }
        planned.validate().map_err(RegistryStoreError::Validation)?;

        let profile = ProfileAuthStorage::new(
            &auth_config.codex_home,
            account_id.clone(),
            auth_config.auth_credentials_store_mode,
            auth_config.keyring_backend_kind,
        )
        .map_err(ProfileAuthRouterError::Authentication)?;
        let Some(_removal_guard) = try_acquire_profile_removal(profile.profile_home())
            .map_err(ProfileAuthRouterError::Authentication)?
        else {
            return Err(ProfileAuthRouterError::AccountInUse);
        };
        let credential_guard = profile
            .acquire_lock()
            .map_err(ProfileAuthRouterError::Authentication)?;
        let credentials_removed = profile
            .delete_with_guard(&credential_guard)
            .map_err(ProfileAuthRouterError::Authentication)?;
        let update = registry_store.compare_and_swap_with_guard(
            &registry_guard,
            expected_generation,
            |registry| *registry = planned.clone(),
        );
        let updated = match update {
            Ok(updated) => updated,
            Err(RegistryStoreError::CommittedDurabilityUncertain { .. }) => {
                registry_store.repair_committed_durability_with_guard(&registry_guard)?;
                registry_store.read()?
            }
            Err(error) => return Err(ProfileAuthRouterError::Registry(error)),
        };
        Ok(ProfileRemovalOutcome {
            generation: updated.generation,
            default_account_id: updated.default_account_id,
            credentials_removed,
        })
    }

    fn lease(
        &self,
        registry: &AccountRegistry,
        account_id: &AccountId,
    ) -> Result<AccountLease, ProfileAuthRouterError> {
        let managers = self.manager_snapshot()?;
        validate_target(registry, &managers, account_id)?;
        let manager = managers
            .get(account_id)
            .ok_or(ProfileAuthRouterError::UnknownAccount)?;
        let profile = ProfileAuthStorage::new(
            &self.inner.root_home,
            account_id.clone(),
            self.inner.auth_config.auth_credentials_store_mode,
            self.inner.auth_config.keyring_backend_kind,
        )
        .map_err(ProfileAuthRouterError::Authentication)?;
        let use_guard = acquire_profile_use(profile.profile_home())
            .map_err(ProfileAuthRouterError::Authentication)?;
        let mut leases = self
            .inner
            .active_leases
            .lock()
            .map_err(|_| router_state_error())?;
        *leases.entry(account_id.clone()).or_insert(0) += 1;
        drop(leases);
        Ok(AccountLease {
            account_id: account_id.clone(),
            registry_generation: registry.generation,
            auth_manager: Arc::clone(manager),
            _registration: Arc::new(LeaseRegistration {
                router: Arc::downgrade(&self.inner),
                account_id: account_id.clone(),
                _use_guard: use_guard,
            }),
            automatic_switch: false,
        })
    }

    fn registry_snapshot(&self) -> Result<AccountRegistry, ProfileAuthRouterError> {
        self.inner
            .registry
            .read()
            .map(|registry| registry.clone())
            .map_err(|_| router_state_error())
    }

    fn manager_snapshot(
        &self,
    ) -> Result<HashMap<AccountId, Arc<AuthManager>>, ProfileAuthRouterError> {
        self.inner
            .managers
            .read()
            .map(|managers| managers.clone())
            .map_err(|_| router_state_error())
    }
}

fn authenticated_fallback(
    auth_config: &AuthConfig,
    registry: &AccountRegistry,
) -> Result<Option<AccountId>, ProfileAuthRouterError> {
    for account in registry.enabled_by_priority() {
        let profile = ProfileAuthStorage::new(
            &auth_config.codex_home,
            account.id.clone(),
            auth_config.auth_credentials_store_mode,
            auth_config.keyring_backend_kind,
        )
        .map_err(ProfileAuthRouterError::Authentication)?;
        if profile
            .load()
            .map_err(ProfileAuthRouterError::Authentication)?
            .is_some()
        {
            return Ok(Some(account.id.clone()));
        }
    }
    Ok(None)
}

fn validate_target(
    registry: &AccountRegistry,
    managers: &HashMap<AccountId, Arc<AuthManager>>,
    account_id: &AccountId,
) -> Result<(), ProfileAuthRouterError> {
    let account = registry
        .accounts
        .iter()
        .find(|account| &account.id == account_id)
        .ok_or(ProfileAuthRouterError::UnknownAccount)?;
    if !account.enabled {
        return Err(ProfileAuthRouterError::DisabledAccount);
    }
    if managers
        .get(account_id)
        .and_then(|manager| manager.auth_cached())
        .is_none()
    {
        return Err(ProfileAuthRouterError::NotAuthenticated);
    }
    Ok(())
}

async fn load_managers(
    root_home: &std::path::Path,
    auth_config: &AuthConfig,
    registry: &AccountRegistry,
) -> Result<HashMap<AccountId, Arc<AuthManager>>, ProfileAuthRouterError> {
    let mut managers = HashMap::new();
    for account in &registry.accounts {
        managers.insert(
            account.id.clone(),
            load_manager(root_home, auth_config, account).await?,
        );
    }
    Ok(managers)
}

async fn load_manager(
    root_home: &std::path::Path,
    auth_config: &AuthConfig,
    account: &AccountMetadata,
) -> Result<Arc<AuthManager>, ProfileAuthRouterError> {
    let profile = ProfileAuthStorage::new(
        root_home,
        account.id.clone(),
        auth_config.auth_credentials_store_mode,
        auth_config.keyring_backend_kind,
    )
    .map_err(ProfileAuthRouterError::Authentication)?;
    let auth_dot_json = profile
        .load()
        .map_err(ProfileAuthRouterError::Authentication)?;
    let auth = match auth_dot_json {
        Some(auth_dot_json) => {
            if auth_dot_json.resolved_mode() != account.auth_mode {
                return Err(ProfileAuthRouterError::Authentication(
                    std::io::Error::other("profile auth mode conflicts with account metadata"),
                ));
            }
            let authapi_base_url =
                agent_identity_authapi_base_url(auth_config.chatgpt_base_url.as_deref()).ok();
            Some(
                CodexAuth::from_profile_storage(
                    &profile,
                    auth_dot_json,
                    auth_config.chatgpt_base_url.as_deref(),
                    auth_config.keyring_backend_kind,
                    authapi_base_url.as_deref(),
                    &auth_config.auth_route_config,
                )
                .await
                .map_err(ProfileAuthRouterError::Authentication)?,
            )
        }
        None => None,
    };
    let mut manager_config = auth_config.clone();
    manager_config.codex_home = profile.profile_home().to_path_buf();
    AuthManager::shared_from_resolved_profile(manager_config, auth)
        .map_err(ProfileAuthRouterError::Authentication)
}

fn router_state_error() -> ProfileAuthRouterError {
    ProfileAuthRouterError::Authentication(std::io::Error::other(
        "account profile router state is unavailable",
    ))
}

#[cfg(test)]
#[path = "router_tests.rs"]
mod tests;
