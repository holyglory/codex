use std::time::Duration;

use chrono::Utc;
use codex_account_registry::AccountMetadata;
use codex_account_registry::RegistryStore;
use codex_backend_client::Client as BackendClient;
use codex_core::config::Config;
use codex_login::ProfileAuthRouter;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use serde::Serialize;

use super::AccountCommandError;
use super::AccountErrorKind;
use super::JSON_SCHEMA_VERSION;
use super::LimitsArgs;
use super::read_registry;
use super::require_enabled;
use super::resolve_account;
use super::view::print_json;

const LIMIT_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LimitsJson {
    schema_version: u32,
    observed_at: i64,
    accounts: Vec<AccountLimitsJson>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountLimitsJson {
    id: String,
    alias: String,
    state: &'static str,
    reason: Option<&'static str>,
    buckets: Vec<RateLimitSnapshot>,
}

pub(super) async fn run(
    config: &Config,
    store: &RegistryStore,
    args: LimitsArgs,
    json: bool,
) -> Result<(), AccountCommandError> {
    let registry = read_registry(store)?;
    let mut selected = if args.all {
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

    let router = ProfileAuthRouter::open_for_management(config.auth_config())
        .await
        .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?;
    let mut results = Vec::with_capacity(selected.len());
    let mut observed = 0usize;
    for account in selected.drain(..) {
        let result = fetch_account_limits(config, &router, &account).await;
        if result.state == "observed" {
            observed += 1;
        }
        results.push(result);
    }
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
    if observed == 0 {
        Err(AccountCommandError::new(
            AccountErrorKind::RateLimitsUnavailable,
        ))
    } else {
        Ok(())
    }
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
    AccountLimitsJson {
        id: account.id.to_string(),
        alias: account.alias.to_string(),
        state: "observed",
        reason: None,
        buckets,
    }
}

fn unknown(account: &AccountMetadata, reason: &'static str) -> AccountLimitsJson {
    AccountLimitsJson {
        id: account.id.to_string(),
        alias: account.alias.to_string(),
        state: "unknown",
        reason: Some(reason),
        buckets: Vec::new(),
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

fn window_label(window: Option<&RateLimitWindow>) -> String {
    match window {
        Some(window) => format!("{:.1}% used", window.used_percent),
        None => "unknown".to_string(),
    }
}
