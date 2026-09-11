use std::time::Duration;

use chrono::Utc;
use codex_account_registry::AccountMetadata;
use codex_account_registry::RegistryStore;
use codex_backend_client::Client as BackendClient;
use codex_core::config::Config;
use codex_login::ProfileAuthRouter;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use futures::future::join_all;
use serde::Serialize;

use super::AccountCommandError;
use super::AccountErrorKind;
use super::JSON_SCHEMA_VERSION;
use super::LimitsArgs;
use super::read_registry;
use super::require_enabled;
use super::resolve_account;
use super::view::print_json;

const LIMIT_FETCH_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 10);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LimitsJson {
    schema_version: u32,
    observed_at: i64,
    accounts: Vec<AccountLimitsJson>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AccountLimitsJson {
    pub(super) id: String,
    alias: String,
    pub(super) state: &'static str,
    pub(super) reason: Option<&'static str>,
    pub(super) buckets: Vec<RateLimitSnapshot>,
    pub(super) next_reset_at: Option<i64>,
}

pub(super) async fn run(
    config: &Config,
    store: &RegistryStore,
    args: LimitsArgs,
    json: bool,
) -> Result<(), AccountCommandError> {
    let registry = read_registry(store)?;
    let selected = if args.all {
        let mut accounts = registry.accounts.clone();
        accounts.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.alias.cmp(&right.alias))
                .then_with(|| left.id.cmp(&right.id))
        });
        accounts
    } else {
        let account = match args.account.as_deref() {
            Some(reference) => resolve_account(&registry, reference)?,
            None => {
                let id = registry
                    .default_account_id
                    .as_ref()
                    .ok_or_else(|| AccountCommandError::new(AccountErrorKind::UnknownAccount))?;
                registry
                    .accounts
                    .iter()
                    .find(|account| &account.id == id)
                    .ok_or_else(|| AccountCommandError::new(AccountErrorKind::Integrity))?
            }
        };
        require_enabled(account)?;
        vec![account.clone()]
    };
    if selected.is_empty() {
        return Err(AccountCommandError::new(
            AccountErrorKind::RateLimitsUnavailable,
        ));
    }

    let results = fetch_all(config, &selected).await?;
    let observed = results.iter().any(|result| result.state == "observed");
    let report = LimitsJson {
        schema_version: JSON_SCHEMA_VERSION,
        observed_at: Utc::now().timestamp(),
        accounts: results,
    };
    if json {
        print_json(&report)?;
    } else {
        print_human(&report);
    }
    if !observed {
        Err(AccountCommandError::new(
            AccountErrorKind::RateLimitsUnavailable,
        ))
    } else {
        Ok(())
    }
}

pub(super) async fn fetch_all(
    config: &Config,
    accounts: &[AccountMetadata],
) -> Result<Vec<AccountLimitsJson>, AccountCommandError> {
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    let router = ProfileAuthRouter::open_for_management(config.auth_config())
        .await
        .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?;
    Ok(join_all(
        accounts
            .iter()
            .map(|account| fetch_account_limits(config, &router, account)),
    )
    .await)
}

async fn fetch_account_limits(
    config: &Config,
    router: &ProfileAuthRouter,
    account: &AccountMetadata,
) -> AccountLimitsJson {
    if !account.enabled {
        return unknown(account, "disabled");
    }
    let lease = match router.lease_for_account(&account.id).await {
        Ok(lease) => lease,
        Err(_) => return unknown(account, "credentialUnavailable"),
    };
    let Some(auth) = lease.auth_manager().auth().await else {
        return unknown(account, "notAuthenticated");
    };
    if !auth.uses_codex_backend() {
        return unknown(account, "unsupportedAuthentication");
    }
    let client = BackendClient::from_auth(
        config.chatgpt_base_url.clone(),
        &auth,
        config.http_client_factory(),
    );
    let response =
        match tokio::time::timeout(LIMIT_FETCH_TIMEOUT, client.get_rate_limits_many()).await {
            Ok(Ok(snapshots)) => snapshots,
            Ok(Err(_)) => return unknown(account, "requestFailed"),
            Err(_) => return unknown(account, "requestTimedOut"),
        };
    if response.is_empty() || response.iter().any(invalid_snapshot) {
        return unknown(account, "invalidResponse");
    }
    let mut buckets = response;
    buckets.sort_by(|left, right| {
        left.limit_id
            .cmp(&right.limit_id)
            .then_with(|| left.limit_name.cmp(&right.limit_name))
    });
    let now = Utc::now().timestamp();
    let next_reset_at = buckets
        .iter()
        .flat_map(|bucket| {
            bucket
                .primary
                .iter()
                .chain(bucket.secondary.iter())
                .filter_map(|window| window.resets_at)
                .chain(bucket.individual_limit.iter().map(|limit| limit.resets_at))
        })
        .filter(|reset| {
            *reset > now && chrono::DateTime::from_timestamp(*reset, /*nsecs*/ 0).is_some()
        })
        .min();
    AccountLimitsJson {
        id: account.id.to_string(),
        alias: account.alias.to_string(),
        state: "observed",
        reason: None,
        buckets,
        next_reset_at,
    }
}

