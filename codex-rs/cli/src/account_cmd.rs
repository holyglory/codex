use std::io::IsTerminal;
use std::io::Write;

use chrono::Utc;
use clap::Args;
use clap::Parser;
use codex_account_registry::AccountAlias;
use codex_account_registry::AccountId;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_account_registry::RegistryValidationError;
use codex_config::types::AuthCredentialsStoreMode;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_login::ProfileAuthRouter;
use codex_login::ProfileAuthRouterError;
use codex_login::ProfileAuthStorage;
use codex_login::migrate_legacy_auth_if_needed;
use codex_utils_cli::CliConfigOverrides;
use serde::Serialize;

use self::error::AccountCommandError;
use self::error::AccountErrorKind;

mod add;
mod doctor;
mod error;
mod limits;
mod view;

pub(crate) use error::print_error;

const JSON_SCHEMA_VERSION: u32 = 1;
const MAX_MUTATION_ATTEMPTS: usize = 3;
const MAX_NOTE_BYTES: usize = 1_024;

#[derive(Debug, Parser)]
pub(crate) struct AccountCommand {
    #[clap(skip)]
    pub(crate) config_overrides: CliConfigOverrides,

    /// Emit stable, versioned JSON.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    #[command(subcommand)]
    action: AccountAction,
}

#[derive(Debug, clap::Subcommand)]
enum AccountAction {
    /// List configured account profiles.
    List,
    /// Show the current default account profile.
    Current,
    /// Show one account profile.
    Show(AccountReference),
    /// Authorize and add a new account profile.
    Add(AddArgs),
    /// Read service-reported rate-limit buckets.
    Limits(LimitsArgs),
    /// Rename an account profile.
    Rename(RenameArgs),
    /// Edit account metadata.
    Edit(EditArgs),
    /// List or change priorities; higher numbers drain first and smaller numbers drain last.
    Priority(PriorityArgs),
    /// Make an account the global default.
    Use(MutationReference),
    /// Enable an account profile.
    Enable(MutationReference),
    /// Disable an account profile.
    Disable(MutationReference),
    /// Remove account metadata and credentials.
    Remove(RemoveArgs),
    /// Inspect or change higher-first automatic account selection.
    Auto(AutoArgs),
    /// Check registry, credential backend, and private-file integrity.
    Doctor,
}

#[derive(Clone, Debug, Args)]
struct AddArgs {
    #[arg(value_name = "ALIAS")]
    alias: AccountAlias,

    /// Use OAuth device authorization instead of a browser callback.
    #[arg(long = "device-auth")]
    device_auth: bool,
}

#[derive(Clone, Debug, Args)]
struct LimitsArgs {
    #[arg(value_name = "ACCOUNT", conflicts_with = "all")]
    account: Option<String>,

    /// Include every profile, preserving unavailable accounts as unknown.
    #[arg(long)]
    all: bool,
}

#[derive(Debug, Args)]
struct AccountReference {
    #[arg(value_name = "ACCOUNT")]
    account: String,
}

#[derive(Debug, Args)]
struct MutationReference {
    #[arg(value_name = "ACCOUNT")]
    account: String,

    /// Require this registry generation instead of retrying a concurrent update.
    #[arg(long, value_name = "GENERATION")]
    expected_generation: Option<u64>,
}

#[derive(Debug, Args)]
struct RenameArgs {
    #[arg(value_name = "ACCOUNT")]
    account: String,

    #[arg(value_name = "NEW_ALIAS")]
    new_alias: AccountAlias,

    /// Require this registry generation instead of retrying a concurrent update.
    #[arg(long, value_name = "GENERATION")]
    expected_generation: Option<u64>,
}

#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("change")
        .required(true)
        .multiple(true)
        .args(["priority", "note", "clear_note"])
))]
struct EditArgs {
    #[arg(value_name = "ACCOUNT")]
    account: String,

