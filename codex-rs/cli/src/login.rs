//! CLI login commands and their direct-user observability surfaces.
//!
//! The TUI path already installs a broader tracing stack with feedback, OpenTelemetry, and other
//! interactive-session layers. Direct `codex login` intentionally does less: it preserves the
//! existing stderr/browser UX and adds only a small file-backed tracing layer for login-specific
//! targets. Keeping that setup local avoids pulling the TUI's session-oriented logging machinery
//! into a one-shot CLI command while still producing a durable `codex-login.log` artifact that
//! support can request from users.

use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_config::types::AuthCredentialsStoreMode;
use codex_core::config::Config;
use codex_core::config::edit::ConfigEdit;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::AuthRouteConfig;
use codex_login::CLIENT_ID;
use codex_login::PendingProfileLogin;
use codex_login::ProfileAuthCommitError;
use codex_login::ProfileAuthStorage;
use codex_login::ServerOptions;
use codex_login::is_workload_identity_selected;
use codex_login::login_with_access_token;
use codex_login::login_with_access_token_to_profile;
use codex_login::login_with_api_key;
use codex_login::login_with_api_key_to_profile;
use codex_login::logout_profile_with_revoke;
use codex_login::logout_with_revoke;
use codex_login::run_device_code_login;
use codex_login::run_login_server;
use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_utils_cli::CliConfigOverrides;
use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::io::Read;
use std::path::Path;
use tracing_appender::non_blocking;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const CHATGPT_LOGIN_DISABLED_MESSAGE: &str =
    "ChatGPT login is disabled. Use API key login instead.";
const API_KEY_LOGIN_DISABLED_MESSAGE: &str =
    "API key login is disabled. Use ChatGPT login instead.";
const ACCESS_TOKEN_LOGIN_DISABLED_MESSAGE: &str =
    "Access token login is disabled. Use API key login instead.";
const LOGIN_SUCCESS_MESSAGE: &str = "Successfully logged in";

#[derive(Clone, Debug)]
enum CliAuthTarget {
    Legacy,
    Profile(ProfileAuthStorage),
}

#[derive(Debug)]
enum CliLoginTarget {
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

fn auth_target_or_exit(config: &Config) -> CliAuthTarget {
    match resolve_auth_target(config) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("Error resolving active account: {error}");
            std::process::exit(1);
        }
    }
}

fn login_target_or_exit(config: &Config) -> CliLoginTarget {
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

/// Installs a small file-backed tracing layer for direct `codex login` flows.
///
/// This deliberately duplicates a narrow slice of the TUI logging setup instead of reusing it
/// wholesale. The TUI stack includes session-oriented layers that are valuable for interactive
/// runs but unnecessary for a one-shot login command. Keeping the direct CLI path local lets this
/// command produce a durable `codex-login.log` artifact without coupling it to the TUI's broader
/// telemetry and feedback initialization.
fn init_login_file_logging(config: &Config) -> Option<WorkerGuard> {
    let log_dir = match codex_core::config::log_dir(config) {
        Ok(log_dir) => log_dir,
        Err(err) => {
            eprintln!("Warning: failed to resolve login log directory: {err}");
            return None;
        }
    };

    if let Err(err) = std::fs::create_dir_all(&log_dir) {
        eprintln!("Warning: failed to create the login log directory: {err}");
        return None;
    }

    let mut log_file_opts = OpenOptions::new();
    log_file_opts.create(true).append(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log_file_opts.mode(0o600);
    }

    let log_path = log_dir.join("codex-login.log");
    let log_file = match log_file_opts.open(&log_path) {
        Ok(log_file) => log_file,
        Err(err) => {
            eprintln!("Warning: failed to open the login log file: {err}");
            return None;
        }
    };

    let (non_blocking, guard) = non_blocking(log_file);
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("codex_cli=info,codex_core=info,codex_login=info"));
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_target(true)
        .with_ansi(false)
        .with_filter(env_filter);

    // Direct `codex login` otherwise relies on ephemeral stderr and browser output.
    // Persist the same login targets to a file so support can inspect auth failures
    // without reproducing them through TUI or app-server.
    if let Err(err) = tracing_subscriber::registry().with(file_layer).try_init() {
        eprintln!("Warning: failed to initialize login file logging: {err}");
        return None;
    }

    Some(guard)
}

fn print_login_server_start(actual_port: u16, auth_url: &str) {
    eprintln!(
        "Starting local login server on http://localhost:{actual_port}.\nIf your browser did not open, navigate to this URL to authenticate:\n\n{auth_url}\n\nOn a remote or headless machine? Use `codex login --device-auth` instead."
    );
}

