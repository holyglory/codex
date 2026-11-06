use super::Session;
use super::turn_context::TurnContext;
use super::turn_context::TurnContextBuildMode;
use crate::client::ModelClient;
use crate::config::Config;
use codex_login::AuthManagerLease;
use codex_login::RouterExternalAuthState;
use codex_login::SharedProfileAuthRouter;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct TurnAuth {
    pub(super) lease: Option<AuthManagerLease>,
    error: Option<String>,
}

struct TurnModelClient(ModelClient);

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
