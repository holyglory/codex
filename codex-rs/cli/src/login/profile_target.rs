//! Keeps direct login and logout bound to the selected local account profile.
//! Interactive login stages credentials before replacing the active profile.

use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_core::config::Config;
use codex_login::AuthRouteConfig;
use codex_login::PendingProfileLogin;
use codex_login::ProfileAuthCommitError;
use codex_login::ProfileAuthStorage;
use codex_login::ServerOptions;
use codex_protocol::auth::AuthMode;

use super::LOGIN_SUCCESS_MESSAGE;
use super::clear_existing_auth_before_login;

#[derive(Debug)]
pub(super) enum CliAuthTarget {
    Legacy,
    Profile(ProfileAuthStorage),
}

#[derive(Debug)]
pub(super) enum CliLoginTarget {
    Legacy,
    Profile {
        active: ProfileAuthStorage,
        pending: Box<PendingProfileLogin>,
    },
}

fn resolve_auth_target(config: &Config) -> std::io::Result<CliAuthTarget> {
    let registry = match RegistryStore::new(&config.codex_home).read() {
        Ok(registry) => registry,
        Err(RegistryStoreError::NotFound) => return Ok(CliAuthTarget::Legacy),
        Err(_) => return Err(std::io::Error::other("account registry is unavailable")),
    };
    let account_id = registry
        .default_account_id
        .ok_or_else(|| std::io::Error::other("active account profile is unavailable"))?;
    let account = registry
        .accounts
        .iter()
        .find(|account| account.id == account_id)
        .ok_or_else(|| std::io::Error::other("active account profile is unavailable"))?;
    if !account.enabled {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "active account profile is disabled",
        ));
    }
    ProfileAuthStorage::new(
        &config.codex_home,
        account_id,
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
    )
    .map(CliAuthTarget::Profile)
}

pub(super) fn auth_target_or_exit(config: &Config) -> CliAuthTarget {
    match resolve_auth_target(config) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("Error resolving active account: {error}");
            std::process::exit(1);
        }
    }
}

pub(super) fn login_target_or_exit(config: &Config) -> CliLoginTarget {
    match auth_target_or_exit(config) {
        CliAuthTarget::Legacy => CliLoginTarget::Legacy,
        CliAuthTarget::Profile(active) => {
            let store = RegistryStore::new(&config.codex_home);
            let guard = match store.acquire_lock() {
                Ok(guard) => guard,
                Err(_) => {
                    eprintln!(
                        "Error preparing active account login: account registry is unavailable"
                    );
                    std::process::exit(1);
                }
            };
            let registry = match store.read() {
                Ok(registry) => registry,
                Err(_) => {
                    eprintln!(
                        "Error preparing active account login: account registry is unavailable"
                    );
                    std::process::exit(1);
                }
            };
            let alias = match registry
                .accounts
                .iter()
                .find(|account| &account.id == active.account_id())
            {
                Some(account) if account.enabled => account.alias.clone(),
                Some(_) => {
                    eprintln!("Error preparing active account login: active account is disabled");
                    std::process::exit(1);
                }
                None => {
                    eprintln!(
                        "Error preparing active account login: active account is unavailable"
                    );
                    std::process::exit(1);
                }
            };
            let pending = match PendingProfileLogin::begin(
                &config.codex_home,
                alias,
                config.cli_auth_credentials_store_mode,
                config.auth_keyring_backend_kind(),
            ) {
                Ok(pending) => pending,
                Err(_) => {
                    eprintln!(
                        "Error preparing active account login: pending profile storage is unavailable"
                    );
                    std::process::exit(1);
                }
            };
            drop(guard);
            CliLoginTarget::Profile {
                active,
                pending: Box::new(pending),
            }
        }
    }
}

pub(super) async fn clear_auth_target_before_login(
    target: &CliLoginTarget,
    config: &Config,
    auth_route_config: &AuthRouteConfig,
) {
    match target {
        CliLoginTarget::Legacy => {
            clear_existing_auth_before_login(
                &config.codex_home,
                config.cli_auth_credentials_store_mode,
                config.auth_keyring_backend_kind(),
                auth_route_config,
            )
            .await;
        }
        CliLoginTarget::Profile { .. } => {}
    }
}

