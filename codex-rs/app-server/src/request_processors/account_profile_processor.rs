mod login;

use crate::auth_mode::auth_mode_to_api;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::outgoing_message::OutgoingMessageSender;
use chrono::Utc;
use codex_account_registry::AccountAlias;
use codex_account_registry::AccountId;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_account_registry::RegistryValidationError;
use codex_account_registry::SelectionPolicy;
use codex_app_server_protocol::AccountAutoSelection;
use codex_app_server_protocol::AccountAutoSelectionPolicy;
use codex_app_server_protocol::AccountAutoSelectionReadParams;
use codex_app_server_protocol::AccountAutoSelectionReadResponse;
use codex_app_server_protocol::AccountAutoSelectionWriteParams;
use codex_app_server_protocol::AccountAutoSelectionWriteResponse;
use codex_app_server_protocol::AccountPriorityOrder;
use codex_app_server_protocol::AccountProfile;
use codex_app_server_protocol::AccountProfileActivateParams;
use codex_app_server_protocol::AccountProfileActivateResponse;
use codex_app_server_protocol::AccountProfileActiveChangedNotification;
use codex_app_server_protocol::AccountProfileListParams;
use codex_app_server_protocol::AccountProfileListResponse;
use codex_app_server_protocol::AccountProfileRateLimitReadParams;
use codex_app_server_protocol::AccountProfileRateLimitReadResponse;
use codex_app_server_protocol::AccountProfileReadParams;
use codex_app_server_protocol::AccountProfileReadResponse;
use codex_app_server_protocol::AccountProfileRemoveParams;
use codex_app_server_protocol::AccountProfileRemoveResponse;
use codex_app_server_protocol::AccountProfileUpdateParams;
use codex_app_server_protocol::AccountProfileUpdateResponse;
use codex_app_server_protocol::AccountUpdatedNotification;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_backend_client::Client as BackendClient;
use codex_core::config::Config;
use codex_login::CodexAuth;
use codex_login::ProfileAuthRouter;
use codex_login::ProfileAuthRouterError;
use codex_login::ProfileAuthStorage;
use codex_login::SharedProfileAuthRouter;
use codex_model_provider::is_supported_amazon_bedrock_region;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const DEFAULT_PAGE_LIMIT: u32 = 50;
const MAX_PAGE_LIMIT: u32 = 100;
const MAX_CURSOR_BYTES: usize = 256;
const MAX_NOTE_BYTES: usize = 1_024;
const MAX_MUTATION_ATTEMPTS: usize = 3;
const LIMIT_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub(crate) struct AccountProfileRequestProcessor {
    pub(super) config: Arc<Config>,
    pub(super) store: RegistryStore,
    pub(super) router: SharedProfileAuthRouter,
    pub(super) outgoing: Arc<OutgoingMessageSender>,
    active_login: Arc<Mutex<Option<login::ActiveProfileLogin>>>,
}

