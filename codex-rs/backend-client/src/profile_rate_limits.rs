use std::time::Duration;

use codex_http_client::HttpClientFactory;
use codex_login::AuthManagerLease;
use codex_protocol::auth::AuthMode;
use codex_protocol::protocol::RateLimitSnapshot;

use crate::Client;

const RATE_LIMIT_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Fetches current service limits for one leased managed-ChatGPT profile.
///
/// The lease remains held throughout the request so profile removal cannot race
/// credential use. Failures are intentionally collapsed to `None`; the caller
/// re-runs fail-closed account selection with whatever fresh observations were
/// established by successful probes.
pub async fn fetch_profile_rate_limits(
    lease: AuthManagerLease,
    chatgpt_base_url: String,
    http_client_factory: HttpClientFactory,
) -> Option<Vec<RateLimitSnapshot>> {
    let auth = lease.auth_manager().auth().await?;
    if auth.api_auth_mode() != AuthMode::Chatgpt {
        return None;
    }
    let client = Client::from_auth(chatgpt_base_url, &auth, http_client_factory);
    let mut snapshots =
        tokio::time::timeout(RATE_LIMIT_FETCH_TIMEOUT, client.get_rate_limits_many())
            .await
            .ok()?
            .ok()?;
    if snapshots.is_empty() {
        return None;
    }
    for snapshot in &mut snapshots {
        if snapshot.limit_id.is_none() {
            snapshot.limit_id = Some("codex".to_string());
        }
    }
    Some(snapshots)
}