    #[arg(long, value_name = "N")]
    priority: Option<u32>,

    #[arg(long, value_name = "TEXT", conflicts_with = "clear_note")]
    note: Option<String>,

    #[arg(long, conflicts_with = "note")]
    clear_note: bool,

    /// Require this registry generation instead of retrying a concurrent update.
    #[arg(long, value_name = "GENERATION")]
    expected_generation: Option<u64>,
}

#[derive(Debug, Args)]
struct PriorityArgs {
    #[command(subcommand)]
    action: Option<PriorityAction>,
}

#[derive(Debug, clap::Subcommand)]
enum PriorityAction {
    /// List priorities from highest (drained first) to lowest (drained last).
    List,
    /// Set one account's priority.
    Set(PrioritySetArgs),
    /// Atomically set every configured account to one priority.
    SetAll(PrioritySetAllArgs),
}

#[derive(Debug, Args)]
struct PrioritySetArgs {
    #[arg(value_name = "ACCOUNT")]
    account: String,

    #[arg(value_name = "N")]
    priority: u32,

    /// Require this registry generation instead of retrying a concurrent update.
    #[arg(long, value_name = "GENERATION")]
    expected_generation: Option<u64>,
}

#[derive(Debug, Args)]
struct PrioritySetAllArgs {
    #[arg(value_name = "N")]
    priority: u32,

    /// Require this registry generation instead of retrying a concurrent update.
    #[arg(long, value_name = "GENERATION")]
    expected_generation: Option<u64>,
}

#[derive(Debug, Args)]
struct RemoveArgs {
    #[arg(value_name = "ACCOUNT")]
    account: String,

    /// Confirm permanent credential deletion without an interactive prompt.
    #[arg(long)]
    yes: bool,

    /// Require this registry generation instead of retrying a concurrent update.
    #[arg(long, value_name = "GENERATION")]
    expected_generation: Option<u64>,
}

#[derive(Debug, Args)]
struct AutoArgs {
    #[command(subcommand)]
    action: Option<AutoAction>,
}

