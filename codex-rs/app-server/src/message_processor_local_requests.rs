//! Construct local profile and usage RPC futures outside the shared dispatcher poll frame.

use super::MessageProcessor;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use futures::future::BoxFuture;
use std::sync::Arc;

impl MessageProcessor {
    // Keep this constructor synchronous: boxing an already-constructed async handler in the
    // dispatcher would still reserve its construction temporaries on every unrelated RPC.
    pub(super) fn handle_local_client_request(
        &self,
        request: ClientRequest,
    ) -> BoxFuture<'_, Result<Option<ClientResponsePayload>, JSONRPCErrorError>> {
        Box::pin(async move {
            match request {
                ClientRequest::LocalUsageSummary { params, .. } => {
                    crate::request_processors::local_usage_processor::summary(
                        self.local_usage_home.clone(),
                        params,
                    )
                    .await
                    .map(|response| Some(response.into()))
                }
                ClientRequest::LocalUsageBindingTransition { params, .. } => {
                    crate::request_processors::local_usage_control::binding_transition(
                        Arc::clone(&self.local_usage_manager),
                        params,
                    )
                    .await
                    .map(|response| Some(response.into()))
                }
                ClientRequest::LocalUsageBindingStatus { params, .. } => {
                    crate::request_processors::local_usage_control::binding_status(
                        Arc::clone(&self.local_usage_manager),
                        params,
                    )
                    .await
                    .map(|response| Some(response.into()))
                }
                ClientRequest::LocalUsageRepositoryIdentity { params, .. } => {
                    crate::request_processors::local_usage_processor::repository_identity(params)
                        .await
                        .map(|response| Some(response.into()))
                }
                ClientRequest::AccountProfileList { params, .. } => {
                    self.account_profile_processor.list(params).await
                }
                ClientRequest::AccountProfileRead { params, .. } => {
                    self.account_profile_processor.read(params).await
                }
                ClientRequest::AccountProfileActivate { params, .. } => {
                    self.account_profile_processor.activate(params).await
                }
                ClientRequest::AccountProfileUpdate { params, .. } => {
                    self.account_profile_processor.update(params).await
                }
                ClientRequest::AccountProfileRemove { params, .. } => {
                    self.account_profile_processor.remove(params).await
                }
                ClientRequest::AccountProfileLoginStart { params, .. } => {
                    self.account_profile_processor.login_start(params).await
                }
                ClientRequest::AccountProfileLoginCancel { params, .. } => {
                    self.account_profile_processor.login_cancel(params).await
                }
                ClientRequest::AccountProfileRateLimitRead { params, .. } => {
                    self.account_profile_processor.rate_limits(params).await
                }
                ClientRequest::AccountAutoSelectionRead { params, .. } => {
                    self.account_profile_processor.auto_read(params).await
                }
                ClientRequest::AccountAutoSelectionWrite { params, .. } => {
                    self.account_profile_processor.auto_write(params).await
                }
                _ => unreachable!("only local profile and usage requests reach this handler"),
            }
        })
    }
}