pub(super) fn unknown(account: &AccountMetadata, reason: &'static str) -> AccountLimitsJson {
    AccountLimitsJson {
        id: account.id.to_string(),
        alias: account.alias.to_string(),
        state: "unknown",
        reason: Some(reason),
        buckets: Vec::new(),
        next_reset_at: None,
    }
}

fn invalid_snapshot(snapshot: &RateLimitSnapshot) -> bool {
    snapshot.primary.as_ref().is_some_and(invalid_window)
        || snapshot.secondary.as_ref().is_some_and(invalid_window)
}

fn invalid_window(window: &RateLimitWindow) -> bool {
    !window.used_percent.is_finite() || !(0.0..=100.0).contains(&window.used_percent)
}

fn print_human(report: &LimitsJson) {
    for account in &report.accounts {
        println!("Account: {}", account.alias);
        if account.state == "unknown" {
            println!(
                "  State: unknown ({})",
                account.reason.unwrap_or("unavailable")
            );
            continue;
        }
        println!("  State: observed");
        for bucket in &account.buckets {
            println!(
                "  {}: primary {} secondary {}",
                bucket
                    .limit_name
                    .as_deref()
                    .or(bucket.limit_id.as_deref())
                    .unwrap_or("codex"),
                window_label(bucket.primary.as_ref()),
                window_label(bucket.secondary.as_ref()),
            );
        }
    }
}

pub(super) fn reset_label(reset: Option<i64>) -> String {
    reset
        .and_then(|reset| chrono::DateTime::from_timestamp(reset, /*nsecs*/ 0))
        .map(|reset| reset.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

pub(super) fn window_label(window: Option<&RateLimitWindow>) -> String {
    match window {
        Some(window) => format!(
            "{} (resets {})",
            window_usage_label(Some(window)),
            reset_label(window.resets_at)
        ),
        None => "unknown".to_string(),
    }
}

pub(super) fn window_usage_label(window: Option<&RateLimitWindow>) -> String {
    match window {
        Some(window) => {
            let duration = match window.window_minutes {
                Some(minutes) if minutes % (24 * 60) == 0 => format!("{}d", minutes / (24 * 60)),
                Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
                Some(minutes) => format!("{minutes}m"),
                None => "window".to_string(),
            };
            format!("{duration} {:.1}% used", window.used_percent)
        }
        None => "unknown".to_string(),
    }
}

pub(super) fn bucket_summary(bucket: &RateLimitSnapshot) -> String {
    use codex_protocol::protocol::RateLimitReachedType;
    let mut parts = bucket
        .primary
        .iter()
        .chain(bucket.secondary.iter())
        .map(|window| window_usage_label(Some(window)))
        .collect::<Vec<_>>();
    if let Some(limit) = &bucket.individual_limit {
        parts.push(format!(
            "individual {} / {} used ({}% left)",
            super::view::safe_human_text(&limit.used),
            super::view::safe_human_text(&limit.limit),
            limit.remaining_percent
        ));
    }
    if let Some(credits) = &bucket.credits {
        parts.push(if credits.unlimited {
            "unlimited credits".into()
        } else if !credits.has_credits {
            "credits depleted".into()
        } else if let Some(balance) = &credits.balance {
            format!("{} credits", super::view::safe_human_text(balance))
        } else {
            "credits available".into()
        });
    }
    if bucket.spend_control_reached == Some(true) {
        parts.push("spend limit reached".into());
    }
    if let Some(reason) = bucket.rate_limit_reached_type {
        parts.push(
            match reason {
                RateLimitReachedType::RateLimitReached => "rate limit reached",
                RateLimitReachedType::WorkspaceOwnerCreditsDepleted => {
                    "workspace owner credits depleted"
                }
                RateLimitReachedType::WorkspaceMemberCreditsDepleted => {
                    "workspace member credits depleted"
                }
                RateLimitReachedType::WorkspaceOwnerUsageLimitReached => {
                    "workspace owner usage limit reached"
                }
                RateLimitReachedType::WorkspaceMemberUsageLimitReached => {
                    "workspace member usage limit reached"
                }
            }
            .into(),
        );
    }
    if parts.is_empty() {
        parts.push("unknown".into());
    }
    parts.join(" / ")
}
