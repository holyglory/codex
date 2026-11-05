use std::cmp::Ordering;
use std::io::IsTerminal;

use codex_account_registry::AccountAlias;
use codex_account_registry::AccountId;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_core::config::Config;
use codex_login::ProfileAuthStorage;
use codex_protocol::auth::AuthMode;
use codex_protocol::auth::PlanType;
use owo_colors::OwoColorize;
use owo_colors::Style;
use serde::Serialize;

use super::AccountCommandError;
use super::AccountErrorKind;
use super::JSON_SCHEMA_VERSION;
use super::limits;
use super::limits::AccountLimitsJson;
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
struct AccountListJson<'a> {
    schema_version: u32,
    generation: u64,
    active_account: Option<AccountAlias>,
    priority_order: &'static str,
    accounts: Vec<AccountListEntry<'a>>,
}

#[derive(Serialize)]
struct AccountListEntry<'a> {
    #[serde(flatten)]
    account: AccountView,
    #[serde(skip_serializing_if = "Option::is_none")]
    limits: Option<&'a AccountLimitsJson>,
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
    render_list(config, &registry, json, &[])
}

pub(super) async fn list_with_limits(
    config: &Config,
    store: &RegistryStore,
    json: bool,
) -> Result<(), AccountCommandError> {
    let registry = read_or_empty(store)?;
    let limits = limits::fetch_all(config, &registry.accounts)
        .await
        .unwrap_or_else(|_| {
            registry
                .accounts
                .iter()
                .map(|account| limits::unknown(account, "credentialUnavailable"))
                .collect()
        });
    render_list(config, &registry, json, &limits)
}

fn render_list(
    config: &Config,
    registry: &AccountRegistry,
    json: bool,
    limits: &[AccountLimitsJson],
) -> Result<(), AccountCommandError> {
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
        .map(|account| -> Result<_, AccountCommandError> {
            Ok(AccountListEntry {
                account: AccountView::load(config, registry, account)?,
                limits: limits
                    .iter()
                    .find(|limit| limit.id == account.id.to_string()),
            })
        })
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
        let mut views = views;
        views.sort_by(compare_human_accounts);
        let alias_width = views
            .iter()
            .map(|entry| entry.account.alias.as_str().len())
            .max()
            .unwrap_or("ALIAS".len())
            .max("ALIAS".len());
        let alias_column_width = alias_width + 1;
        let color = std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && supports_color::on(supports_color::Stream::Stdout).is_some();
        let header = if limits.is_empty() {
            format!("{:<alias_column_width$}\tPRIORITY\tNOTE", "ALIAS")
        } else {
            format!(
                "{:<alias_column_width$}\tPRIORITY\tNOTE\tBANKED RESETS\tEXPIRY IN\tLIMITS\tRESET IN",
                "ALIAS"
            )
        };
        let header_style = if color {
            Style::new().bold().cyan()
        } else {
            Style::new()
        };
        println!("{}", header.style(header_style));
        for entry in views {
            let account = entry.account;
            let account_style = if color && account.current {
                Style::new().bold().green()
            } else {
                Style::new()
            };
            let alias = format!(
                "{}{:<alias_width$}",
                if account.current { "*" } else { " " },
                account.alias.as_str()
            );
            let limits_columns = match entry.limits {
                Some(limits) => {
                    let summary = if limits.state == "observed" {
                        limits::codex_usage_percent(limits)
                            .map(|percent| styled_usage_percent(percent, color))
                            .unwrap_or_else(|| "unknown".to_string())
                    } else {
                        format!("unknown ({})", limits.reason.unwrap_or("unavailable"))
                    };
                    let (banked_count, expiry) = match &limits.banked_resets {
                        Some(resets) => {
                            let expiry = if resets.available_count == 0 {
                                "none".to_string()
                            } else if !resets.expiry_known {
                                "unknown".to_string()
                            } else if let Some(expires_at) = resets.soonest_expires_at {
                                limits::reset_countdown_days_hours(
                                    Some(expires_at),
                                    chrono::Utc::now().timestamp(),
                                )
                            } else {
                                "never".to_string()
                            };
                            let banked_count = resets.available_count.to_string();
                            let banked_count = format!("{banked_count:<13}");
                            let banked_count = if color && resets.available_count > 0 {
                                banked_count.style(Style::new().bold().green()).to_string()
                            } else {
                                banked_count
                            };
                            (banked_count, expiry)
                        }
                        None => (format!("{:<13}", "unknown"), "unknown".to_string()),
                    };
                    format!(
                        "\t{banked_count}\t{expiry:<9}\t{summary}\t{}",
                        limits::reset_countdown(
                            limits.next_reset_at,
                            chrono::Utc::now().timestamp()
                        )
                    )
                }
                None => String::new(),
            };
            println!(
                "{}\t{}\t{}{limits_columns}",
                alias.style(account_style),
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

fn compare_human_accounts(left: &AccountListEntry<'_>, right: &AccountListEntry<'_>) -> Ordering {
    let left_usage = left.limits.and_then(limits::codex_usage_percent);
    let right_usage = right.limits.and_then(limits::codex_usage_percent);
    let left_group = usage_group(left_usage);
    let right_group = usage_group(right_usage);
    left_group
        .cmp(&right_group)
        .then_with(|| {
            if left_group == 0 {
                left_usage
                    .unwrap_or(100.0)
                    .total_cmp(&right_usage.unwrap_or(100.0))
            } else {
                Ordering::Equal
            }
        })
        .then_with(|| {
            if left_group == 1 {
                banked_count(right).cmp(&banked_count(left))
            } else {
                Ordering::Equal
            }
        })
        .then_with(|| reset_at(left).cmp(&reset_at(right)))
        .then_with(|| right.account.priority.cmp(&left.account.priority))
        .then_with(|| left.account.alias.cmp(&right.account.alias))
        .then_with(|| left.account.id.cmp(&right.account.id))
}

fn usage_group(usage: Option<f64>) -> u8 {
    match usage {
        Some(percent) if percent < 100.0 => 0,
        Some(_) => 1,
        None => 2,
    }
}

fn banked_count(entry: &AccountListEntry<'_>) -> i64 {
    entry
        .limits
        .and_then(|limits| limits.banked_resets.as_ref())
        .map_or(-1, |resets| resets.available_count)
}

fn reset_at(entry: &AccountListEntry<'_>) -> i64 {
    entry
        .limits
        .and_then(|limits| limits.next_reset_at)
        .unwrap_or(i64::MAX)
}

fn styled_usage_percent(percent: f64, color: bool) -> String {
    let text = format!("{percent:.0}%");
    if !color {
        return text;
    }
    let style = if percent >= 67.0 {
        Style::new().red()
    } else if percent >= 34.0 {
        Style::new().yellow()
    } else {
        Style::new().green()
    };
    text.style(style).to_string()
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

pub(super) fn safe_human_text(value: &str) -> String {
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