#[derive(Clone, Copy, Debug, clap::Subcommand)]
enum AutoAction {
    /// Show automatic-selection state.
    Status,
    /// Enable automatic selection.
    On(ExpectedGenerationArgs),
    /// Disable automatic selection.
    Off(ExpectedGenerationArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccountEnabledState {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, Args)]
struct ExpectedGenerationArgs {
    /// Require this registry generation instead of retrying a concurrent update.
    #[arg(long, value_name = "GENERATION")]
    expected_generation: Option<u64>,
}

pub(crate) async fn run(
    command: AccountCommand,
    strict_config: bool,
) -> Result<(), AccountCommandError> {
    let overrides = command
        .config_overrides
        .parse_overrides()
        .map_err(|_| AccountCommandError::new(AccountErrorKind::Configuration))?;
    let config = ConfigBuilder::default()
        .cli_overrides(overrides)
        .strict_config(strict_config)
        .build()
        .await
        .map_err(|_| AccountCommandError::new(AccountErrorKind::Configuration))?;
    config
        .auth_config()
        .validate()
        .map_err(|_| AccountCommandError::new(AccountErrorKind::Configuration))?;
    if config.cli_auth_credentials_store_mode == AuthCredentialsStoreMode::Ephemeral {
        return Err(AccountCommandError::new(AccountErrorKind::CredentialStore));
    }
    migrate_legacy_auth_if_needed(
        &config.codex_home,
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
    )
    .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?;
    execute(&config, command.action, command.json).await
}

async fn execute(
    config: &Config,
    action: AccountAction,
    json: bool,
) -> Result<(), AccountCommandError> {
    let store = RegistryStore::new(&config.codex_home);
    match action {
        AccountAction::List => view::list(config, &store, json),
        AccountAction::Current => view::current(config, &store, json),
        AccountAction::Show(reference) => view::show(config, &store, &reference.account, json),
        AccountAction::Add(args) => add::run(config, &store, args, json).await,
        AccountAction::Limits(args) => limits::run(config, &store, args, json).await,
        AccountAction::Rename(args) => rename(config, &store, args, json),
        AccountAction::Edit(args) => edit(config, &store, args, json),
        AccountAction::Priority(args) => priority(config, &store, args, json),
        AccountAction::Use(reference) => use_account(config, &store, reference, json),
        AccountAction::Enable(reference) => set_enabled(
            config,
            &store,
            reference,
            AccountEnabledState::Enabled,
            json,
        ),
        AccountAction::Disable(reference) => set_enabled(
            config,
            &store,
            reference,
            AccountEnabledState::Disabled,
            json,
        ),
        AccountAction::Remove(args) => remove(config, &store, args, json),
        AccountAction::Auto(args) => auto(&store, args, json),
        AccountAction::Doctor => doctor::run(config, &store, json),
    }
}

fn priority(
    config: &Config,
    store: &RegistryStore,
    args: PriorityArgs,
    json: bool,
) -> Result<(), AccountCommandError> {
    match args.action.unwrap_or(PriorityAction::List) {
        PriorityAction::List => view::list(config, store, json),
        PriorityAction::Set(args) => set_priority(config, store, args, json),
        PriorityAction::SetAll(args) => set_all_priorities(config, store, args, json),
    }
}

fn set_priority(
    config: &Config,
    store: &RegistryStore,
    args: PrioritySetArgs,
    json: bool,
) -> Result<(), AccountCommandError> {
    let (registry, changed, id) = mutate_registry(store, args.expected_generation, |registry| {
        let id = resolve_account(registry, &args.account)?.id.clone();
        let mut planned = registry.clone();
        let account = account_mut(&mut planned, &id)?;
        let changed = account.priority != args.priority;
        account.priority = args.priority;
        Ok((planned, changed, id))
    })?;
    view::mutation(config, &registry, &id, "setPriority", changed, json)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetAllPrioritiesJson {
    schema_version: u32,
    generation: u64,
    action: &'static str,
    changed: bool,
    changed_count: usize,
    priority: u32,
    accounts: Vec<AccountAlias>,
}

fn set_all_priorities(
    _config: &Config,
    store: &RegistryStore,
    args: PrioritySetAllArgs,
    json: bool,
) -> Result<(), AccountCommandError> {
    let (registry, changed, changed_count) =
        mutate_registry(store, args.expected_generation, |registry| {
            let mut planned = registry.clone();
            let mut changed_count = 0;
            for account in &mut planned.accounts {
                if account.priority != args.priority {
                    account.priority = args.priority;
                    changed_count += 1;
                }
            }
            Ok((planned, changed_count != 0, changed_count))
        })?;
    let mut accounts = registry
        .accounts
        .iter()
        .map(|account| account.alias.clone())
        .collect::<Vec<_>>();
    accounts.sort();
    if json {
        view::print_json(&SetAllPrioritiesJson {
            schema_version: JSON_SCHEMA_VERSION,
            generation: registry.generation,
            action: "setAllPriorities",
            changed,
            changed_count,
            priority: args.priority,
            accounts,
        })
    } else {
        println!(
            "Set priority {} for {} account profile(s); {} changed. Higher numbers drain first; smaller numbers drain last.",
            args.priority,
            accounts.len(),
            changed_count
        );
        Ok(())
    }
}

fn rename(
    config: &Config,
    store: &RegistryStore,
    args: RenameArgs,
    json: bool,
) -> Result<(), AccountCommandError> {
    let new_alias = args.new_alias;
    let (registry, changed, id) = mutate_registry(store, args.expected_generation, |registry| {
        let id = resolve_account(registry, &args.account)?.id.clone();
        let mut planned = registry.clone();
        let account = account_mut(&mut planned, &id)?;
        let changed = account.alias != new_alias;
        account.alias = new_alias.clone();
        validate_registry(&planned)?;
        Ok((planned, changed, id))
    })?;
    view::mutation(config, &registry, &id, "rename", changed, json)
}

fn edit(
    config: &Config,
    store: &RegistryStore,
    args: EditArgs,
    json: bool,
) -> Result<(), AccountCommandError> {
    if args
        .note
        .as_ref()
        .is_some_and(|note| note.len() > MAX_NOTE_BYTES || note.chars().any(char::is_control))
    {
        return Err(AccountCommandError::new(AccountErrorKind::InvalidInput));
    }
    let (registry, changed, id) = mutate_registry(store, args.expected_generation, |registry| {
        let id = resolve_account(registry, &args.account)?.id.clone();
        let mut planned = registry.clone();
        let account = account_mut(&mut planned, &id)?;
        let before = (account.priority, account.note.clone());
        if let Some(priority) = args.priority {
            account.priority = priority;
        }
        if let Some(note) = &args.note {
            account.note = Some(note.clone());
        } else if args.clear_note {
            account.note = None;
        }
        let changed = before != (account.priority, account.note.clone());
        Ok((planned, changed, id))
    })?;
    view::mutation(config, &registry, &id, "edit", changed, json)
}

fn use_account(
    config: &Config,
    store: &RegistryStore,
    args: MutationReference,
    json: bool,
) -> Result<(), AccountCommandError> {
    let (registry, changed, id) = mutate_registry(store, args.expected_generation, |registry| {
        let account = resolve_account(registry, &args.account)?;
        require_enabled(account)?;
        require_authenticated(config, account)?;
        let id = account.id.clone();
        let mut planned = registry.clone();
        let changed = planned.default_account_id.as_ref() != Some(&id);
        planned.default_account_id = Some(id.clone());
        account_mut(&mut planned, &id)?.last_used_at = Some(Utc::now());
        Ok((planned, changed, id))
    })?;
    view::mutation(config, &registry, &id, "use", changed, json)
}

fn set_enabled(
    config: &Config,
    store: &RegistryStore,
    args: MutationReference,
    state: AccountEnabledState,
    json: bool,
) -> Result<(), AccountCommandError> {
    let enabled = state == AccountEnabledState::Enabled;
    let (registry, changed, id) = mutate_registry(store, args.expected_generation, |registry| {
        let id = resolve_account(registry, &args.account)?.id.clone();
        let mut planned = registry.clone();
        let account = account_mut(&mut planned, &id)?;
        let mut changed = account.enabled != enabled;
        account.enabled = enabled;
        if !enabled && planned.default_account_id.as_ref() == Some(&id) {
            let fallback = authenticated_fallback(config, &planned, Some(&id))?;
            changed |= planned.default_account_id != fallback;
            planned.default_account_id = fallback;
        } else if enabled && planned.default_account_id.is_none() {
            let selected = planned
                .accounts
                .iter()
                .find(|account| account.id == id)
                .ok_or_else(|| AccountCommandError::new(AccountErrorKind::Integrity))?;
            if is_authenticated(config, selected)? {
                planned.default_account_id = Some(id.clone());
                changed = true;
            }
        }
        Ok((planned, changed, id))
    })?;
    view::mutation(
        config,
        &registry,
        &id,
        if enabled { "enable" } else { "disable" },
        changed,
        json,
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RemovalJson {
    schema_version: u32,
    generation: u64,
    removed_account: AccountAlias,
    credentials_removed: bool,
    active_account: Option<AccountAlias>,
}

fn remove(
    config: &Config,
    store: &RegistryStore,
    args: RemoveArgs,
    json: bool,
) -> Result<(), AccountCommandError> {
    let initial = read_registry(store)?;
    let account = resolve_account(&initial, &args.account)?;
    let alias = account.alias.clone();
    if !args.yes {
        if json || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
            return Err(AccountCommandError::new(
                AccountErrorKind::ConfirmationRequired,
            ));
        }
        eprint!("Remove account {alias} and permanently delete its credentials? [y/N]: ");
        std::io::stderr()
            .flush()
            .map_err(|_| AccountCommandError::new(AccountErrorKind::Output))?;
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|_| AccountCommandError::new(AccountErrorKind::Output))?;
        if !confirmation_accepted(&answer) {
            println!("Removal cancelled.");
            return Ok(());
        }
    }

    let mut expected = args.expected_generation.unwrap_or(initial.generation);
    let auth_config = config.auth_config();
    let mut outcome = None;
    for _ in 0..MAX_MUTATION_ATTEMPTS {
        match ProfileAuthRouter::remove_persistent_account(&auth_config, &account.id, expected) {
            Ok(removed) => {
                outcome = Some(removed);
                break;
            }
            Err(ProfileAuthRouterError::Registry(RegistryStoreError::GenerationConflict {
                actual,
                ..
            })) if args.expected_generation.is_none() => expected = actual,
            Err(error) => return Err(map_router_error(error)),
        }
    }
    let outcome =
        outcome.ok_or_else(|| AccountCommandError::new(AccountErrorKind::GenerationConflict))?;
    let updated = read_registry(store)?;
    let active_account = outcome.default_account_id.as_ref().and_then(|id| {
        updated
            .accounts
            .iter()
            .find(|account| &account.id == id)
            .map(|account| account.alias.clone())
    });
    if json {
        view::print_json(&RemovalJson {
            schema_version: JSON_SCHEMA_VERSION,
            generation: outcome.generation,
            removed_account: alias,
            credentials_removed: outcome.credentials_removed,
            active_account,
        })
    } else {
        if outcome.credentials_removed {
            println!("Removed account {alias} and its stored credentials.");
        } else {
            println!("Removed account {alias}; no stored credentials were present.");
        }
        Ok(())
    }
}

fn confirmation_accepted(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AutoJson {
    schema_version: u32,
    generation: u64,
    enabled: bool,
    policy: &'static str,
    priority_order: &'static str,
    changed: bool,
}

fn auto(store: &RegistryStore, args: AutoArgs, json: bool) -> Result<(), AccountCommandError> {
    let action = args.action.unwrap_or(AutoAction::Status);
    let (registry, changed) = match action {
        AutoAction::Status => (read_or_empty(store)?, false),
        AutoAction::On(generation) | AutoAction::Off(generation) => {
            let enabled = matches!(action, AutoAction::On(_));
            let (registry, changed, ()) =
                mutate_registry(store, generation.expected_generation, |registry| {
                    let mut planned = registry.clone();
                    let changed = planned.auto_selection.enabled != enabled;
                    planned.auto_selection.enabled = enabled;
                    Ok((planned, changed, ()))
                })?;
            (registry, changed)
        }
    };
    if json {
        view::print_json(&AutoJson {
            schema_version: JSON_SCHEMA_VERSION,
            generation: registry.generation,
            enabled: registry.auto_selection.enabled,
            policy: "priority",
            priority_order: "higherFirst",
            changed,
        })
    } else {
        println!(
            "Automatic account selection is {} (priority policy: higher numbers drain first and smaller numbers drain last; eligible: locally managed ChatGPT OAuth profiles only).",
            if registry.auto_selection.enabled {
                "on"
            } else {
                "off"
            }
        );
        Ok(())
    }
}

fn mutate_registry<T, F>(
    store: &RegistryStore,
    required_generation: Option<u64>,
    mut plan: F,
) -> Result<(AccountRegistry, bool, T), AccountCommandError>
where
    F: FnMut(&AccountRegistry) -> Result<(AccountRegistry, bool, T), AccountCommandError>,
{
    for _ in 0..MAX_MUTATION_ATTEMPTS {
        let (current, missing) = match store.read() {
            Ok(registry) => (registry, false),
            Err(RegistryStoreError::NotFound) => (AccountRegistry::default(), true),
            Err(_) => return Err(AccountCommandError::new(AccountErrorKind::Registry)),
        };
        if required_generation.is_some_and(|required| required != current.generation) {
            return Err(AccountCommandError::new(
                AccountErrorKind::GenerationConflict,
            ));
        }
        let (planned, changed, value) = plan(&current)?;
        if !changed {
            return Ok((current, false, value));
        }
        validate_registry(&planned)?;
        let guard = store
            .acquire_lock()
            .map_err(|_| AccountCommandError::new(AccountErrorKind::Registry))?;
        if missing {
            match store.create_with_guard(&guard, &planned) {
                Ok(()) => return Ok((planned, true, value)),
                Err(RegistryStoreError::AlreadyExists) if required_generation.is_none() => {
                    continue;
                }
                Err(RegistryStoreError::AlreadyExists) => {
                    return Err(AccountCommandError::new(
                        AccountErrorKind::GenerationConflict,
                    ));
                }
                Err(RegistryStoreError::CommittedDurabilityUncertain { .. }) => {
                    store
                        .repair_committed_durability_with_guard(&guard)
                        .map_err(|_| AccountCommandError::new(AccountErrorKind::Registry))?;
                    return Ok((read_registry(store)?, true, value));
                }
                Err(_) => return Err(AccountCommandError::new(AccountErrorKind::Registry)),
            }
        }
        match store.compare_and_swap_with_guard(&guard, current.generation, |registry| {
            *registry = planned.clone();
        }) {
            Ok(updated) => return Ok((updated, true, value)),
            Err(RegistryStoreError::GenerationConflict { .. }) if required_generation.is_none() => {
            }
            Err(RegistryStoreError::CommittedDurabilityUncertain { .. }) => {
                store
                    .repair_committed_durability_with_guard(&guard)
                    .map_err(|_| AccountCommandError::new(AccountErrorKind::Registry))?;
                return Ok((read_registry(store)?, true, value));
            }
            Err(RegistryStoreError::GenerationConflict { .. }) => {
                return Err(AccountCommandError::new(
                    AccountErrorKind::GenerationConflict,
                ));
            }
            Err(_) => return Err(AccountCommandError::new(AccountErrorKind::Registry)),
        }
    }
    Err(AccountCommandError::new(
        AccountErrorKind::GenerationConflict,
    ))
}

fn read_or_empty(store: &RegistryStore) -> Result<AccountRegistry, AccountCommandError> {
    match store.read() {
        Ok(registry) => Ok(registry),
        Err(RegistryStoreError::NotFound) => Ok(AccountRegistry::default()),
        Err(_) => Err(AccountCommandError::new(AccountErrorKind::Registry)),
    }
}

fn read_registry(store: &RegistryStore) -> Result<AccountRegistry, AccountCommandError> {
    store.read().map_err(|error| match error {
        RegistryStoreError::NotFound => AccountCommandError::new(AccountErrorKind::UnknownAccount),
        RegistryStoreError::GenerationConflict { .. } => {
            AccountCommandError::new(AccountErrorKind::GenerationConflict)
        }
        _ => AccountCommandError::new(AccountErrorKind::Registry),
    })
}

fn resolve_account<'a>(
    registry: &'a AccountRegistry,
    reference: &str,
) -> Result<&'a AccountMetadata, AccountCommandError> {
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
            Err(AccountCommandError::new(AccountErrorKind::AmbiguousAccount))
        }
        (Some(account), _) | (_, Some(account)) => Ok(account),
        (None, None) => Err(AccountCommandError::new(AccountErrorKind::UnknownAccount)),
    }
}

