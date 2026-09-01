use super::AccountProfileRequestProcessor;
use super::bedrock_region_valid;
use super::registry_validation_error;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use codex_account_registry::AccountAlias;
use codex_account_registry::AccountId;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::DEFAULT_ACCOUNT_PRIORITY;
use codex_account_registry::OpaqueServiceId;
use codex_app_server_protocol::AccountLoginCompletedNotification;
use codex_app_server_protocol::AccountProfileLogin;
use codex_app_server_protocol::AccountProfileLoginCancelParams;
use codex_app_server_protocol::AccountProfileLoginCancelResponse;
use codex_app_server_protocol::AccountProfileLoginMethod;
use codex_app_server_protocol::AccountProfileLoginStartParams;
use codex_app_server_protocol::AccountProfileLoginStartResponse;
use codex_app_server_protocol::CancelLoginAccountStatus;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::DesktopOnboardingEntrypoint;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::LoginAppBrand;
use codex_app_server_protocol::ServerNotification;
use codex_login::CLIENT_ID;
use codex_login::LoginOnboardingEntrypoint;
use codex_login::LoginSuccessPage;
use codex_login::LoginSuccessPageBrand;
use codex_login::PendingProfileLogin;
use codex_login::PendingProfileLoginError;
use codex_login::ServerOptions;
use codex_login::complete_device_code_login;
use codex_login::login_with_api_key_to_profile;
use codex_login::login_with_bedrock_api_key_to_profile;
use codex_login::request_device_code;
use codex_login::run_login_server;
use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::ForcedLoginMethod;
use std::str::FromStr;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
#[cfg(debug_assertions)]
const LOGIN_ISSUER_OVERRIDE_ENV_VAR: &str = "CODEX_APP_SERVER_LOGIN_ISSUER";
#[cfg(debug_assertions)]
const LOGIN_OPEN_APP_URL_OVERRIDE_ENV_VAR: &str = "CODEX_APP_SERVER_DEV_OPEN_APP_URL";

pub(super) struct ActiveProfileLogin {
    login_id: Uuid,
    cancel: Option<CancellationToken>,
}

