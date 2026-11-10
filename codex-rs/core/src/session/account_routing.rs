use super::Session;
use super::turn_context::TurnContext;
use super::turn_context::TurnContextBuildMode;
use crate::client::ModelClient;
use crate::config::Config;
use codex_account_registry::AccountId;
use codex_login::AuthManagerLease;
use codex_login::RouterExternalAuthState;
use codex_login::SharedProfileAuthRouter;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::error::UsageLimitReachedError;
use codex_protocol::protocol::RateLimitReachedType;
use codex_protocol::protocol::RateLimitSnapshot;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct TurnAuth {
    pub(super) lease: Option<AuthManagerLease>,
    error: Option<String>,
}

struct TurnModelClient(ModelClient);

#[derive(Default)]
struct RejectedProfiles(std::sync::Mutex<HashSet<AccountId>>);

/// Identify explicit provider credentials that must retain their upstream owner.
pub fn router_external_auth_state(config: &Config) -> RouterExternalAuthState {
    let provider = &config.model_provider;
    let is_auth_header = |name: &str| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization" | "api-key" | "x-api-key" | "x-goog-api-key"
        )
    };
    let direct_header = provider
        .http_headers
        .as_ref()
        .is_some_and(|headers| headers.keys().any(|name| is_auth_header(name)));
    let environment_header = provider.env_http_headers.as_ref().is_some_and(|headers| {
        headers.iter().any(|(name, variable)| {
            is_auth_header(name)
                && std::env::var_os(variable).is_some_and(|value| !value.is_empty())
        })
    });
    let environment_key = provider
        .env_key
        .as_ref()
        .is_some_and(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    RouterExternalAuthState {
        header_or_host: !provider.requires_openai_auth
            || provider.experimental_bearer_token.is_some()
            || provider.auth.is_some()
            || provider.aws.is_some()
            || direct_header
            || environment_header
            || environment_key,
        ..Default::default()
    }
}

impl Session {
    pub(super) async fn record_rate_limits_for_turn(
        &self,
        turn_context: &TurnContext,
        new_rate_limits: RateLimitSnapshot,
    ) {
        // SSE emits a default snapshot even when the response has no limit headers.
        // Missing headers must neither erase a probe nor refresh its observation time.
        let has_capacity_observation = new_rate_limits.primary.is_some()
            || new_rate_limits.secondary.is_some()
            || new_rate_limits.credits.is_some()
            || new_rate_limits.individual_limit.is_some()
            || new_rate_limits.spend_control_reached.is_some()
            || new_rate_limits.rate_limit_reached_type.is_some();
        if has_capacity_observation
            && let Some(router) = self
                .services
                .thread_extension_data
                .get::<SharedProfileAuthRouter>()
            && let Some(lease) = turn_context.extension_data.get::<AuthManagerLease>()
            && let Some(id) = lease.account_id()
        {
            let _ = router.observe_rate_limits(
                id.clone(),
                chrono::Utc::now().timestamp(),
                vec![new_rate_limits.clone()],
            );
        }
        self.record_rate_limits_info(new_rate_limits).await;
    }

    pub(super) fn record_usage_limit_rejection(
        &self,
        turn: &TurnContext,
        error: &UsageLimitReachedError,
    ) {
        let Some(lease) = turn.extension_data.get::<AuthManagerLease>() else {
            return;
        };
        let Some(account_id) = lease.account_id() else {
            return;
        };
        let Some(router) = self
            .services
            .thread_extension_data
            .get::<SharedProfileAuthRouter>()
        else {
            return;
        };
        let mut snapshot = error
            .rate_limits
            .as_deref()
            .cloned()
            .unwrap_or(RateLimitSnapshot {
                limit_id: None,
                limit_name: None,
                normal_model_slug: None,
                primary: None,
                secondary: None,
                credits: None,
                individual_limit: None,
                spend_control_reached: None,
                plan_type: error.plan_type.clone().map(Into::into),
                rate_limit_reached_type: None,
            });
        snapshot.rate_limit_reached_type = Some(
            error
                .rate_limit_reached_type
                .or(snapshot.rate_limit_reached_type)
                .unwrap_or(RateLimitReachedType::RateLimitReached),
        );
        // A rejection supersedes a preflight observation, including one from this same second.
        // Do not fabricate percentages or refresh unrelated buckets from the rejected request.
        let _ = router.record_usage_limit_rejection(
            account_id.clone(),
            chrono::Utc::now().timestamp(),
            snapshot,
        );
    }

