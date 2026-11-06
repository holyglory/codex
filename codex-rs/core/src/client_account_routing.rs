use super::*;
use codex_login::AuthManagerLease;

impl ModelClient {
    pub(crate) fn runtime_auth_is_owned(&self) -> bool {
        self.runtime_auth.is_some()
    }

    /// Retain one profile owner for every request and connection in this client.
    pub(crate) fn for_auth_lease(&self, lease: Arc<AuthManagerLease>) -> Result<Self> {
        let manager = Arc::clone(lease.auth_manager());
        if self
            .auth_manager()
            .is_some_and(|current| Arc::ptr_eq(&current, &manager))
        {
            return Ok(Self {
                _auth_lease: Some(lease),
                ..self.clone()
            });
        }
        if self.runtime_auth.is_some() {
            return Err(std::io::Error::other(
                "account profile conflicts with runtime-owned authentication",
            )
            .into());
        }
        let mut cached = self
            .state
            .cached_profile_client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(client) = cached.as_ref()
            && client
                .auth_manager()
                .is_some_and(|current| Arc::ptr_eq(&current, &manager))
        {
            return Ok(Self {
                _auth_lease: Some(lease),
                ..client.clone()
            });
        }
        let mut client = Self::new(
            Some(manager),
            self.agent_identity_policy,
            self.state.thread_id,
            self.state.provider.info().clone(),
            self.state.session_source.clone(),
            self.state.originator.clone(),
            self.state.model_verbosity,
            self.state.content_item_kinds_enabled,
            self.state.reasoning_effort_override_enabled,
            self.state.enable_request_compression,
            self.state.include_timing_metrics,
            self.state.beta_features_header.clone(),
            self.state.concurrent_reasoning_summaries_enabled,
            self.state.output_token_replay_enabled,
            self.state.attestation_provider.clone(),
            self.http_client_factory.clone(),
            self.state.workspace_routing.clone(),
            self.request_contributors.clone(),
        );
        if let Some(state) = Arc::get_mut(&mut client.state) {
            state.disable_websockets = Arc::clone(&self.state.disable_websockets);
            state.agent_identity_session_fallback =
                self.state.agent_identity_session_fallback.clone();
            *state
                .last_inference_tools
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = self
                .state
                .last_inference_tools
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
        }
        let client = Self {
            prompt_cache_key_policy: self.prompt_cache_key_policy.clone(),
            session_id_override: self.session_id_override.clone(),
            codex_responses_headers: self.codex_responses_headers.clone(),
            event_sender: self.event_sender.clone(),
            restored_history: self.restored_history,
            executed_tool_calls: self.executed_tool_calls.clone(),
            api_key_cyber_access_programs: self.api_key_cyber_access_programs,
            ..client
        };
        // The bounded connection cache must not hold a profile-use lease between turns.
        *cached = Some(client.clone());
        Ok(Self {
            _auth_lease: Some(lease),
            ..client
        })
    }
}

impl ModelClientSession {
    pub(crate) fn auth_manager_matches(&self, manager: &Arc<AuthManager>) -> bool {
        self.client
            .auth_manager()
            .is_some_and(|current| Arc::ptr_eq(&current, manager))
    }
}