pub(super) fn options_for_target(
    mut options: ServerOptions,
    target: &CliLoginTarget,
) -> ServerOptions {
    if let CliLoginTarget::Profile { pending, .. } = target {
        options = options.with_profile_auth_storage(pending.storage().clone());
    }
    options
}

pub(super) fn notify_profile_auth_changed(
    config: &Config,
    target: &CliAuthTarget,
) -> std::io::Result<()> {
    if !matches!(target, CliAuthTarget::Profile(_)) {
        return Ok(());
    }
    let store = RegistryStore::new(&config.codex_home);
    let guard = store.acquire_lock().map_err(std::io::Error::other)?;
    let registry = store.read().map_err(std::io::Error::other)?;
    match store.compare_and_swap_with_guard(&guard, registry.generation, |_| {}) {
        Ok(_) => Ok(()),
        Err(RegistryStoreError::CommittedDurabilityUncertain { .. }) => store
            .repair_committed_durability_with_guard(&guard)
            .map_err(std::io::Error::other),
        Err(error) => Err(std::io::Error::other(error)),
    }
}

pub(super) fn finish_login(target: CliLoginTarget) -> ! {
    if let CliLoginTarget::Profile { active, pending } = target {
        let first = pending.storage().load();
        let second = pending.storage().load();
        let auth = match (first, second) {
            (Ok(Some(first)), Ok(Some(second))) if first == second => first,
            _ => {
                let _ = pending.cleanup();
                eprintln!("Login failed while verifying the staged account credentials.");
                std::process::exit(1);
            }
        };
        match active.replace_auth_and_metadata(&auth) {
            Ok(_) => {
                if pending.cleanup().is_err() {
                    eprintln!("Login succeeded, but pending profile cleanup failed.");
                    std::process::exit(1);
                }
            }
            Err(ProfileAuthCommitError::CommittedDurabilityUncertain) => {
                eprintln!(
                    "Login committed, but account registry durability is uncertain; retry after storage recovers."
                );
                std::process::exit(1);
            }
            Err(error) => {
                let _ = pending.cleanup();
                eprintln!("Error committing active account login: {error}");
                std::process::exit(1);
            }
        }
    }
    eprintln!("{LOGIN_SUCCESS_MESSAGE}");
    std::process::exit(0);
}

pub(super) fn fail_login(target: CliLoginTarget, message: impl std::fmt::Display) -> ! {
    if let CliLoginTarget::Profile { pending, .. } = target
        && pending.cleanup().is_err()
    {
        eprintln!("Login failed and pending profile cleanup also failed.");
        std::process::exit(1);
    }
    eprintln!("{message}");
    std::process::exit(1);
}

pub(super) fn report_profile_login_status(auth: codex_login::AuthDotJson) -> ! {
    match auth.resolved_mode() {
        AuthMode::ApiKey => match auth.openai_api_key.as_deref() {
            Some(_api_key) => {
                eprintln!("Logged in using an API key");
                std::process::exit(0);
            }
            None => {
                eprintln!("Error checking login status: API key auth is incomplete");
                std::process::exit(1);
            }
        },
        AuthMode::Chatgpt => {
            eprintln!("Logged in using ChatGPT");
            std::process::exit(0);
        }
        AuthMode::ChatgptAuthTokens | AuthMode::Headers => {
            eprintln!("Error checking login status: external auth cannot be stored in a profile");
            std::process::exit(1);
        }
        AuthMode::AgentIdentity => {
            eprintln!("Logged in using access token");
            std::process::exit(0);
        }
        AuthMode::PersonalAccessToken => {
            eprintln!("Logged in using personal access token");
            std::process::exit(0);
        }
        AuthMode::BedrockApiKey => {
            eprintln!("Logged in using Amazon Bedrock API key");
            std::process::exit(0);
        }
        AuthMode::BedrockAccessKeys => {
            eprintln!("Logged in using Amazon Bedrock AWS access keys");
            std::process::exit(0);
        }
    }
}