async fn clear_existing_auth_before_login(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    auth_keyring_backend_kind: AuthKeyringBackendKind,
    auth_route_config: &AuthRouteConfig,
) {
    if let Err(err) = logout_with_revoke(
        codex_home,
        auth_credentials_store_mode,
        auth_keyring_backend_kind,
        auth_route_config,
    )
    .await
    {
        tracing::warn!("failed to clear existing auth before login: {err}");
    }
}

async fn clear_auth_target_before_login(
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

fn options_for_target(mut options: ServerOptions, target: &CliLoginTarget) -> ServerOptions {
    if let CliLoginTarget::Profile { pending, .. } = target {
        options = options.with_profile_auth_storage(pending.storage().clone());
    }
    options
}

fn notify_profile_auth_changed(config: &Config, target: &CliAuthTarget) -> std::io::Result<()> {
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

fn finish_login(target: CliLoginTarget) -> ! {
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

fn fail_login(target: CliLoginTarget, message: impl std::fmt::Display) -> ! {
    if let CliLoginTarget::Profile { pending, .. } = target
        && pending.cleanup().is_err()
    {
        eprintln!("Login failed and pending profile cleanup also failed.");
        std::process::exit(1);
    }
    eprintln!("{message}");
    std::process::exit(1);
}

pub async fn run_login_with_chatgpt(cli_config_overrides: CliConfigOverrides) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    let _login_log_guard = init_login_file_logging(&config);
    tracing::info!("starting browser login flow");

    if !config
        .auth_config()
        .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
    {
        eprintln!("{CHATGPT_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }

    let target = login_target_or_exit(&config);
    let auth_route_config = config.auth_route_config();
    clear_auth_target_before_login(&target, &config, &auth_route_config).await;
    let options = options_for_target(
        ServerOptions::new(
            config.codex_home.to_path_buf(),
            CLIENT_ID.to_string(),
            config.auth_config().effective_chatgpt_workspaces(),
            config.cli_auth_credentials_store_mode,
            config.auth_keyring_backend_kind(),
            auth_route_config,
        ),
        &target,
    );
    let result = run_login_server(options).inspect(|server| {
        print_login_server_start(server.actual_port, &server.auth_url);
    });
    let result = match result {
        Ok(server) => server.block_until_done().await,
        Err(error) => Err(error),
    };
    match result {
        Ok(_) => finish_login(target),
        Err(error) => fail_login(target, format_args!("Error logging in: {error}")),
    }
}

pub async fn run_login_with_api_key(
    cli_config_overrides: CliConfigOverrides,
    api_key: String,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    let _login_log_guard = init_login_file_logging(&config);
    tracing::info!("starting api key login flow");

    if !config
        .auth_config()
        .is_login_method_allowed(ForcedLoginMethod::Api)
    {
        eprintln!("{API_KEY_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }

    let target = login_target_or_exit(&config);
    let result = match &target {
        CliLoginTarget::Legacy => login_with_api_key(
            &config.codex_home,
            &api_key,
            config.cli_auth_credentials_store_mode,
            config.auth_keyring_backend_kind(),
        ),
        CliLoginTarget::Profile { pending, .. } => {
            login_with_api_key_to_profile(pending.storage(), &api_key)
        }
    };
    match result {
        Ok(_) => finish_login(target),
        Err(error) => fail_login(target, format_args!("Error logging in: {error}")),
    }
}

pub async fn run_login_with_access_token(
    cli_config_overrides: CliConfigOverrides,
    access_token: String,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    let _login_log_guard = init_login_file_logging(&config);
    tracing::info!("starting access token login flow");

    if !config
        .auth_config()
        .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
    {
        eprintln!("{ACCESS_TOKEN_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }

    let auth_route_config = config.auth_route_config();
    let effective_chatgpt_workspaces = config.auth_config().effective_chatgpt_workspaces();
    let target = login_target_or_exit(&config);
    let result = match &target {
        CliLoginTarget::Legacy => {
            login_with_access_token(
                &config.codex_home,
                &access_token,
                config.cli_auth_credentials_store_mode,
                effective_chatgpt_workspaces.as_deref(),
                Some(&config.chatgpt_base_url),
                config.auth_keyring_backend_kind(),
                &auth_route_config,
            )
            .await
        }
        CliLoginTarget::Profile { pending, .. } => {
            login_with_access_token_to_profile(
                pending.storage(),
                &access_token,
                effective_chatgpt_workspaces.as_deref(),
                Some(&config.chatgpt_base_url),
                &auth_route_config,
            )
            .await
        }
    };
    match result {
        Ok(_) => finish_login(target),
        Err(error) => fail_login(
            target,
            format_args!("Error logging in with access token: {error}"),
        ),
    }
}

pub fn read_api_key_from_stdin() -> String {
    read_stdin_secret(
        "--with-api-key expects the API key on stdin. Try piping it, e.g. `printenv OPENAI_API_KEY | codex login --with-api-key`.",
        "Reading API key from stdin...",
        "No API key provided via stdin.",
    )
}

pub fn read_access_token_from_stdin() -> String {
    read_stdin_secret(
        "--with-access-token expects the access token on stdin. Try piping it, e.g. `printenv CODEX_ACCESS_TOKEN | codex login --with-access-token`.",
        "Reading access token from stdin...",
        "No access token provided via stdin.",
    )
}

fn read_stdin_secret(terminal_message: &str, reading_message: &str, empty_message: &str) -> String {
    let mut stdin = std::io::stdin();

    if stdin.is_terminal() {
        eprintln!("{terminal_message}");
        std::process::exit(1);
    }

    eprintln!("{reading_message}");

    let mut buffer = String::new();
    if let Err(err) = stdin.read_to_string(&mut buffer) {
        eprintln!("Failed to read stdin: {err}");
        std::process::exit(1);
    }

    let secret = buffer.trim().to_string();
    if secret.is_empty() {
        eprintln!("{empty_message}");
        std::process::exit(1);
    }

    secret
}

/// Login using the OAuth device code flow.
pub async fn run_login_with_device_code(
    cli_config_overrides: CliConfigOverrides,
    issuer_base_url: Option<String>,
    client_id: Option<String>,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    let _login_log_guard = init_login_file_logging(&config);
    tracing::info!("starting device code login flow");
    if !config
        .auth_config()
        .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
    {
        eprintln!("{CHATGPT_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }
    let auth_route_config = config.auth_route_config();
    let target = login_target_or_exit(&config);
    clear_auth_target_before_login(&target, &config, &auth_route_config).await;
    let effective_chatgpt_workspaces = config.auth_config().effective_chatgpt_workspaces();
    let mut opts = options_for_target(
        ServerOptions::new(
            config.codex_home.to_path_buf(),
            client_id.unwrap_or(CLIENT_ID.to_string()),
            effective_chatgpt_workspaces,
            config.cli_auth_credentials_store_mode,
            config.auth_keyring_backend_kind(),
            auth_route_config,
        ),
        &target,
    );
    if let Some(iss) = issuer_base_url {
        opts.issuer = iss;
    }
    match run_device_code_login(opts).await {
        Ok(()) => finish_login(target),
        Err(error) => fail_login(
            target,
            format_args!("Error logging in with device code: {error}"),
        ),
    }
}

/// Prefers device-code login (with `open_browser = false`) when headless environment is detected, but keeps
/// `codex login` working in environments where device-code may be disabled/feature-gated.
/// If `run_device_code_login` returns `ErrorKind::NotFound` ("device-code unsupported"), this
/// falls back to starting the local browser login server.
pub async fn run_login_with_device_code_fallback_to_browser(
    cli_config_overrides: CliConfigOverrides,
    issuer_base_url: Option<String>,
    client_id: Option<String>,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    let _login_log_guard = init_login_file_logging(&config);
    tracing::info!("starting login flow with device code fallback");
    if !config
        .auth_config()
        .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
    {
        eprintln!("{CHATGPT_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }
    let auth_route_config = config.auth_route_config();
    let target = login_target_or_exit(&config);
    clear_auth_target_before_login(&target, &config, &auth_route_config).await;

    let effective_chatgpt_workspaces = config.auth_config().effective_chatgpt_workspaces();
    let mut opts = options_for_target(
        ServerOptions::new(
            config.codex_home.to_path_buf(),
            client_id.unwrap_or(CLIENT_ID.to_string()),
            effective_chatgpt_workspaces,
            config.cli_auth_credentials_store_mode,
            config.auth_keyring_backend_kind(),
            auth_route_config,
        ),
        &target,
    );
    if let Some(iss) = issuer_base_url {
        opts.issuer = iss;
    }
    opts.open_browser = false;

    match run_device_code_login(opts.clone()).await {
        Ok(()) => finish_login(target),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                eprintln!("Device code login is not enabled; falling back to browser login.");
                match run_login_server(opts) {
                    Ok(server) => {
                        print_login_server_start(server.actual_port, &server.auth_url);
                        match server.block_until_done().await {
                            Ok(()) => finish_login(target),
                            Err(error) => {
                                fail_login(target, format_args!("Error logging in: {error}"))
                            }
                        }
                    }
                    Err(error) => fail_login(target, format_args!("Error logging in: {error}")),
                }
            } else {
                fail_login(
                    target,
                    format_args!("Error logging in with device code: {e}"),
                )
            }
        }
    }
}

pub async fn run_login_status(cli_config_overrides: CliConfigOverrides) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;

    if is_workload_identity_selected() {
        match AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ false).await {
            Ok(_) => {
                eprintln!("Logged in using workload identity");
                std::process::exit(0);
            }
            Err(err) => {
                eprintln!("Error checking login status: {err}");
                std::process::exit(1);
            }
        }
    }

    if let CliAuthTarget::Profile(profile) = auth_target_or_exit(&config) {
        match profile.load() {
            Ok(Some(auth)) => report_profile_login_status(auth),
            Ok(None) => {
                eprintln!("Not logged in");
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("Error checking login status: {error}");
                std::process::exit(1);
            }
        }
    }

    let auth_config = config.auth_config();
    match auth_config
        .load_auth(/*enable_codex_api_key_env*/ false)
        .await
    {
        Ok(Some(auth)) => match auth.auth_mode() {
            AuthMode::ApiKey => match auth.get_token() {
                Ok(_api_key) => {
                    eprintln!("Logged in using an API key");
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Unexpected error retrieving API key: {e}");
                    std::process::exit(1);
                }
            },
            AuthMode::Chatgpt | AuthMode::ChatgptAuthTokens => {
                eprintln!("Logged in using ChatGPT");
                std::process::exit(0);
            }
            AuthMode::Headers => {
                unreachable!("header auth cannot be loaded from auth storage")
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
        },
        Ok(None) => {
            eprintln!("Not logged in");
            std::process::exit(1);
        }
        Err(err) => {
            eprintln!("Error checking login status: {err}");
            std::process::exit(1);
        }
    }
}

fn report_profile_login_status(auth: codex_login::AuthDotJson) -> ! {
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

pub async fn run_logout(cli_config_overrides: CliConfigOverrides) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    let auth_route_config = config.auth_route_config();

    let target = auth_target_or_exit(&config);
    let logged_out = match match &target {
        CliAuthTarget::Legacy => {
            logout_with_revoke(
                &config.codex_home,
                config.cli_auth_credentials_store_mode,
                config.auth_keyring_backend_kind(),
                &auth_route_config,
            )
            .await
        }
        CliAuthTarget::Profile(profile) => {
            logout_profile_with_revoke(profile, &auth_route_config).await
        }
    } {
        Ok(logged_out) => logged_out,
        Err(err) => {
            eprintln!("Error logging out: {err}");
            std::process::exit(1);
        }
    };

    if logged_out && notify_profile_auth_changed(&config, &target).is_err() {
        eprintln!("Logout succeeded, but the account registry notification failed.");
        std::process::exit(1);
    }

    let cleared_bedrock_config =
        if let Some(paths) = ConfigEditsBuilder::bedrock_provider_config_paths_to_clear(&config) {
            let edits = paths
                .into_iter()
                .map(|segments| ConfigEdit::ClearPath { segments });
            if let Err(err) = ConfigEditsBuilder::for_config(&config)
                .with_edits(edits)
                .apply()
                .await
            {
                eprintln!("Error clearing Amazon Bedrock configuration after logout: {err}");
                std::process::exit(1);
            }
            true
        } else {
            false
        };

    if logged_out || cleared_bedrock_config {
        eprintln!("Successfully logged out");
    } else {
        eprintln!("Not logged in");
    }
    std::process::exit(0);
}

async fn load_config_or_exit(cli_config_overrides: CliConfigOverrides) -> Config {
    let cli_overrides = match cli_config_overrides.parse_overrides() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing -c overrides: {e}");
            std::process::exit(1);
        }
    };

    match Config::load_with_cli_overrides(cli_overrides).await {
        Ok(config) => match config.auth_config().validate() {
            Ok(()) => config,
            Err(e) => {
                eprintln!("Error loading configuration: {e}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("Error loading configuration: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use codex_config::types::AuthCredentialsStoreMode;
    use codex_login::AuthKeyringBackendKind;
    use codex_login::load_auth_dot_json;
    use codex_login::login_with_api_key;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::clear_existing_auth_before_login;

    #[tokio::test]
    async fn clears_existing_auth_before_login() {
        let codex_home = tempdir().expect("create temporary Codex home");
        login_with_api_key(
            codex_home.path(),
            "sk-existing",
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )
        .expect("save existing auth");

        clear_existing_auth_before_login(
            codex_home.path(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
            &codex_login::test_support::transport_default_auth_route_config(),
        )
        .await;

        let auth = load_auth_dot_json(
            codex_home.path(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )
        .expect("load auth after cleanup");
        assert_eq!(auth, None);
    }
}
