use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_account_registry::AccountId;
use codex_login::AccountLease;

use super::Session;
use super::turn_context::TurnContext;
use super::turn_context::router_external_auth_state;

impl TurnContext {
    pub(crate) fn automatic_account_switched(&self) -> bool {
        self.account_lease
            .as_ref()
            .is_some_and(AccountLease::automatic_switched)
            || self
                .automatic_account_switch_occurred
                .load(Ordering::Relaxed)
    }

    pub(crate) fn note_automatic_account_switch(&self) {
        self.automatic_account_switch_occurred
            .store(true, Ordering::Relaxed);
    }

    pub(crate) fn with_account_lease(&self, account_lease: AccountLease) -> Self {
        let auth_manager = Arc::clone(account_lease.auth_manager());
        let extension_data = Arc::new(self.extension_data.fork());
        extension_data.insert(codex_login::AuthManagerLease::profile(
            account_lease.clone(),
        ));
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
            current_settings: arc_swap::ArcSwap::from(self.current_settings.load_full()),
            account_lease: Some(account_lease),
            profile_auth_error: None,
            model_info: Arc::clone(&self.model_info),
            session_telemetry: self.session_telemetry.clone(),
            provider: codex_model_provider::create_model_provider(
                self.provider.info().clone(),
                Some(auth_manager),
            ),
            session_source: self.session_source.clone(),
            history_mode: self.history_mode,
            parent_thread_id: self.parent_thread_id,
            originator: self.originator.clone(),
            environments: self.environments.clone(),
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
            unified_exec_shell_mode: self.unified_exec_shell_mode.clone(),
            final_output_json_schema: self.final_output_json_schema.clone(),
            dynamic_tools: self.dynamic_tools.clone(),
            turn_metadata_state: Arc::clone(&self.turn_metadata_state),
            extension_data,
            turn_timing_state: Arc::clone(&self.turn_timing_state),
            terminal_error: Arc::clone(&self.terminal_error),
            automatic_account_switch_occurred: Arc::clone(&self.automatic_account_switch_occurred),
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
            Ok(Some(_)) => {
                tracing::warn!("automatic account failover selected an excluded profile");
                return None;
            }
            Ok(None) => return None,
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