impl AccountProfileRequestProcessor {
    pub(crate) async fn login_start(
        &self,
        params: AccountProfileLoginStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let pending = self.begin_pending(params.alias)?;
        let account_id = pending.account_id().to_string();
        let activate = params.activate;
        let login = match params.login {
            AccountProfileLoginMethod::ApiKey { api_key } => {
                if !self
                    .config
                    .auth_config()
                    .is_login_method_allowed(ForcedLoginMethod::Api)
                {
                    cleanup(pending)?;
                    return Err(invalid_params("API key profile login is disabled"));
                }
                if api_key.trim().is_empty() {
                    cleanup(pending)?;
                    return Err(invalid_params("API key must not be empty"));
                }
                if login_with_api_key_to_profile(pending.storage(), &api_key).is_err() {
                    cleanup(pending)?;
                    return Err(internal_error(
                        "account profile credentials could not be saved",
                    ));
                }
                let completed = self.finish_pending(pending, activate).await?;
                self.send_login_completion(/*login_id*/ None, Ok(completed))
                    .await;
                AccountProfileLogin::ApiKey {}
            }
            AccountProfileLoginMethod::AmazonBedrock { api_key, region } => {
                if !self
                    .config
                    .auth_config()
                    .is_login_method_allowed(ForcedLoginMethod::Api)
                {
                    cleanup(pending)?;
                    return Err(invalid_params("Amazon Bedrock profile login is disabled"));
                }
                let api_key = api_key.trim();
                let region = region.trim();
                if api_key.is_empty() || !bedrock_region_valid(region) {
                    cleanup(pending)?;
                    return Err(invalid_params("Amazon Bedrock credentials are invalid"));
                }
                if login_with_bedrock_api_key_to_profile(pending.storage(), api_key, region)
                    .is_err()
                {
                    cleanup(pending)?;
                    return Err(internal_error(
                        "account profile credentials could not be saved",
                    ));
                }
                let completed = self.finish_pending(pending, activate).await?;
                self.send_login_completion(/*login_id*/ None, Ok(completed))
                    .await;
                AccountProfileLogin::AmazonBedrock {}
            }
            AccountProfileLoginMethod::Chatgpt {
                codex_streamlined_login,
                use_hosted_login_success_page,
                app_brand,
            } => {
                if !self
                    .config
                    .auth_config()
                    .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
                {
                    cleanup(pending)?;
                    return Err(invalid_params("ChatGPT profile login is disabled"));
                }
                let success_page =
                    match login_success_page(use_hosted_login_success_page, app_brand) {
                        Ok(page) => page,
                        Err(error) => {
                            cleanup(pending)?;
                            return Err(error);
                        }
                    };
                let options = match self.login_options(
                    pending.storage().clone(),
                    codex_streamlined_login,
                    success_page,
                ) {
                    Ok(options) => options,
                    Err(error) => {
                        cleanup(pending)?;
                        return Err(error);
                    }
                };
                let login_id = Uuid::now_v7();
                let cancel = match self.register_login(login_id).await {
                    Ok(cancel) => cancel,
                    Err(error) => {
                        cleanup(pending)?;
                        return Err(error);
                    }
                };
                let server = match run_login_server(options) {
                    Ok(server) => server,
                    Err(_) => {
                        self.clear_registered_login(login_id).await;
                        cleanup(pending)?;
                        return Err(internal_error("account profile login could not be started"));
                    }
                };
                let auth_url = server.auth_url.clone();
                let shutdown = server.cancel_handle();
                let processor = self.clone();
                tokio::spawn(async move {
                    let result = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            shutdown.shutdown();
                            Err(LoginFailure::Cancelled)
                        }
                        result = tokio::time::timeout(
                            LOGIN_TIMEOUT,
                            server.block_until_done_with_callback_result(),
                        ) => match result {
                            Ok(Ok(callback)) => Ok(callback.onboarding_entrypoint),
                            Ok(Err(_)) => Err(LoginFailure::Failed),
                            Err(_) => {
                                shutdown.shutdown();
                                Err(LoginFailure::TimedOut)
                            }
                        }
                    };
                    processor
                        .complete_background_login(login_id, pending, activate, result)
                        .await;
                });
                AccountProfileLogin::Chatgpt {
                    login_id: login_id.to_string(),
                    auth_url,
                }
            }
            AccountProfileLoginMethod::ChatgptDeviceCode => {
                if !self
                    .config
                    .auth_config()
                    .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
                {
                    cleanup(pending)?;
                    return Err(invalid_params("ChatGPT profile login is disabled"));
                }
                let options = match self.login_options(
                    pending.storage().clone(),
                    /*codex_streamlined_login*/ false,
                    LoginSuccessPage::default(),
                ) {
                    Ok(options) => options,
                    Err(error) => {
                        cleanup(pending)?;
                        return Err(error);
                    }
                };
                let login_id = Uuid::now_v7();
                let cancel = match self.register_login(login_id).await {
                    Ok(cancel) => cancel,
                    Err(error) => {
                        cleanup(pending)?;
                        return Err(error);
                    }
                };
                let device_code = match request_device_code(&options).await {
                    Ok(device_code) => device_code,
                    Err(_) => {
                        self.clear_registered_login(login_id).await;
                        cleanup(pending)?;
                        return Err(internal_error("device authorization could not be started"));
                    }
                };
                let verification_url = device_code.verification_url.clone();
                let user_code = device_code.user_code.clone();
                let processor = self.clone();
                tokio::spawn(async move {
                    let result = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => Err(LoginFailure::Cancelled),
                        result = complete_device_code_login(options, device_code) => {
                            result.map(|()| None).map_err(|_| LoginFailure::Failed)
                        }
                    };
                    processor
                        .complete_background_login(login_id, pending, activate, result)
                        .await;
                });
                AccountProfileLogin::ChatgptDeviceCode {
                    login_id: login_id.to_string(),
                    verification_url,
                    user_code,
                }
            }
        };
        Ok(Some(
            AccountProfileLoginStartResponse { account_id, login }.into(),
        ))
    }

    pub(crate) async fn login_cancel(
        &self,
        params: AccountProfileLoginCancelParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let login_id = Uuid::parse_str(&params.login_id)
            .map_err(|_| invalid_params("login identifier is invalid"))?;
        let mut active = self.active_login.lock().await;
        let cancel = active
            .as_ref()
            .filter(|login| login.login_id == login_id)
            .and_then(|login| login.cancel.clone());
        let status = if let Some(cancel) = cancel {
            *active = None;
            cancel.cancel();
            CancelLoginAccountStatus::Canceled
        } else {
            CancelLoginAccountStatus::NotFound
        };
        Ok(Some(AccountProfileLoginCancelResponse { status }.into()))
    }

    pub(crate) async fn cancel_active_profile_login(&self) {
        if let Some(cancel) = self
            .active_login
            .lock()
            .await
            .take()
            .and_then(|active| active.cancel)
        {
            cancel.cancel();
        }
    }

    fn begin_pending(
        &self,
        requested_alias: Option<String>,
    ) -> Result<PendingProfileLogin, JSONRPCErrorError> {
        let guard = self.store.acquire_lock().map_err(super::registry_error)?;
        let registry = self.read_or_empty()?;
        let alias = pending_alias(&registry, requested_alias)?;
        if registry
            .accounts
            .iter()
            .any(|account| account.alias == alias)
        {
            return Err(invalid_params("account alias is already in use"));
        }
        let pending = PendingProfileLogin::begin(
            &self.config.codex_home,
            alias,
            self.config.cli_auth_credentials_store_mode,
            self.config.auth_keyring_backend_kind(),
        )
        .map_err(pending_error)?;
        drop(guard);
        Ok(pending)
    }

    async fn register_login(&self, login_id: Uuid) -> Result<CancellationToken, JSONRPCErrorError> {
        let mut active = self.active_login.lock().await;
        if active.is_some() {
            return Err(invalid_params(
                "another account profile login is already active",
            ));
        }
        let cancel = CancellationToken::new();
        *active = Some(ActiveProfileLogin {
            login_id,
            cancel: Some(cancel.clone()),
        });
        Ok(cancel)
    }

    async fn clear_registered_login(&self, login_id: Uuid) {
        let mut active = self.active_login.lock().await;
        if active
            .as_ref()
            .is_some_and(|active| active.login_id == login_id)
        {
            *active = None;
        }
    }

    async fn complete_background_login(
        &self,
        login_id: Uuid,
        pending: PendingProfileLogin,
        activate: bool,
        authorization: Result<Option<LoginOnboardingEntrypoint>, LoginFailure>,
    ) {
        let owns_completion = {
            let mut active = self.active_login.lock().await;
            active
                .as_mut()
                .filter(|active| active.login_id == login_id)
                .and_then(|active| active.cancel.take())
                .is_some()
        };
        let authorization = if owns_completion {
            authorization
        } else {
            Err(LoginFailure::Cancelled)
        };
        let result = match authorization {
            Ok(onboarding) => self
                .finish_pending(pending, activate)
                .await
                .map(|completed| {
                    (
                        completed,
                        onboarding.map(|LoginOnboardingEntrypoint::LifeSciences| {
                            DesktopOnboardingEntrypoint::LifeSciences
                        }),
                    )
                }),
            Err(failure) => {
                let failure = match failure {
                    LoginFailure::Cancelled => {
                        invalid_params("account profile login was cancelled")
                    }
                    LoginFailure::TimedOut => invalid_params("account profile login timed out"),
                    LoginFailure::Failed => invalid_params("account profile login failed"),
                };
                match cleanup(pending) {
                    Ok(()) => Err(failure),
                    Err(error) => Err(error),
                }
            }
        };
        match result {
            Ok((completed, onboarding)) => {
                self.send_login_completion_with_onboarding(
                    Some(login_id),
                    Ok(completed),
                    onboarding,
                )
                .await;
            }
            Err(error) => {
                self.outgoing
                    .send_server_notification(ServerNotification::AccountLoginCompleted(
                        AccountLoginCompletedNotification {
                            login_id: Some(login_id.to_string()),
                            success: false,
                            error: Some(error.message),
                            onboarding_entrypoint: None,
                        },
                    ))
                    .await;
            }
        }
        self.clear_registered_login(login_id).await;
    }

    async fn finish_pending(
        &self,
        pending: PendingProfileLogin,
        activate: bool,
    ) -> Result<CompletedProfileLogin, JSONRPCErrorError> {
        let account = match verified_pending_account(&pending) {
            Ok(account) => account,
            Err(error) => {
                cleanup(pending)?;
                return Err(error);
            }
        };
        let account_id = account.id.clone();
        let (updated, previous_default) = match self.mutate_registry(|current| {
            let previous_default = current.default_account_id.clone();
            let mut planned = current.clone();
            planned
                .add_account(account.clone())
                .map_err(registry_validation_error)?;
            if activate {
                planned.default_account_id = Some(account_id.clone());
            }
            Ok((planned, previous_default))
        }) {
            Ok(result) => result,
            Err(error) => {
                cleanup(pending)?;
                return Err(error);
            }
        };
        if pending.finish().is_err() {
            tracing::warn!("account profile login committed; pending journal cleanup is deferred");
        }
        Ok(CompletedProfileLogin {
            registry: updated,
            previous_default,
            activated: activate,
        })
    }

    async fn send_login_completion(
        &self,
        login_id: Option<Uuid>,
        completed: Result<CompletedProfileLogin, JSONRPCErrorError>,
    ) {
        self.send_login_completion_with_onboarding(
            login_id, completed, /*onboarding_entrypoint*/ None,
        )
        .await;
    }

    async fn send_login_completion_with_onboarding(
        &self,
        login_id: Option<Uuid>,
        completed: Result<CompletedProfileLogin, JSONRPCErrorError>,
        onboarding_entrypoint: Option<DesktopOnboardingEntrypoint>,
    ) {
        match completed {
            Ok(completed) => {
                self.outgoing
                    .send_server_notification(ServerNotification::AccountLoginCompleted(
                        AccountLoginCompletedNotification {
                            login_id: login_id.map(|id| id.to_string()),
                            success: true,
                            error: None,
                            onboarding_entrypoint,
                        },
                    ))
                    .await;
                if completed.activated {
                    let _ = self
                        .send_active_notifications(
                            completed.previous_default.as_ref(),
                            completed.registry.generation,
                            /*force_account*/ false,
                        )
                        .await;
                }
            }
            Err(error) => {
                self.outgoing
                    .send_server_notification(ServerNotification::AccountLoginCompleted(
                        AccountLoginCompletedNotification {
                            login_id: login_id.map(|id| id.to_string()),
                            success: false,
                            error: Some(error.message),
                            onboarding_entrypoint: None,
                        },
                    ))
                    .await;
            }
        }
    }

    fn login_options(
        &self,
        storage: codex_login::ProfileAuthStorage,
        codex_streamlined_login: bool,
        login_success_page: LoginSuccessPage,
    ) -> Result<ServerOptions, JSONRPCErrorError> {
        let auth_config = self.config.auth_config();
        let mut options = ServerOptions::new(
            auth_config.codex_home.clone(),
            CLIENT_ID.to_string(),
            auth_config.effective_chatgpt_workspaces(),
            auth_config.auth_credentials_store_mode,
            auth_config.keyring_backend_kind,
            auth_config.auth_route_config,
        )
        .with_profile_auth_storage(storage);
        options.open_browser = false;
        options.codex_streamlined_login = codex_streamlined_login;
        options.login_success_page = login_success_page;
        #[cfg(debug_assertions)]
        {
            if let Ok(issuer) = std::env::var(LOGIN_ISSUER_OVERRIDE_ENV_VAR)
                && !issuer.trim().is_empty()
            {
                options.issuer = issuer;
            }
        }
        Ok(options)
    }
}

