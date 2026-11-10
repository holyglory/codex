use super::*;

impl TurnContext {
    /// Forks authentication without changing contexts retained by earlier requests or tools.
    pub(crate) fn with_auth_manager(&self, auth_manager: Arc<AuthManager>) -> Self {
        let extension_data = Arc::new(self.extension_data.as_ref().clone());
        extension_data.remove::<codex_api::ResponseId>();
        extension_data.remove::<crate::guardian::ParentResponse>();
        Self {
            sub_id: self.sub_id.clone(),
            trace_id: self.trace_id.clone(),
            realtime_active: self.realtime_active,
            code_mode_available: self.code_mode_available,
            config: Arc::clone(&self.config),
            configured_token_budget: self.configured_token_budget.clone(),
            use_model_token_budget_defaults: self.use_model_token_budget_defaults,
            auth_manager: Some(Arc::clone(&auth_manager)),
            initial_settings: Arc::clone(&self.initial_settings),
            disabled_plugin_ids: self.disabled_plugin_ids.clone(),
            active_host_plugin_identities: self.active_host_plugin_identities.clone(),
            next_step_settings: Arc::clone(&self.next_step_settings),
            session_telemetry: self.session_telemetry.clone(),
            provider: codex_model_provider::create_model_provider(
                self.provider.info().clone(),
                Some(auth_manager),
            ),
            session_source: self.session_source.clone(),
            history_mode: self.history_mode,
            parent_thread_id: self.parent_thread_id,
            originator: self.originator.clone(),
            initial_environments: self.initial_environments.clone(),
            #[allow(deprecated)]
            cwd: self.cwd.clone(),
            current_date: self.current_date.clone(),
            timezone: self.timezone.clone(),
            app_server_client_name: self.app_server_client_name.clone(),
            developer_instructions: self.developer_instructions.clone(),
            multi_agent_version: self.multi_agent_version,
            network: self.network.clone(),
            windows_sandbox_level: self.windows_sandbox_level,
            available_models: self.available_models.clone(),
            final_output_json_schema: self.final_output_json_schema.clone(),
            turn_metadata_state: Arc::clone(&self.turn_metadata_state),
            extension_data,
            turn_timing_state: Arc::clone(&self.turn_timing_state),
            terminal_error: Arc::clone(&self.terminal_error),
            session_grants: Arc::clone(&self.session_grants),
            turn_grants: Arc::clone(&self.turn_grants),
            server_model_warning_emitted: AtomicBool::new(
                self.server_model_warning_emitted.load(Ordering::Relaxed),
            ),
            model_verification_emitted: AtomicBool::new(
                self.model_verification_emitted.load(Ordering::Relaxed),
            ),
            cyber_access_program: self.cyber_access_program,
        }
    }
}