impl AccountProfileRequestProcessor {
    pub(crate) fn new(
        config: Arc<Config>,
        router: SharedProfileAuthRouter,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> Self {
        Self {
            store: RegistryStore::new(&config.codex_home),
            config,
            router,
            outgoing,
            active_login: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) async fn list(
        &self,
        params: AccountProfileListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let registry = self.read_or_empty()?;
        let active_account_id = self.process_active_account_id().await?;
        let limit = validated_limit(params.limit)?;
        let cursor = params
            .cursor
            .map(|cursor| parse_cursor(&cursor))
            .transpose()?;
        let mut accounts = registry.accounts.iter().collect::<Vec<_>>();
        accounts.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.alias.cmp(&right.alias))
                .then_with(|| left.id.cmp(&right.id))
        });
        if let Some((priority, alias, id)) = cursor {
            accounts.retain(|account| {
                account.priority < priority
                    || (account.priority == priority
                        && (account.alias > alias || (account.alias == alias && account.id > id)))
            });
        }
        let has_more = accounts.len() > limit as usize;
        let data = accounts
            .into_iter()
            .take(limit as usize)
            .map(|account| {
                api_profile(&self.config, account, &registry, active_account_id.as_ref())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = has_more
            .then(|| data.last())
            .flatten()
            .map(|account| format!("v2|{}|{}|{}", account.priority, account.alias, account.id));
        Ok(Some(
            AccountProfileListResponse { data, next_cursor }.into(),
        ))
    }

    pub(crate) async fn read(
        &self,
        params: AccountProfileReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let registry = self.read_registry()?;
        let active_account_id = self.process_active_account_id().await?;
        let account = find_account(&registry, &params.account_id)?;
        Ok(Some(
            AccountProfileReadResponse {
                profile: api_profile(&self.config, account, &registry, active_account_id.as_ref())?,
            }
            .into(),
        ))
    }

    pub(crate) async fn activate(
        &self,
        params: AccountProfileActivateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let account_id = account_id(&params.account_id)?;
        let mut previous = None;
        let mut generation = None;
        for _ in 0..MAX_MUTATION_ATTEMPTS {
            let registry = self.read_registry()?;
            previous = registry.default_account_id.clone();
            let router = self.router.require_router().await.map_err(router_error)?;
            match router
                .activate_default(&account_id, registry.generation)
                .await
            {
                Ok(updated) => {
                    generation = Some(updated);
                    break;
                }
                Err(ProfileAuthRouterError::Registry(RegistryStoreError::GenerationConflict {
                    ..
                }))
                | Err(ProfileAuthRouterError::RegistryChanged) => {}
                Err(error) => return Err(router_error(error)),
            }
        }
        let generation = generation.ok_or_else(generation_conflict)?;
        let registry = self.read_registry()?;
        let active_account_id = self.process_active_account_id().await?;
        let account = find_account(&registry, account_id.as_str())?;
        self.send_active_notifications(previous.as_ref(), generation, /*force_account*/ true)
            .await?;
        Ok(Some(
            AccountProfileActivateResponse {
                profile: api_profile(&self.config, account, &registry, active_account_id.as_ref())?,
            }
            .into(),
        ))
    }

    pub(crate) async fn update(
        &self,
        params: AccountProfileUpdateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        if params.note.is_some() && params.clear_note {
            return Err(invalid_params("note and clearNote cannot be used together"));
        }
        if params
            .note
            .as_ref()
            .is_some_and(|note| note.len() > MAX_NOTE_BYTES || note.chars().any(char::is_control))
        {
            return Err(invalid_params("account note is invalid"));
        }
        let id = account_id(&params.account_id)?;
        let alias = params
            .alias
            .as_deref()
            .map(AccountAlias::from_str)
            .transpose()
            .map_err(|_| invalid_params("account alias is invalid"))?;
        let config = Arc::clone(&self.config);
        let (registry, previous_default) = self.mutate_registry(|current| {
            let mut planned = current.clone();
            let previous_default = planned.default_account_id.clone();
            let account = account_mut(&mut planned, &id)?;
            if let Some(alias) = &alias {
                account.alias = alias.clone();
            }
            if let Some(priority) = params.priority {
                account.priority = priority;
            }
            if let Some(note) = &params.note {
                account.note = Some(note.clone());
            } else if params.clear_note {
                account.note = None;
            }
            if let Some(enabled) = params.enabled {
                account.enabled = enabled;
                if !enabled && planned.default_account_id.as_ref() == Some(&id) {
                    planned.default_account_id =
                        authenticated_fallback(&config, &planned, Some(&id))?;
                } else if enabled && planned.default_account_id.is_none() {
                    let selected = planned
                        .accounts
                        .iter()
                        .find(|account| account.id == id)
                        .ok_or_else(resource_not_found)?;
                    if is_authenticated(&config, selected)? {
                        planned.default_account_id = Some(id.clone());
                    }
                }
            }
            planned.validate().map_err(registry_validation_error)?;
            Ok((planned, previous_default))
        })?;
        let account = find_account(&registry, id.as_str())?;
        let active_account_id = self.process_active_account_id().await?;
        if previous_default != registry.default_account_id {
            self.send_active_notifications(
                previous_default.as_ref(),
                registry.generation,
                /*force_account*/ true,
            )
            .await?;
        }
        Ok(Some(
            AccountProfileUpdateResponse {
                profile: api_profile(&self.config, account, &registry, active_account_id.as_ref())?,
            }
            .into(),
        ))
    }

    pub(crate) async fn remove(
        &self,
        params: AccountProfileRemoveParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let id = account_id(&params.account_id)?;
        if let Some(router) = self
            .router
            .router_if_configured()
            .await
            .map_err(router_error)?
        {
            router.check_removal_allowed(&id).map_err(router_error)?;
        }
        let mut outcome = None;
        let mut previous_default = None;
        for _ in 0..MAX_MUTATION_ATTEMPTS {
            let registry = self.read_registry()?;
            previous_default = registry.default_account_id.clone();
            match ProfileAuthRouter::remove_persistent_account(
                &self.config.auth_config(),
                &id,
                registry.generation,
            ) {
                Ok(removed) => {
                    outcome = Some(removed);
                    break;
                }
                Err(ProfileAuthRouterError::Registry(RegistryStoreError::GenerationConflict {
                    ..
                })) => {}
                Err(error) => return Err(router_error(error)),
            }
        }
        let outcome = outcome.ok_or_else(generation_conflict)?;
        self.router.remove_rate_limits(&id);
        if previous_default != outcome.default_account_id {
            self.send_active_notifications(
                previous_default.as_ref(),
                outcome.generation,
                /*force_account*/ true,
            )
            .await?;
        }
        Ok(Some(
            AccountProfileRemoveResponse {
                account_id: id.to_string(),
            }
            .into(),
        ))
    }

    pub(crate) async fn rate_limits(
        &self,
        params: AccountProfileRateLimitReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let id = account_id(&params.account_id)?;
        let registry = self.read_registry()?;
        let account = find_account(&registry, id.as_str())?;
        if !account.enabled {
            return Err(invalid_params("account profile is disabled"));
        }
        let router = self.router.require_router().await.map_err(router_error)?;
        let lease = router.lease_for_account(&id).await.map_err(router_error)?;
        let auth = lease
            .auth_manager()
            .auth()
            .await
            .ok_or_else(|| invalid_params("account profile is not authenticated"))?;
        if !auth.uses_codex_backend() {
            return Err(invalid_params(
                "account profile does not support service rate limits",
            ));
        }
        let client = BackendClient::from_auth(
            self.config.chatgpt_base_url.clone(),
            &auth,
            self.config.http_client_factory(),
        );
        let mut data = tokio::time::timeout(LIMIT_FETCH_TIMEOUT, client.get_rate_limits_many())
            .await
            .map_err(|_| internal_error("account profile rate-limit request timed out"))?
            .map_err(|_| internal_error("account profile rate limits are unavailable"))?;
        if data.is_empty() || data.iter().any(invalid_snapshot) {
            return Err(internal_error(
                "account profile rate-limit response is invalid",
            ));
        }
        for snapshot in &mut data {
            if snapshot.limit_id.is_none() {
                snapshot.limit_id = Some("codex".to_string());
            }
        }
        let observed_at = Utc::now().timestamp();
        self.router
            .record_rate_limits(id.clone(), observed_at, data.clone())
            .map_err(|_| internal_error("account profile rate-limit response is invalid"))?;
        if let Some(limit_id) = params.limit_id {
            data.retain(|snapshot| snapshot.limit_id.as_deref() == Some(limit_id.as_str()));
            if data.is_empty() {
                return Err(invalid_params("rate-limit bucket was not found"));
            }
        }
        data.sort_by(|left, right| {
            left.limit_id
                .cmp(&right.limit_id)
                .then_with(|| left.limit_name.cmp(&right.limit_name))
        });
        Ok(Some(
            AccountProfileRateLimitReadResponse {
                account_id: id.to_string(),
                data: data.into_iter().map(Into::into).collect(),
                observed_at: Some(observed_at),
            }
            .into(),
        ))
    }

    pub(crate) async fn auto_read(
        &self,
        _params: AccountAutoSelectionReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let registry = self.read_or_empty()?;
        Ok(Some(
            AccountAutoSelectionReadResponse {
                auto_selection: api_auto_selection(&registry),
            }
            .into(),
        ))
    }

    pub(crate) async fn auto_write(
        &self,
        params: AccountAutoSelectionWriteParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let enabled = params.enabled;
        match params.policy {
            AccountAutoSelectionPolicy::Priority => {}
        }
        let (registry, ()) = self.mutate_registry(|current| {
            let mut planned = current.clone();
            planned.auto_selection.enabled = enabled;
            planned.auto_selection.policy = SelectionPolicy::Priority;
            Ok((planned, ()))
        })?;
        Ok(Some(
            AccountAutoSelectionWriteResponse {
                auto_selection: api_auto_selection(&registry),
            }
            .into(),
        ))
    }

    pub(super) fn read_registry(&self) -> Result<AccountRegistry, JSONRPCErrorError> {
        self.store.read().map_err(registry_error)
    }

    pub(super) fn read_or_empty(&self) -> Result<AccountRegistry, JSONRPCErrorError> {
        match self.store.read() {
            Ok(registry) => Ok(registry),
            Err(RegistryStoreError::NotFound) => Ok(AccountRegistry::default()),
            Err(error) => Err(registry_error(error)),
        }
    }

    pub(super) fn mutate_registry<T>(
        &self,
        mut plan: impl FnMut(&AccountRegistry) -> Result<(AccountRegistry, T), JSONRPCErrorError>,
    ) -> Result<(AccountRegistry, T), JSONRPCErrorError> {
        for _ in 0..MAX_MUTATION_ATTEMPTS {
            let (current, missing) = match self.store.read() {
                Ok(registry) => (registry, false),
                Err(RegistryStoreError::NotFound) => (AccountRegistry::default(), true),
                Err(error) => return Err(registry_error(error)),
            };
            let (planned, value) = plan(&current)?;
            planned.validate().map_err(registry_validation_error)?;
            let guard = self.store.acquire_lock().map_err(registry_error)?;
            let result = if missing {
                self.store
                    .create_with_guard(&guard, &planned)
                    .map(|()| planned.clone())
            } else {
                self.store
                    .compare_and_swap_with_guard(&guard, current.generation, |registry| {
                        *registry = planned.clone()
                    })
            };
            match result {
                Ok(updated) => return Ok((updated, value)),
                Err(RegistryStoreError::AlreadyExists)
                | Err(RegistryStoreError::GenerationConflict { .. }) => {}
                Err(RegistryStoreError::CommittedDurabilityUncertain { .. }) => {
                    self.store
                        .repair_committed_durability_with_guard(&guard)
                        .map_err(registry_error)?;
                    return Ok((self.read_registry()?, value));
                }
                Err(error) => return Err(registry_error(error)),
            }
        }
        Err(generation_conflict())
    }

    pub(super) async fn send_active_notifications(
        &self,
        previous: Option<&AccountId>,
        generation: u64,
        force_account: bool,
    ) -> Result<(), JSONRPCErrorError> {
        let router = self
            .router
            .router_if_configured()
            .await
            .map_err(router_error)?;
        let (current, previous_active) = match &router {
            Some(router) => {
                let current = Some(
                    router
                        .active_account_id_at_turn_boundary()
                        .await
                        .map_err(router_error)?,
                );
                let previous_active = router
                    .process_pin_account_id()
                    .or_else(|| previous.cloned());
                (current, previous_active)
            }
            None => (None, None),
        };
        if force_account || current != previous_active {
            let auth = match (&router, current.as_ref()) {
                (Some(router), Some(account_id)) => router
                    .lease_for_account(account_id)
                    .await
                    .ok()
                    .and_then(|lease| lease.auth_manager().auth_cached()),
                (Some(_) | None, None) | (None, Some(_)) => None,
            };
            self.outgoing
                .send_server_notification(ServerNotification::AccountUpdated(
                    AccountUpdatedNotification {
                        auth_mode: auth
                            .as_ref()
                            .map(CodexAuth::api_auth_mode)
                            .map(auth_mode_to_api),
                        plan_type: auth.as_ref().and_then(CodexAuth::account_plan_type),
                    },
                ))
                .await;
        }
        if current != previous_active
            && let Some(account_id) = current.as_ref()
        {
            self.outgoing
                .send_server_notification(ServerNotification::AccountProfileActiveChanged(
                    AccountProfileActiveChangedNotification {
                        account_id: account_id.to_string(),
                        previous_account_id: previous_active.as_ref().map(ToString::to_string),
                        changed_at: Utc::now().timestamp(),
                        generation,
                    },
                ))
                .await;
        }
        Ok(())
    }

    async fn process_active_account_id(&self) -> Result<Option<AccountId>, JSONRPCErrorError> {
        let Some(router) = self
            .router
            .router_if_configured()
            .await
            .map_err(router_error)?
        else {
            return Ok(None);
        };
        match router.active_account_id_at_turn_boundary().await {
            Ok(account_id) => Ok(Some(account_id)),
            Err(ProfileAuthRouterError::UnknownAccount) => Ok(None),
            Err(error) => Err(router_error(error)),
        }
    }
}

fn api_profile(
    config: &Config,
    account: &AccountMetadata,
    registry: &AccountRegistry,
    active_account_id: Option<&AccountId>,
) -> Result<AccountProfile, JSONRPCErrorError> {
    Ok(AccountProfile {
        id: account.id.to_string(),
        alias: account.alias.to_string(),
        auth_mode: auth_mode_to_api(account.auth_mode),
        email: account.email.clone(),
        plan_type: account.plan_type.clone().map(Into::into),
        enabled: account.enabled,
        authenticated: is_authenticated(config, account)?,
        priority: account.priority,
        created_at: account.created_at.timestamp(),
        last_used_at: account.last_used_at.map(|time| time.timestamp()),
        note: account.note.clone(),
        is_default: registry.default_account_id.as_ref() == Some(&account.id),
        is_active: active_account_id == Some(&account.id),
    })
}

fn api_auto_selection(registry: &AccountRegistry) -> AccountAutoSelection {
    AccountAutoSelection {
        enabled: registry.auto_selection.enabled,
        policy: match registry.auto_selection.policy {
            SelectionPolicy::Priority => AccountAutoSelectionPolicy::Priority,
        },
        priority_order: AccountPriorityOrder::HigherFirst,
    }
}

fn validated_limit(limit: Option<u32>) -> Result<u32, JSONRPCErrorError> {
    let limit = limit.unwrap_or(DEFAULT_PAGE_LIMIT);
    (1..=MAX_PAGE_LIMIT)
        .contains(&limit)
        .then_some(limit)
        .ok_or_else(|| invalid_params("limit must be between 1 and 100"))
}

fn parse_cursor(cursor: &str) -> Result<(u32, AccountAlias, AccountId), JSONRPCErrorError> {
    if cursor.len() > MAX_CURSOR_BYTES || cursor.chars().any(char::is_control) {
        return Err(invalid_params("cursor is invalid"));
    }
    let mut fields = cursor.split('|');
    let version = fields.next();
    let priority = fields.next().and_then(|value| value.parse::<u32>().ok());
    let alias = fields
        .next()
        .and_then(|value| AccountAlias::from_str(value).ok());
    let id = fields
        .next()
        .and_then(|value| AccountId::from_str(value).ok());
    if version != Some("v2") || fields.next().is_some() {
        return Err(invalid_params("cursor is invalid"));
    }
    priority
        .zip(alias)
        .zip(id)
        .map(|((priority, alias), id)| (priority, alias, id))
        .ok_or_else(|| invalid_params("cursor is invalid"))
}

pub(super) fn account_id(value: &str) -> Result<AccountId, JSONRPCErrorError> {
    AccountId::from_str(value).map_err(|_| invalid_params("account profile identifier is invalid"))
}

fn find_account<'a>(
    registry: &'a AccountRegistry,
    id: &str,
) -> Result<&'a AccountMetadata, JSONRPCErrorError> {
    let id = account_id(id)?;
    registry
        .accounts
        .iter()
        .find(|account| account.id == id)
        .ok_or_else(resource_not_found)
}

