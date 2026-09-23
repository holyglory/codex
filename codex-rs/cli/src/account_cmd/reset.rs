//! Applies one account-scoped banked reset and retains replay identities on uncertainty.
use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use codex_backend_client::Client as BackendClient;
use codex_backend_client::ConsumeRateLimitResetCreditCode;
use codex_backend_client::RateLimitResetCreditDetails;
use codex_backend_client::RequestError;
use codex_core::config::Config;
use codex_login::ProfileAuthRouter;
use codex_protocol::protocol::RateLimitSnapshot;
use serde::Serialize;
use uuid::Uuid;

use super::AccountCommandError;
use super::AccountErrorKind;
use super::RegistryStore;
use super::ResetArgs;
use super::limits::reset_label;
use super::map_router_error;
use super::read_registry;
use super::require_enabled;
use super::resolve_account;
use super::view::print_json;

const RESET_REQUEST_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 10);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResetJson {
    schema_version: u32,
    account: String,
    credit_id: String,
    request_id: String,
    outcome: &'static str,
    windows_reset: Option<i64>,
    refreshed_usage: Option<Vec<RateLimitSnapshot>>,
    refreshed_banked_resets: Option<RefreshedBankedResets>,
    refresh_error: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RefreshedBankedResets {
    available_count: i64,
    soonest_expires_at: Option<i64>,
    expiry_known: bool,
}

pub(super) async fn run(
    config: &Config,
    store: &RegistryStore,
    args: ResetArgs,
    json: bool,
) -> Result<(), AccountCommandError> {
    for value in [args.credit_id.as_deref(), args.request_id.as_deref()]
        .into_iter()
        .flatten()
    {
        if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
            return Err(AccountCommandError::new(AccountErrorKind::InvalidInput));
        }
    }
    let registry = read_registry(store)?;
    let account = resolve_account(&registry, &args.account)?;
    require_enabled(account)?;
    let router = ProfileAuthRouter::open_for_management(config.auth_config())
        .await
        .map_err(map_router_error)?;
    let lease = router
        .lease_for_account(&account.id)
        .await
        .map_err(map_router_error)?;
    let Some(auth) = lease.auth_manager().auth().await else {
        return Err(AccountCommandError::new(AccountErrorKind::NotAuthenticated));
    };
    if !auth.uses_codex_backend() {
        return Err(AccountCommandError::new(
            AccountErrorKind::ResetUnsupportedAuth,
        ));
    }
    let client = BackendClient::from_auth(
        config.chatgpt_base_url.clone(),
        &auth,
        config.http_client_factory(),
    );
    // A replay must reach the idempotent endpoint even if the credit is already redeemed or
    // expired. Requiring it to still be available would prevent recovery after a lost response.
    let credit_id = if args.request_id.is_some() {
        args.credit_id
            .ok_or_else(|| AccountCommandError::new(AccountErrorKind::InvalidInput))?
    } else {
        let details = tokio::time::timeout(
            RESET_REQUEST_TIMEOUT,
            client.list_rate_limit_reset_credits(),
        )
        .await
        .map_err(|_| AccountCommandError::new(AccountErrorKind::ResetFailed))?
        .map_err(|_| AccountCommandError::new(AccountErrorKind::ResetFailed))?;
        if details.available_count < 0 {
            return Err(AccountCommandError::new(AccountErrorKind::ResetFailed));
        }
        if details.available_count == 0 {
            return Err(AccountCommandError::new(AccountErrorKind::NoResetCredit));
        }
        select_credit(&details.credits, args.credit_id.as_deref())?
    };
    let request_id = args
        .request_id
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let mut report = ResetJson {
        schema_version: super::JSON_SCHEMA_VERSION,
        account: account.alias.to_string(),
        credit_id,
        request_id,
        outcome: "uncertain",
        windows_reset: None,
        refreshed_usage: None,
        refreshed_banked_resets: None,
        refresh_error: None,
    };
    let mut failure = Some(AccountErrorKind::ResetUncertain);
    for _ in 0..2 {
        let response = tokio::time::timeout(
            RESET_REQUEST_TIMEOUT,
            client.consume_rate_limit_reset_credit_by_id(&report.request_id, &report.credit_id),
        )
        .await;
        match response {
            Ok(Ok(response)) if response.windows_reset >= 0 => {
                report.outcome = match response.code {
                    ConsumeRateLimitResetCreditCode::Reset => "reset",
                    ConsumeRateLimitResetCreditCode::NothingToReset => "nothingToReset",
                    ConsumeRateLimitResetCreditCode::NoCredit => "noCredit",
                    ConsumeRateLimitResetCreditCode::AlreadyRedeemed => "alreadyRedeemed",
                };
                report.windows_reset = Some(response.windows_reset);
                failure = (response.code == ConsumeRateLimitResetCreditCode::NoCredit)
                    .then_some(AccountErrorKind::NoResetCredit);
                break;
            }
            Ok(Err(error))
                if error
                    .downcast_ref::<RequestError>()
                    .and_then(RequestError::status)
                    .is_some_and(|status| matches!(status.as_u16(), 400 | 401 | 403 | 404)) =>
            {
                report.outcome = "failed";
                failure = Some(AccountErrorKind::ResetFailed);
                break;
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {}
        }
    }
    if report.windows_reset.is_some() {
        let (usage, details) = tokio::join!(
            tokio::time::timeout(
                RESET_REQUEST_TIMEOUT,
                client.get_rate_limits_with_reset_credits()
            ),
            tokio::time::timeout(
                RESET_REQUEST_TIMEOUT,
                client.list_rate_limit_reset_credits()
            )
        );
        match usage {
            Ok(Ok(usage)) => report.refreshed_usage = Some(usage.rate_limits),
            Ok(Err(_)) | Err(_) => report.refresh_error = Some("usageUnavailable"),
        }
        match details {
            Ok(Ok(details)) if details.available_count >= 0 => {
                let available = details
                    .credits
                    .iter()
                    .filter(|credit| credit.status == "available")
                    .collect::<Vec<_>>();
                let mut expiry_known = available.len() as i64 == details.available_count;
                let soonest = available
                    .iter()
                    .filter_map(|credit| {
                        let value = credit.expires_at.as_deref()?;
                        match DateTime::parse_from_rfc3339(value) {
                            Ok(date) => Some(date.timestamp()),
                            Err(_) => {
                                expiry_known = false;
                                None
                            }
                        }
                    })
                    .min();
                report.refreshed_banked_resets = Some(RefreshedBankedResets {
                    available_count: details.available_count,
                    soonest_expires_at: expiry_known.then_some(soonest).flatten(),
                    expiry_known,
                });
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {
                report.refresh_error = Some("resetDetailsUnavailable")
            }
        }
    }
    if json {
        print_json(&report)?;
    } else {
        println!(
            "Account: {}\nOutcome: {}\nCredit: {}\nRequest: {}",
            report.account, report.outcome, report.credit_id, report.request_id
        );
        if let Some(windows) = report.windows_reset {
            println!("Windows reset: {windows}");
        }
        if let Some(resets) = &report.refreshed_banked_resets {
            let expiry = if resets.available_count == 0 {
                "none".to_string()
            } else if !resets.expiry_known {
                "unknown".to_string()
            } else if resets.soonest_expires_at.is_none() {
                "never".to_string()
            } else {
                reset_label(resets.soonest_expires_at)
            };
            println!(
                "Banked resets remaining: {}\nSoonest expiry: {expiry}",
                resets.available_count
            );
        }
        if let Some(error) = report.refresh_error {
            println!("Refresh: {error}");
        }
        if report.outcome == "uncertain" {
            println!(
                "Retry this account with --credit-id and --request-id using the values above."
            );
        }
    }
    match failure {
        Some(kind) => Err(AccountCommandError::new(kind)),
        None => Ok(()),
    }
}

fn select_credit(
    credits: &[RateLimitResetCreditDetails],
    requested_id: Option<&str>,
) -> Result<String, AccountCommandError> {
    let now = Utc::now();
    let mut candidates = Vec::new();
    for credit in credits {
        if credit.status != "available"
            || credit.reset_type != "codex_rate_limits"
            || credit.id.trim().is_empty()
            || credit.id.len() > 512
            || credit.id.chars().any(char::is_control)
        {
            continue;
        }
        let Ok(granted) = DateTime::parse_from_rfc3339(&credit.granted_at) else {
            continue;
        };
        if granted > now {
            continue;
        }
        let expires = match credit.expires_at.as_deref() {
            None => None,
            Some(value) => {
                let Ok(date) = DateTime::parse_from_rfc3339(value) else {
                    continue;
                };
                if date <= now || date <= granted {
                    continue;
                }
                Some(date)
            }
        };
        candidates.push((credit, expires));
    }
    candidates.sort_by(|(left, le), (right, re)| {
        le.is_none()
            .cmp(&re.is_none())
            .then_with(|| le.cmp(re))
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates
        .into_iter()
        .find(|(credit, _)| requested_id.is_none_or(|id| id == credit.id))
        .map(|(credit, _)| credit.id.clone())
        .ok_or_else(|| AccountCommandError::new(AccountErrorKind::NoResetCredit))
}