fn account_mut<'a>(
    registry: &'a mut AccountRegistry,
    id: &AccountId,
) -> Result<&'a mut AccountMetadata, AccountCommandError> {
    registry
        .accounts
        .iter_mut()
        .find(|account| &account.id == id)
        .ok_or_else(|| AccountCommandError::new(AccountErrorKind::UnknownAccount))
}

fn require_enabled(account: &AccountMetadata) -> Result<(), AccountCommandError> {
    if account.enabled {
        Ok(())
    } else {
        Err(AccountCommandError::new(AccountErrorKind::DisabledAccount))
    }
}

fn require_authenticated(
    config: &Config,
    account: &AccountMetadata,
) -> Result<(), AccountCommandError> {
    if is_authenticated(config, account)? {
        Ok(())
    } else {
        Err(AccountCommandError::new(AccountErrorKind::NotAuthenticated))
    }
}

fn is_authenticated(
    config: &Config,
    account: &AccountMetadata,
) -> Result<bool, AccountCommandError> {
    let profile = ProfileAuthStorage::new(
        &config.codex_home,
        account.id.clone(),
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
    )
    .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?;
    profile
        .load()
        .map(|auth| auth.is_some())
        .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))
}

fn authenticated_fallback(
    config: &Config,
    registry: &AccountRegistry,
    excluded: Option<&AccountId>,
) -> Result<Option<AccountId>, AccountCommandError> {
    for account in registry.enabled_by_priority() {
        if excluded == Some(&account.id) {
            continue;
        }
        if is_authenticated(config, account)? {
            return Ok(Some(account.id.clone()));
        }
    }
    Ok(None)
}