fn account_mut<'a>(
    registry: &'a mut AccountRegistry,
    id: &AccountId,
) -> Result<&'a mut AccountMetadata, JSONRPCErrorError> {
    registry
        .accounts
        .iter_mut()
        .find(|account| &account.id == id)
        .ok_or_else(resource_not_found)
}

fn is_authenticated(config: &Config, account: &AccountMetadata) -> Result<bool, JSONRPCErrorError> {
    ProfileAuthStorage::new(
        &config.codex_home,
        account.id.clone(),
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
    )
    .and_then(|storage| storage.load())
    .map(|auth| auth.is_some())
    .map_err(|_| internal_error("account profile credential backend is unavailable"))
}

fn authenticated_fallback(
    config: &Config,
    registry: &AccountRegistry,
    excluded: Option<&AccountId>,
) -> Result<Option<AccountId>, JSONRPCErrorError> {
    for account in registry.enabled_by_priority() {
        if excluded != Some(&account.id) && is_authenticated(config, account)? {
            return Ok(Some(account.id.clone()));
        }
    }
    Ok(None)
}

fn invalid_snapshot(snapshot: &RateLimitSnapshot) -> bool {
    snapshot.primary.as_ref().is_some_and(invalid_window)
        || snapshot.secondary.as_ref().is_some_and(invalid_window)
}

