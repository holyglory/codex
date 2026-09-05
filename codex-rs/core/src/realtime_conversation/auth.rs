use crate::client::ModelClient;
use crate::config::Config;
use crate::session::session::Session;
use crate::session::turn_context::router_external_auth_state;
use codex_login::AccountLease;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use std::sync::Arc;

pub(super) struct RealtimeAuth {
    pub(super) model_client: ModelClient,
    pub(super) account_lease: Option<AccountLease>,
}

impl RealtimeAuth {
    pub(super) async fn resolve(session: &Session, config: &Config) -> Result<Self> {
        let account_lease = match &session.services.profile_auth_router {
            Some(router) => {
                let chatgpt_base_url = config.chatgpt_base_url.clone();
                let http_client_factory = config.http_client_factory();
                router
                    .lease_for_turn_with_external_auth_and_probe(
                        router_external_auth_state(config),
                        move |lease| {
                            codex_backend_client::fetch_profile_rate_limits(
                                lease,
                                chatgpt_base_url.clone(),
                                http_client_factory.clone(),
                            )
                        },
                    )
                    .await
                    .map_err(|error| CodexErr::InvalidRequest(error.safe_message()))?
            }
            None => None,
        };
        let model_client = match &account_lease {
            Some(lease) => session
                .services
                .model_client
                .for_auth_manager(Arc::clone(lease.auth_manager())),
            None => session.services.model_client.clone(),
        };
        Ok(Self {
            model_client,
            account_lease,
        })
    }
}
