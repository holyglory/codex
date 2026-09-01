use codex_account_registry::AccountAlias;
use codex_account_registry::AccountId;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_core::config::Config;
use codex_login::ProfileAuthStorage;
use codex_protocol::auth::AuthMode;
use codex_protocol::auth::PlanType;
use serde::Serialize;

use super::AccountCommandError;
use super::AccountErrorKind;
use super::JSON_SCHEMA_VERSION;
use super::read_or_empty;
use super::read_registry;
use super::resolve_account;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountView {
    id: AccountId,
    alias: AccountAlias,
    auth_mode: AuthMode,
    email: Option<String>,
    plan_type: Option<PlanType>,
    enabled: bool,
    authenticated: bool,
    priority: u32,
    created_at: String,
    last_used_at: Option<String>,
    note: Option<String>,
    current: bool,
}

impl AccountView {
    fn load(
        config: &Config,
        registry: &AccountRegistry,
        account: &AccountMetadata,
    ) -> Result<Self, AccountCommandError> {
        let profile = ProfileAuthStorage::new(
            &config.codex_home,
            account.id.clone(),
            config.cli_auth_credentials_store_mode,
            config.auth_keyring_backend_kind(),
        )
        .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?;
        let auth = profile
            .load()
            .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?;
        let authenticated = auth.is_some();
        if auth.is_some_and(|auth| auth.resolved_mode() != account.auth_mode) {
            return Err(AccountCommandError::new(AccountErrorKind::Integrity));
        }
        Ok(Self {
            id: account.id.clone(),
            alias: account.alias.clone(),
            auth_mode: account.auth_mode,
            email: account.email.clone(),
            plan_type: account.plan_type.clone(),
            enabled: account.enabled,
            authenticated,
            priority: account.priority,
            created_at: account.created_at.to_rfc3339(),
            last_used_at: account.last_used_at.map(|value| value.to_rfc3339()),
            note: account.note.clone(),
            current: registry.default_account_id.as_ref() == Some(&account.id),
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountListJson {
    schema_version: u32,
    generation: u64,
    active_account: Option<AccountAlias>,
    priority_order: &'static str,
    accounts: Vec<AccountView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountJson {
    schema_version: u32,
    generation: u64,
    account: AccountView,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationJson {
    schema_version: u32,
    generation: u64,
    action: &'static str,
    changed: bool,
    account: AccountView,
}

pub(super) fn list(
    config: &Config,
    store: &RegistryStore,
    json: bool,
) -> Result<(), AccountCommandError> {
    let registry = read_or_empty(store)?;
    let mut accounts = registry.accounts.iter().collect::<Vec<_>>();
    accounts.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.alias.cmp(&right.alias))
            .then_with(|| left.id.cmp(&right.id))
    });
    let views = accounts
        .into_iter()
        .map(|account| AccountView::load(config, &registry, account))
        .collect::<Result<Vec<_>, _>>()?;
    let active_account = registry
        .default_account_id
        .as_ref()
        .and_then(|id| registry.accounts.iter().find(|account| &account.id == id))
        .map(|account| account.alias.clone());
    if json {
        print_json(&AccountListJson {
            schema_version: JSON_SCHEMA_VERSION,
            generation: registry.generation,
            active_account,
            priority_order: "higherFirst",
            accounts: views,
        })
    } else {
        if views.is_empty() {
            println!("No account profiles configured.");
            return Ok(());
        }
        println!("CURRENT\tALIAS\tSTATUS\tAUTH\tPRIORITY (HIGHER DRAINS FIRST)\tNOTE");
        for account in views {
            let current = if account.current { "*" } else { "" };
            let status = match (account.enabled, account.authenticated) {
                (false, _) => "disabled",
                (true, false) => "logged-out",
                (true, true) => "ready",
            };
            println!(
                "{current}\t{}\t{status}\t{}\t{}\t{}",
                account.alias,
                auth_mode_label(account.auth_mode),
                account.priority,
                account
                    .note
                    .as_deref()
                    .map(safe_human_text)
                    .as_deref()
                    .unwrap_or("")
            );
        }
        Ok(())
    }
}

pub(super) fn current(
    config: &Config,
    store: &RegistryStore,
    json: bool,
) -> Result<(), AccountCommandError> {
    let registry = read_registry(store)?;
    let id = registry
        .default_account_id
        .as_ref()
        .ok_or_else(|| AccountCommandError::new(AccountErrorKind::UnknownAccount))?;
    let account = registry
        .accounts
        .iter()
        .find(|account| &account.id == id)
        .ok_or_else(|| AccountCommandError::new(AccountErrorKind::Integrity))?;
    output_account(config, &registry, account, json)
}

pub(super) fn show(
    config: &Config,
    store: &RegistryStore,
    reference: &str,
    json: bool,
) -> Result<(), AccountCommandError> {
    let registry = read_registry(store)?;
    let account = resolve_account(&registry, reference)?;
    output_account(config, &registry, account, json)
}

fn output_account(
    config: &Config,
    registry: &AccountRegistry,
    account: &AccountMetadata,
    json: bool,
) -> Result<(), AccountCommandError> {
    let view = AccountView::load(config, registry, account)?;
    if json {
        print_json(&AccountJson {
            schema_version: JSON_SCHEMA_VERSION,
            generation: registry.generation,
            account: view,
        })
    } else {
        println!("Account: {}", view.alias);
        println!(
            "Status: {}",
            if view.enabled { "enabled" } else { "disabled" }
        );
        println!(
            "Authenticated: {}",
            if view.authenticated { "yes" } else { "no" }
        );
        println!("Authentication: {}", auth_mode_label(view.auth_mode));
        println!("Priority: {}", view.priority);
        if let Some(email) = view.email {
            println!("Email: {}", safe_human_text(&email));
        }
        if let Some(note) = view.note {
            println!("Note: {}", safe_human_text(&note));
        }
        Ok(())
    }
}

pub(super) fn mutation(
    config: &Config,
    registry: &AccountRegistry,
    id: &AccountId,
    action: &'static str,
    changed: bool,
    json: bool,
) -> Result<(), AccountCommandError> {
    let account = registry
        .accounts
        .iter()
        .find(|account| &account.id == id)
        .ok_or_else(|| AccountCommandError::new(AccountErrorKind::Integrity))?;
    let view = AccountView::load(config, registry, account)?;
    if json {
        print_json(&MutationJson {
            schema_version: JSON_SCHEMA_VERSION,
            generation: registry.generation,
            action,
            changed,
            account: view,
        })
    } else {
        println!(
            "Account {} {}.",
            view.alias,
            if changed {
                "updated"
            } else {
                "was already unchanged"
            }
        );
        Ok(())
    }
}

fn auth_mode_label(mode: AuthMode) -> &'static str {
    match mode {
        AuthMode::ApiKey => "api-key",
        AuthMode::Chatgpt => "chatgpt",
        AuthMode::ChatgptAuthTokens => "external-chatgpt",
        AuthMode::Headers => "headers",
        AuthMode::AgentIdentity => "agent-identity",
        AuthMode::PersonalAccessToken => "personal-access-token",
        AuthMode::BedrockApiKey => "bedrock-api-key",
        AuthMode::BedrockAccessKeys => "bedrock-access-keys",
    }
}

fn safe_human_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

pub(super) fn print_json(value: &impl Serialize) -> Result<(), AccountCommandError> {
    let encoded = serde_json::to_string_pretty(value)
        .map_err(|_| AccountCommandError::new(AccountErrorKind::Output))?;
    println!("{encoded}");
    Ok(())
}