fn invalid_window(window: &RateLimitWindow) -> bool {
    !window.used_percent.is_finite() || !(0.0..=100.0).contains(&window.used_percent)
}

fn registry_error(error: RegistryStoreError) -> JSONRPCErrorError {
    match error {
        RegistryStoreError::GenerationConflict { .. } => generation_conflict(),
        RegistryStoreError::Validation(RegistryValidationError::DuplicateAlias { .. }) => {
            invalid_params("account alias is already in use")
        }
        RegistryStoreError::NotFound => resource_not_found(),
        RegistryStoreError::LockBusy => invalid_params("account registry is busy"),
        RegistryStoreError::CommittedDurabilityUncertain { .. } => {
            internal_error("account registry mutation committed with uncertain durability")
        }
        RegistryStoreError::Io { .. }
        | RegistryStoreError::Parse(_)
        | RegistryStoreError::Validation(_)
        | RegistryStoreError::AlreadyExists
        | RegistryStoreError::GenerationOverflow
        | RegistryStoreError::GuardMismatch
        | RegistryStoreError::UnsupportedSecurityPlatform => {
            internal_error("account registry is unavailable")
        }
    }
}

fn registry_validation_error(error: RegistryValidationError) -> JSONRPCErrorError {
    match error {
        RegistryValidationError::DuplicateAlias { .. } => {
            invalid_params("account alias is already in use")
        }
        RegistryValidationError::DuplicateServiceIdentity { .. } => {
            invalid_params("account service identity is already registered")
        }
        RegistryValidationError::DuplicateId { .. }
        | RegistryValidationError::MissingDefault { .. }
        | RegistryValidationError::UnsupportedVersion { .. } => {
            internal_error("account registry is corrupt")
        }
    }
}

