use std::time::Duration;

use codex_http_client::HttpClientFactory;
use codex_login::AuthManagerLease;
use codex_protocol::auth::AuthMode;
use codex_protocol::protocol::RateLimitSnapshot;

use crate::Client;

/// Read current capacity while retaining the selected managed profile's lease.
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
    let snapshots = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 10),
        client.get_rate_limits_many(),
    )
    .await
    .ok()?
    .ok()?;
    (!snapshots.is_empty()).then_some(snapshots)
}