fn validate_registry(registry: &AccountRegistry) -> Result<(), AccountCommandError> {
    registry.validate().map_err(|error| match error {
        RegistryValidationError::DuplicateAlias { .. } => {
            AccountCommandError::new(AccountErrorKind::InvalidInput)
        }
        RegistryValidationError::DuplicateId { .. }
        | RegistryValidationError::DuplicateServiceIdentity { .. }
        | RegistryValidationError::MissingDefault { .. }
        | RegistryValidationError::UnsupportedVersion { .. } => {
            AccountCommandError::new(AccountErrorKind::Integrity)
        }
    })
}

fn map_router_error(error: ProfileAuthRouterError) -> AccountCommandError {
    match error {
        ProfileAuthRouterError::UnknownAccount => {
            AccountCommandError::new(AccountErrorKind::UnknownAccount)
        }
        ProfileAuthRouterError::AmbiguousAccount => {
            AccountCommandError::new(AccountErrorKind::AmbiguousAccount)
        }
        ProfileAuthRouterError::DisabledAccount => {
            AccountCommandError::new(AccountErrorKind::DisabledAccount)
        }
        ProfileAuthRouterError::NotAuthenticated => {
            AccountCommandError::new(AccountErrorKind::NotAuthenticated)
        }
        ProfileAuthRouterError::AccountInUse => {
            AccountCommandError::new(AccountErrorKind::AccountInUse)
        }
        ProfileAuthRouterError::Registry(RegistryStoreError::GenerationConflict { .. })
        | ProfileAuthRouterError::RegistryChanged => {
            AccountCommandError::new(AccountErrorKind::GenerationConflict)
        }
        ProfileAuthRouterError::Authentication(_) => {
            AccountCommandError::new(AccountErrorKind::CredentialStore)
        }
        ProfileAuthRouterError::EphemeralStorageUnsupported
        | ProfileAuthRouterError::PinConflict { .. }
        | ProfileAuthRouterError::ExternalAuthRequiresSingularPath { .. }
        | ProfileAuthRouterError::Registry(_)
        | ProfileAuthRouterError::Migration(_)
        | ProfileAuthRouterError::Selection(_) => {
            AccountCommandError::new(AccountErrorKind::Registry)
        }
    }
}

#[cfg(test)]
#[path = "account_cmd_tests.rs"]
mod tests;
