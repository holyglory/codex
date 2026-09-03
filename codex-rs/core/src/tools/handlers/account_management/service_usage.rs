use chrono::Utc;
use codex_protocol::protocol::RateLimitReachedType;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use futures::StreamExt;
use serde::Serialize;

use super::AccountListOutput;
use super::AccountOutput;
use crate::tools::context::ToolInvocation;

const MAX_SERVICE_BUCKETS: usize = 8;
const MAX_SAFE_BUCKET_NAME_BYTES: usize = 32;
const SERVICE_USAGE_CONCURRENCY: usize = 3;
const SERVICE_USAGE_BUCKET_FIELDS: &[&str] = &[
    "bucket",
    "primaryUsedPercent",
    "primaryResetsAt",
    "secondaryUsedPercent",
    "secondaryResetsAt",
    "hasCredits",
    "unlimitedCredits",
    "individualRemainingPercent",
    "individualResetsAt",
    "spendControlReached",
    "reachedType",
];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ServiceUsageOutput {
    pub(super) state: &'static str,
    pub(super) reason: Option<&'static str>,
    pub(super) observed_at: Option<i64>,
    pub(super) buckets: Vec<ServiceUsageBucket>,
}

#[derive(Clone, Serialize)]
pub(super) struct ServiceUsageBucket(
    pub(super) String,
    pub(super) Option<f64>,
    pub(super) Option<i64>,
    pub(super) Option<f64>,
    pub(super) Option<i64>,
    pub(super) Option<bool>,
    pub(super) Option<bool>,
    pub(super) Option<i32>,
    pub(super) Option<i64>,
    pub(super) Option<bool>,
    pub(super) Option<&'static str>,
);

pub(super) async fn add_service_usage(
    invocation: &ToolInvocation,
    mut output: AccountListOutput,
) -> AccountListOutput {
    output.service_usage_bucket_fields = Some(SERVICE_USAGE_BUCKET_FIELDS);
    let Some(router) = invocation.session.services.profile_auth_router.clone() else {
        for account in &mut output.accounts {
            account.service_usage = Some(unavailable_usage("routingUnavailable"));
        }
        return output;
    };
    let config = invocation.turn.config.clone();
    let refreshed = futures::stream::iter(output.accounts.into_iter().map(|mut account| {
        let router = router.clone();
        let config = config.clone();
        async move {
            account.service_usage = Some(refresh_service_usage(&router, &config, &account).await);
            account
        }
    }))
    .buffered(SERVICE_USAGE_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    output.accounts = refreshed;
    output
}

async fn refresh_service_usage(
    router: &codex_login::SharedProfileAuthRouter,
    config: &crate::config::Config,
    account: &AccountOutput,
) -> ServiceUsageOutput {
    if !account.enabled {
        return unavailable_usage("disabled");
    }
    if !account.authenticated {
        return unavailable_usage("notAuthenticated");
    }
    if account.auth_mode != "chatgpt" {
        return unavailable_usage("notEligibleForAutomaticSelection");
    }
    let lease = match router.lease_for_account_id(&account.account_id).await {
        Ok(lease) => lease,
        Err(_) => return unavailable_usage("credentialUnavailable"),
    };
    let account_id = lease.account_id().cloned();
    let Some(snapshots) = codex_backend_client::fetch_profile_rate_limits(
        lease,
        config.chatgpt_base_url.clone(),
        config.http_client_factory(),
    )
    .await
    else {
        return unavailable_usage("requestFailed");
    };
    if snapshots.len() > MAX_SERVICE_BUCKETS || snapshots.iter().any(invalid_snapshot) {
        return unavailable_usage("invalidResponse");
    }
    let observed_at = Utc::now().timestamp();
    if let Some(account_id) = account_id {
        let _ = router.record_rate_limits(account_id, observed_at, snapshots.clone());
    }
    ServiceUsageOutput {
        state: "observed",
        reason: None,
        observed_at: Some(observed_at),
        buckets: snapshots
            .iter()
            .enumerate()
            .map(|(index, snapshot)| service_bucket(index, snapshot))
            .collect(),
    }
}

fn service_bucket(index: usize, snapshot: &RateLimitSnapshot) -> ServiceUsageBucket {
    ServiceUsageBucket(
        safe_bucket_name(snapshot, index),
        snapshot.primary.as_ref().map(|window| window.used_percent),
        snapshot
            .primary
            .as_ref()
            .and_then(|window| window.resets_at),
        snapshot
            .secondary
            .as_ref()
            .map(|window| window.used_percent),
        snapshot
            .secondary
            .as_ref()
            .and_then(|window| window.resets_at),
        snapshot.credits.as_ref().map(|credits| credits.has_credits),
        snapshot.credits.as_ref().map(|credits| credits.unlimited),
        snapshot
            .individual_limit
            .as_ref()
            .map(|limit| limit.remaining_percent),
        snapshot
            .individual_limit
            .as_ref()
            .map(|limit| limit.resets_at),
        snapshot.spend_control_reached,
        snapshot.rate_limit_reached_type.map(reached_type_label),
    )
}

fn safe_bucket_name(snapshot: &RateLimitSnapshot, index: usize) -> String {
    snapshot
        .limit_name
        .as_deref()
        .or(snapshot.limit_id.as_deref())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_SAFE_BUCKET_NAME_BYTES
                && value.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
                })
        })
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("bucket{index}"))
}

fn invalid_snapshot(snapshot: &RateLimitSnapshot) -> bool {
    snapshot.primary.as_ref().is_some_and(invalid_window)
        || snapshot.secondary.as_ref().is_some_and(invalid_window)
}

fn invalid_window(window: &RateLimitWindow) -> bool {
    !window.used_percent.is_finite() || !(0.0..=100.0).contains(&window.used_percent)
}

fn unavailable_usage(reason: &'static str) -> ServiceUsageOutput {
    ServiceUsageOutput {
        state: "unavailable",
        reason: Some(reason),
        observed_at: None,
        buckets: Vec::new(),
    }
}

fn reached_type_label(reached_type: RateLimitReachedType) -> &'static str {
    match reached_type {
        RateLimitReachedType::RateLimitReached => "rateLimitReached",
        RateLimitReachedType::WorkspaceOwnerCreditsDepleted => "workspaceOwnerCreditsDepleted",
        RateLimitReachedType::WorkspaceMemberCreditsDepleted => "workspaceMemberCreditsDepleted",
        RateLimitReachedType::WorkspaceOwnerUsageLimitReached => "workspaceOwnerUsageLimitReached",
        RateLimitReachedType::WorkspaceMemberUsageLimitReached => {
            "workspaceMemberUsageLimitReached"
        }
    }
}

#[cfg(test)]
pub(super) fn maximal_service_usage() -> ServiceUsageOutput {
    ServiceUsageOutput {
        state: "observed",
        reason: None,
        observed_at: Some(i64::MAX),
        buckets: (0..MAX_SERVICE_BUCKETS)
            .map(|index| {
                ServiceUsageBucket(
                    format!("b{index:0>31}"),
                    Some(100.0),
                    Some(i64::MAX),
                    Some(100.0),
                    Some(i64::MAX),
                    Some(true),
                    Some(true),
                    Some(100),
                    Some(i64::MAX),
                    Some(true),
                    Some("workspaceMemberUsageLimitReached"),
                )
            })
            .collect(),
    }
}

#[cfg(test)]
pub(super) fn bucket_fields() -> &'static [&'static str] {
    SERVICE_USAGE_BUCKET_FIELDS
}
