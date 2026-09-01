use std::future::Future;
use std::io;

use codex_account_registry::AccountMetadata;
use codex_account_registry::DEFAULT_ACCOUNT_PRIORITY;
use codex_account_registry::OpaqueServiceId;
use codex_account_registry::RegistryStore;
use codex_core::config::Config;
use codex_login::CLIENT_ID;
use codex_login::PendingProfileLogin;
use codex_login::PendingProfileLoginError;
use codex_login::ProfileAuthStorage;
use codex_login::ServerOptions;
use codex_login::run_device_code_login;
use codex_login::run_login_server;
use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::ForcedLoginMethod;
use serde::Serialize;

use super::AccountCommandError;
use super::AccountErrorKind;
use super::AddArgs;
use super::JSON_SCHEMA_VERSION;
use super::mutate_registry;
use super::read_or_empty;
use super::view::print_json;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddJson {
    schema_version: u32,
    generation: u64,
    account: AddedAccountJson,
    active_account: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddedAccountJson {
    id: String,
    alias: String,
    auth_mode: AuthMode,
    email: Option<String>,
    enabled: bool,
    priority: u32,
}

pub(super) async fn run(
    config: &Config,
    store: &RegistryStore,
    args: AddArgs,
    json: bool,
) -> Result<(), AccountCommandError> {
    if !config
        .auth_config()
        .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
    {
        return Err(AccountCommandError::new(AccountErrorKind::Configuration));
    }
    let auth_config = config.auth_config();
    let device_auth = args.device_auth;
    add_with_authorizer(config, store, args, json, move |profile| async move {
        let effective_workspaces = auth_config.effective_chatgpt_workspaces();
        let mut options = ServerOptions::new(
            auth_config.codex_home.clone(),
            CLIENT_ID.to_string(),
            effective_workspaces,
            auth_config.auth_credentials_store_mode,
            auth_config.keyring_backend_kind,
            auth_config.auth_route_config,
        )
        .with_profile_auth_storage(profile);
        if device_auth {
            run_device_code_login(options).await
        } else {
            options.open_browser = true;
            let server = run_login_server(options)?;
            eprintln!(
                "Starting local login server on http://localhost:{}.",
                server.actual_port
            );
            eprintln!(
                "If your browser did not open, navigate to this URL to authenticate:\n\n{}",
                server.auth_url
            );
            server.block_until_done().await
        }
    })
    .await
}

async fn add_with_authorizer<F, Fut>(
    config: &Config,
    store: &RegistryStore,
    args: AddArgs,
    json: bool,
    authorize: F,
) -> Result<(), AccountCommandError>
where
    F: FnOnce(ProfileAuthStorage) -> Fut,
    Fut: Future<Output = io::Result<()>>,
{
    let registry_guard = store
        .acquire_lock()
        .map_err(|_| AccountCommandError::new(AccountErrorKind::Registry))?;
    let registry = read_or_empty(store)?;
    if registry
        .accounts
        .iter()
        .any(|account| account.alias == args.alias)
    {
        return Err(AccountCommandError::new(AccountErrorKind::DuplicateAccount));
    }
    let pending = PendingProfileLogin::begin(
        &config.codex_home,
        args.alias,
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
    )
    .map_err(map_pending_error)?;
    drop(registry_guard);

    if let Err(error) = authorize(pending.storage().clone()).await {
        let kind = if error.kind() == io::ErrorKind::Interrupted {
            AccountErrorKind::LoginCancelled
        } else {
            AccountErrorKind::CredentialStore
        };
        pending
            .cleanup()
            .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?;
        return Err(AccountCommandError::new(kind));
    }

    let account = match verified_pending_account(&pending) {
        Ok(account) => account,
        Err(error) => {
            pending
                .cleanup()
                .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?;
            return Err(error);
        }
    };
    let account_id = account.id.clone();
    let account_alias = account.alias.clone();
    let mutation = mutate_registry(store, /*required_generation*/ None, |registry| {
        let mut planned = registry.clone();
        planned
            .add_account(account.clone())
            .map_err(|_| AccountCommandError::new(AccountErrorKind::DuplicateAccount))?;
        if planned.default_account_id.is_none() {
            planned.default_account_id = Some(account_id.clone());
        }
        Ok((planned, true, ()))
    });
    let (updated, _, ()) = match mutation {
        Ok(updated) => updated,
        Err(error) => {
            pending
                .cleanup()
                .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?;
            return Err(error);
        }
    };
    pending
        .finish()
        .map_err(|_| AccountCommandError::new(AccountErrorKind::Registry))?;
    let active_account = updated.default_account_id.as_ref().and_then(|id| {
        updated
            .accounts
            .iter()
            .find(|account| &account.id == id)
            .map(|account| account.alias.to_string())
    });
    if json {
        print_json(&AddJson {
            schema_version: JSON_SCHEMA_VERSION,
            generation: updated.generation,
            account: AddedAccountJson {
                id: account_id.to_string(),
                alias: account_alias.to_string(),
                auth_mode: account.auth_mode,
                email: account.email,
                enabled: true,
                priority: account.priority,
            },
            active_account,
        })
    } else {
        println!("Added account {account_alias}.");
        Ok(())
    }
}

fn verified_pending_account(
    pending: &PendingProfileLogin,
) -> Result<AccountMetadata, AccountCommandError> {
    let first = pending
        .storage()
        .load()
        .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?
        .ok_or_else(|| AccountCommandError::new(AccountErrorKind::NotAuthenticated))?;
    let second = pending
        .storage()
        .load()
        .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?
        .ok_or_else(|| AccountCommandError::new(AccountErrorKind::NotAuthenticated))?;
    if first != second
        || matches!(
            first.resolved_mode(),
            AuthMode::ChatgptAuthTokens | AuthMode::Headers
        )
    {
        return Err(AccountCommandError::new(AccountErrorKind::Integrity));
    }
    let identity = first.profile_metadata();
    let service_identity = match (identity.service_account_id, identity.service_workspace_id) {
        (Some(account), Some(workspace)) => Some((
            OpaqueServiceId::new(account)
                .map_err(|_| AccountCommandError::new(AccountErrorKind::Integrity))?,
            OpaqueServiceId::new(workspace)
                .map_err(|_| AccountCommandError::new(AccountErrorKind::Integrity))?,
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

fn map_pending_error(error: PendingProfileLoginError) -> AccountCommandError {
    match error {
        PendingProfileLoginError::Storage(_) | PendingProfileLoginError::Encoding(_) => {
            AccountCommandError::new(AccountErrorKind::CredentialStore)
        }
        PendingProfileLoginError::UnsupportedJournal
        | PendingProfileLoginError::ConfigurationDrift
        | PendingProfileLoginError::RecoveryConflict
        | PendingProfileLoginError::Registry(_) => {
            AccountCommandError::new(AccountErrorKind::Integrity)
        }
    }
}

#[cfg(test)]
#[path = "add_tests.rs"]
mod tests;
