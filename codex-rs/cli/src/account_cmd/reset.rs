use std::time::Duration;

use chrono::DateTime;
use codex_backend_client::Client as BackendClient;
use codex_backend_client::ConsumeRateLimitResetCreditCode;
use codex_backend_client::RateLimitResetCreditDetails;
use codex_core::config::Config;
use codex_login::ProfileAuthRouter;
use serde::Serialize;
use uuid::Uuid;

use super::AccountCommandError;
use super::AccountErrorKind;
use super::RegistryStore;
use super::ResetArgs;
use super::limits::reset_label;
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
    credit_id: Option<String>,
    request_id: String,
    outcome: &'static str,
    windows_reset: i64,
    refreshed_banked_resets: Option<RefreshedBankedResets>,
    refresh_error: Option<&'static str>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RefreshedBankedResets {
    available_count: i64,
    soonest_expires_at: Option<i64>,
}

pub(super) async fn run(
    config: &Config,
    store: &RegistryStore,
    args: ResetArgs,
    json: bool,
) -> Result<(), AccountCommandError> {
    if args.request_id.is_some() && args.credit_id.is_none() {
        return Err(AccountCommandError::new(AccountErrorKind::InvalidInput));
    }
    let registry = read_registry(store)?;
    let account = resolve_account(&registry, &args.account)?;
    require_enabled(account)?;
    let router = ProfileAuthRouter::open_for_management(config.auth_config())
        .await
        .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?;
    let lease = router
        .lease_for_account(&account.id)
        .await
        .map_err(|_| AccountCommandError::new(AccountErrorKind::CredentialStore))?;
    let Some(auth) = lease.auth_manager().auth().await else {
        return Err(AccountCommandError::new(AccountErrorKind::NotAuthenticated));
    };
    if !auth.uses_codex_backend() {
        return Err(AccountCommandError::new(AccountErrorKind::InvalidInput));
    }
    let client = BackendClient::from_auth(
        config.chatgpt_base_url.clone(),
        &auth,
        config.http_client_factory(),
    );
    let details = tokio::time::timeout(
        RESET_REQUEST_TIMEOUT,
        client.list_rate_limit_reset_credits(),
    )
    .await
    .map_err(|_| AccountCommandError::new(AccountErrorKind::ResetFailed))?
    .map_err(|_| AccountCommandError::new(AccountErrorKind::ResetFailed))?;
    let credit_id = select_credit(&details.credits, args.credit_id.as_deref())?;
    let request_id = args
        .request_id
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let response = tokio::time::timeout(
        RESET_REQUEST_TIMEOUT,
        client.consume_rate_limit_reset_credit_by_id(&request_id, &credit_id),
    )
    .await
    .map_err(|_| AccountCommandError::new(AccountErrorKind::ResetUncertain))?
    .map_err(|_| AccountCommandError::new(AccountErrorKind::ResetUncertain))?;
    let outcome = match response.code {
        ConsumeRateLimitResetCreditCode::Reset => "reset",
        ConsumeRateLimitResetCreditCode::NothingToReset => "nothingToReset",
        ConsumeRateLimitResetCreditCode::NoCredit => "noCredit",
        ConsumeRateLimitResetCreditCode::AlreadyRedeemed => "alreadyRedeemed",
    };
    let refresh = refresh(&client).await;
    let report = ResetJson {
        schema_version: super::JSON_SCHEMA_VERSION,
        account: account.alias.to_string(),
        credit_id: Some(credit_id),
        request_id,
        outcome,
        windows_reset: response.windows_reset,
        refreshed_banked_resets: refresh.as_ref().ok().and_then(|value| value.clone()),
        refresh_error: refresh.as_ref().err().copied(),
    };
    if json {
        print_json(&report)?;
    } else {
        println!("Account: {}", report.account);
        println!("Outcome: {}", report.outcome);
        println!(
            "Credit: {}",
            report.credit_id.as_deref().unwrap_or("unknown")
        );
        println!("Request: {}", report.request_id);
        println!("Windows reset: {}", report.windows_reset);
        if let Some(resets) = &report.refreshed_banked_resets {
            println!("Banked resets remaining: {}", resets.available_count);
            println!("Soonest expiry: {}", reset_label(resets.soonest_expires_at));
        } else if let Some(error) = report.refresh_error {
            println!("Refresh: {error}");
        }
    }
    if matches!(
        response.code,
        ConsumeRateLimitResetCreditCode::Reset
            | ConsumeRateLimitResetCreditCode::NothingToReset
            | ConsumeRateLimitResetCreditCode::AlreadyRedeemed
    ) {
        Ok(())
    } else {
        Err(AccountCommandError::new(AccountErrorKind::NoResetCredit))
    }
}

fn select_credit(
    credits: &[RateLimitResetCreditDetails],
    requested_id: Option<&str>,
) -> Result<String, AccountCommandError> {
    let mut candidates = credits
        .iter()
        .filter(|credit| credit.status == "available" && credit.reset_type == "codex_rate_limits")
        .filter_map(|credit| {
            let expires_at = match credit.expires_at.as_deref() {
                None => None,
                Some(value) => Some(DateTime::parse_from_rfc3339(value).ok()?.timestamp()),
            };
            Some((credit, expires_at))
        })
        .collect::<Vec<_>>();
    if let Some(requested_id) = requested_id {
        return candidates
            .into_iter()
            .find(|(credit, _)| credit.id == requested_id)
            .map(|(credit, _)| credit.id.clone())
            .ok_or_else(|| AccountCommandError::new(AccountErrorKind::NoResetCredit));
    }
    candidates.sort_by(|(left, left_expiry), (right, right_expiry)| {
        left_expiry
            .unwrap_or(i64::MAX)
            .cmp(&right_expiry.unwrap_or(i64::MAX))
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates
        .first()
        .map(|(credit, _)| credit.id.clone())
        .ok_or_else(|| AccountCommandError::new(AccountErrorKind::NoResetCredit))
}

async fn refresh(client: &BackendClient) -> Result<Option<RefreshedBankedResets>, &'static str> {
    tokio::time::timeout(
        RESET_REQUEST_TIMEOUT,
        client.get_rate_limits_with_reset_credits(),
    )
    .await
    .map_err(|_| "refreshTimedOut")?
    .map_err(|_| "refreshFailed")?;
    let details = tokio::time::timeout(
        RESET_REQUEST_TIMEOUT,
        client.list_rate_limit_reset_credits(),
    )
    .await
    .map_err(|_| "refreshTimedOut")?
    .map_err(|_| "refreshFailed")?;
    let soonest = details
        .credits
        .iter()
        .filter(|credit| credit.status == "available")
        .filter_map(|credit| {
            credit
                .expires_at
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.timestamp())
        })
        .min();
    Ok(Some(RefreshedBankedResets {
        available_count: details.available_count,
        soonest_expires_at: soonest,
    }))
}