pub(super) fn router_error(error: ProfileAuthRouterError) -> JSONRPCErrorError {
    match error {
        ProfileAuthRouterError::UnknownAccount => resource_not_found(),
        ProfileAuthRouterError::AmbiguousAccount => {
            invalid_params("account profile reference is ambiguous")
        }
        ProfileAuthRouterError::DisabledAccount => invalid_params("account profile is disabled"),
        ProfileAuthRouterError::NotAuthenticated => {
            invalid_params("account profile is not authenticated")
        }
        ProfileAuthRouterError::AccountInUse => invalid_params("account profile is in use"),
        ProfileAuthRouterError::Registry(RegistryStoreError::GenerationConflict { .. })
        | ProfileAuthRouterError::RegistryChanged => generation_conflict(),
        ProfileAuthRouterError::Selection(_) => {
            invalid_params("no eligible account profile is available")
        }
        ProfileAuthRouterError::Authentication(_) => {
            internal_error("account profile credential backend is unavailable")
        }
        ProfileAuthRouterError::EphemeralStorageUnsupported
        | ProfileAuthRouterError::PinConflict { .. }
        | ProfileAuthRouterError::ExternalAuthRequiresSingularPath { .. }
        | ProfileAuthRouterError::Registry(_)
        | ProfileAuthRouterError::Migration(_) => {
            internal_error("account profile routing is unavailable")
        }
    }
}

pub(super) fn resource_not_found() -> JSONRPCErrorError {
    invalid_params("account profile was not found")
}

fn generation_conflict() -> JSONRPCErrorError {
    invalid_params("account registry changed concurrently; retry the request")
}

pub(super) fn bedrock_region_valid(region: &str) -> bool {
    is_supported_amazon_bedrock_region(region)
}