    pub(super) async fn failover_turn_auth(&self, turn: &TurnContext) -> Option<TurnContext> {
        let lease = turn.extension_data.get::<AuthManagerLease>()?;
        let account_id = lease.account_id()?;
        let rejected = {
            let rejected = turn.extension_data.get_or_init(RejectedProfiles::default);
            let mut rejected = rejected
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !rejected.insert(account_id.clone()) {
                return None;
            }
            rejected.clone()
        };
        let router = self
            .services
            .thread_extension_data
            .get::<SharedProfileAuthRouter>()?;
        let mut external = router_external_auth_state(&turn.config);
        external.header_or_host |= self.services.model_client.runtime_auth_is_owned();
        let base_url = turn.config.chatgpt_base_url.clone();
        let factory = turn.config.http_client_factory();
        let lease = router
            .lease_for_usage_limit_failover_with_external_auth_and_probe(
                external,
                &rejected,
                move |lease| {
                    codex_backend_client::fetch_profile_rate_limits(
                        lease,
                        base_url.clone(),
                        factory.clone(),
                    )
                },
            )
            .await
            .ok()??;
        let turn = turn.with_auth_manager(Arc::clone(lease.auth_manager()));
        turn.install_turn_auth(
            self,
            TurnAuth {
                lease: Some(AuthManagerLease::profile(lease)),
                error: None,
            },
        );
        turn.retarget_usage_capture(self);
        Some(turn)
    }

    pub(super) async fn publish_failover_turn_auth(
        &self,
        previous: &Arc<TurnContext>,
        next: &Arc<TurnContext>,
    ) -> Result<()> {
        let mut active = self.active_turn.lock().await;
        let task = active
            .as_mut()
            .and_then(|turn| turn.task.as_mut())
            .ok_or(CodexErr::TurnAborted)?;
        if !Arc::ptr_eq(&task.turn_context, previous)
            || previous.sub_id != next.sub_id
            || task.cancellation_token.is_cancelled()
        {
            return Err(CodexErr::TurnAborted);
        }
        task.turn_context = Arc::clone(next);
        Ok(())
    }

    pub(super) async fn select_turn_auth(
        &self,
        config: &Config,
        mode: TurnContextBuildMode,
    ) -> TurnAuth {
        let Some(router) = self
            .services
            .thread_extension_data
            .get::<SharedProfileAuthRouter>()
        else {
            return TurnAuth::default();
        };
        if matches!(mode, TurnContextBuildMode::InjectItems) {
            return TurnAuth::default();
        }
        let base_url = config.chatgpt_base_url.clone();
        let factory = config.http_client_factory();
        let probe = move |lease| {
            codex_backend_client::fetch_profile_rate_limits(
                lease,
                base_url.clone(),
                factory.clone(),
            )
        };
        let mut external = router_external_auth_state(config);
        external.header_or_host |= self.services.model_client.runtime_auth_is_owned();
        let result = match mode {
            TurnContextBuildMode::StartupPrewarm => {
                router
                    .lease_for_startup_prewarm_with_external_auth_and_probe(external, probe)
                    .await
            }
            TurnContextBuildMode::Full | TurnContextBuildMode::InjectItems => {
                router
                    .lease_for_turn_with_external_auth_and_probe(external, probe)
                    .await
            }
        };
        match result {
            Ok(lease) => TurnAuth {
                lease: lease.map(AuthManagerLease::profile),
                error: None,
            },
            Err(error) => TurnAuth {
                lease: None,
                error: Some(error.safe_message()),
            },
        }
    }
}

impl TurnContext {
    pub(super) fn install_turn_auth(&self, session: &Session, mut auth: TurnAuth) {
        if let Some(lease) = auth.lease.as_ref() {
            let lease = Arc::new(lease.clone());
            match session
                .services
                .model_client
                .for_auth_lease(Arc::clone(&lease))
            {
                Ok(client) => {
                    self.extension_data.insert(TurnModelClient(client));
                    self.extension_data.insert_arc(lease);
                }
                Err(_) => auth.error = Some("account profile authentication is unavailable".into()),
            }
        }
        self.extension_data.insert(auth);
    }

    pub(crate) fn model_client(&self, session: &Session) -> Result<ModelClient> {
        if let Some(auth) = self.extension_data.get::<TurnAuth>()
            && let Some(error) = &auth.error
        {
            return Err(CodexErr::Fatal(error.clone()));
        }
        if let Some(client) = self
            .extension_data
            .get::<crate::usage_capture::CapturedModelClient>()
        {
            return Ok(client.0.clone());
        }
        Ok(self.extension_data.get::<TurnModelClient>().map_or_else(
            || session.services.model_client.clone(),
            |client| client.0.clone(),
        ))
    }

    pub(crate) fn auth_owner_generation(&self) -> Option<u64> {
        self.auth_manager.as_ref().map(|manager| {
            manager
                .auth_change_state_receiver()
                .borrow()
                .owner_generation
        })
    }

    pub(crate) fn local_profile_id(&self) -> Option<String> {
        self.auth_manager
            .as_ref()
            .and_then(|manager| manager.local_profile_id())
            .map(ToString::to_string)
    }
}

#[cfg(test)]
#[path = "account_routing_tests.rs"]
mod tests;