struct CompletedProfileLogin {
    registry: AccountRegistry,
    previous_default: Option<AccountId>,
    activated: bool,
}

enum LoginFailure {
    Cancelled,
    TimedOut,
    Failed,
}

fn pending_alias(
    registry: &AccountRegistry,
    requested: Option<String>,
) -> Result<AccountAlias, JSONRPCErrorError> {
    if let Some(alias) = requested {
        return AccountAlias::from_str(&alias)
            .map_err(|_| invalid_params("account alias is invalid"));
    }
    let aliases = registry
        .accounts
        .iter()
        .map(|account| account.alias.as_str())
        .collect::<std::collections::HashSet<_>>();
    for index in 1..=10_000_u32 {
        let candidate = format!("account{index}");
        if !aliases.contains(candidate.as_str()) {
            return AccountAlias::from_str(&candidate)
                .map_err(|_| internal_error("account alias could not be derived"));
        }
    }
    Err(internal_error("account alias space is exhausted"))
}

fn verified_pending_account(
    pending: &PendingProfileLogin,
) -> Result<AccountMetadata, JSONRPCErrorError> {
    let first = pending
        .storage()
        .load()
        .map_err(|_| internal_error("account profile credential backend is unavailable"))?
        .ok_or_else(|| invalid_params("account profile is not authenticated"))?;
    let second = pending
        .storage()
        .load()
        .map_err(|_| internal_error("account profile credential backend is unavailable"))?
        .ok_or_else(|| invalid_params("account profile is not authenticated"))?;
    if first != second
        || matches!(
            first.resolved_mode(),
            AuthMode::ChatgptAuthTokens | AuthMode::Headers
        )
    {
        return Err(internal_error(
            "account profile credential verification failed",
        ));
    }
    let identity = first.profile_metadata();
    let service_identity = match (identity.service_account_id, identity.service_workspace_id) {
        (Some(account), Some(workspace)) => Some((
            OpaqueServiceId::new(account)
                .map_err(|_| internal_error("account profile metadata is invalid"))?,
            OpaqueServiceId::new(workspace)
                .map_err(|_| internal_error("account profile metadata is invalid"))?,
        )),
        _ => None,
    };
    let mut account = AccountMetadata {
        id: pending.account_id().clone(),
        alias: pending.alias().clone(),
        auth_mode: identity.auth_mode,
        email: identity.email,
        plan_type: identity.plan_type,
        enabled: true,
        priority: DEFAULT_ACCOUNT_PRIORITY,
        created_at: pending.started_at(),
        last_used_at: None,
        note: None,
        service_account_id: None,
        service_workspace_id: None,
    };
    if let Some((service_account_id, service_workspace_id)) = service_identity {
        account.service_account_id = Some(service_account_id);
        account.service_workspace_id = Some(service_workspace_id);
    }
    Ok(account)
}

