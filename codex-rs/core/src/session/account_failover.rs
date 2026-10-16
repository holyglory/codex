use std::collections::HashSet;
use std::sync::Arc;

use codex_account_registry::AccountId;

use super::Session;
use super::turn_context::TurnContext;
use super::turn_context::router_external_auth_state;

impl Session {
    pub(crate) async fn failover_turn_context_after_usage_limit(
        &self,
        turn_context: &Arc<TurnContext>,
        excluded_account_ids: &HashSet<AccountId>,
    ) -> Option<Arc<TurnContext>> {
        let router = self.services.profile_auth_router.as_ref()?;
        let current_lease = turn_context.account_lease.as_ref()?;
        if !excluded_account_ids.contains(current_lease.account_id()) {
            return None;
        }
        let chatgpt_base_url = turn_context.config.chatgpt_base_url.clone();
        let http_client_factory = turn_context.config.http_client_factory();
        let probe = move |lease| {
            codex_backend_client::fetch_profile_rate_limits(
                lease,
                chatgpt_base_url.clone(),
                http_client_factory.clone(),
            )
        };
        let lease = match router
            .lease_for_usage_limit_failover_with_external_auth_and_probe(
                router_external_auth_state(&turn_context.config),
                excluded_account_ids,
                probe,
            )
            .await
        {
            Ok(Some(lease)) if !excluded_account_ids.contains(lease.account_id()) => lease,
            Ok(Some(_)) | Ok(None) => return None,
            Err(error) => {
                tracing::warn!(
                    "automatic account failover is unavailable: {}",
                    error.safe_message()
                );
                return None;
            }
        };
        turn_context.note_automatic_account_switch();
        Some(Arc::new(turn_context.with_account_lease(lease)))
    }
}