fn cleanup(pending: PendingProfileLogin) -> Result<(), JSONRPCErrorError> {
    pending
        .cleanup()
        .map_err(|_| internal_error("pending account profile cleanup failed"))
}

fn pending_error(error: PendingProfileLoginError) -> JSONRPCErrorError {
    match error {
        PendingProfileLoginError::Storage(_) | PendingProfileLoginError::Encoding(_) => {
            internal_error("pending account profile storage is unavailable")
        }
        PendingProfileLoginError::UnsupportedJournal
        | PendingProfileLoginError::ConfigurationDrift
        | PendingProfileLoginError::RecoveryConflict
        | PendingProfileLoginError::Registry(_) => {
            internal_error("pending account profile recovery requires attention")
        }
    }
}

fn login_success_page(
    hosted: bool,
    app_brand: Option<LoginAppBrand>,
) -> Result<LoginSuccessPage, JSONRPCErrorError> {
    if !hosted {
        return Ok(LoginSuccessPage::default());
    }
    let brand = match app_brand.unwrap_or_default() {
        LoginAppBrand::Codex => LoginSuccessPageBrand::Codex,
        LoginAppBrand::Chatgpt => LoginSuccessPageBrand::Chatgpt,
    };
    let mut url = codex_login::CODEX_OPEN_APP_URL
        .parse()
        .map_err(|_| internal_error("login success URL is invalid"))?;
    #[cfg(debug_assertions)]
    if let Ok(override_url) = std::env::var(LOGIN_OPEN_APP_URL_OVERRIDE_ENV_VAR)
        && !override_url.trim().is_empty()
    {
        url = override_url
            .parse()
            .map_err(|_| internal_error("login success URL is invalid"))?;
    }
    Ok(LoginSuccessPage::Hosted {
        url,
        app_brand: brand,
    })
}
