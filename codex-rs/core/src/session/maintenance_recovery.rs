//! Restores the active turn's choices without changing the thread's future defaults.
use super::session::Session;
use super::step_settings::StepSettingsUpdate;
use super::turn_context::TurnContext;
use codex_protocol::AgentPath;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::protocol::AskForApproval;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MaintenanceTurnContext {
    pub options: crate::TurnStartOptions,
    pub initiating_agent_path: Option<AgentPath>,
    pub collaboration_mode: CollaborationMode,
    pub summary: Option<ReasoningSummary>,
    pub service_tier: Option<String>,
    pub personality: Option<Personality>,
    pub approval_policy: AskForApproval,
    pub approvals_reviewer: ApprovalsReviewer,
    pub mcp_approvals_reviewer_override: Option<ApprovalsReviewer>,
}

pub(crate) struct PendingMaintenanceContext(
    pub(crate) Mutex<Option<(String, MaintenanceTurnContext)>>,
);

impl crate::CodexThread {
    pub async fn maintenance_turn_context(&self) -> Option<MaintenanceTurnContext> {
        let active = self.session.active_turn.lock().await;
        let task = active.as_ref()?.task.as_ref()?;
        if task.cancellation_token.is_cancelled() {
            return None;
        }
        let context = &task.turn_context;
        let settings = context.next_step_settings.load();
        let selected = settings.selected();
        let metadata = &context.turn_metadata_state;
        Some(MaintenanceTurnContext {
            options: crate::TurnStartOptions {
                turn_trigger: metadata.current_turn_trigger(),
                parent_turn_id: metadata.parent_turn_id(),
                root_turn_id: metadata.root_turn_id(),
                final_output_json_schema: context.final_output_json_schema.clone(),
                service_tier: selected.service_tier.clone(),
                cyber_access_program: context.cyber_access_program,
            },
            initiating_agent_path: metadata.initiating_agent_path().cloned(),
            collaboration_mode: selected.collaboration_mode.clone(),
            summary: selected.reasoning_summary,
            service_tier: selected.service_tier.clone(),
            personality: selected.personality,
            approval_policy: selected.approval_policy.value(),
            approvals_reviewer: selected.approvals_reviewer,
            mcp_approvals_reviewer_override: settings.mcp_approvals_reviewer_override,
        })
    }
}

impl Session {
    pub(super) async fn restore_maintenance_context(
        &self,
        turn: &Arc<TurnContext>,
    ) -> codex_protocol::error::Result<()> {
        let Some(pending) = self
            .services
            .thread_extension_data
            .get::<PendingMaintenanceContext>()
        else {
            return Ok(());
        };
        let saved = {
            let mut pending = pending
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.as_ref().is_none_or(|(id, _)| id != &turn.sub_id) {
                return Ok(());
            }
            pending.take().map(|(_, saved)| saved)
        };
        let Some(saved) = saved else {
            return Ok(());
        };
        let update = StepSettingsUpdate {
            collaboration_mode: Some(saved.collaboration_mode),
            reasoning_summary: saved.summary,
            service_tier: Some(saved.service_tier),
            personality: saved.personality,
            approval_policy: Some(saved.approval_policy),
            approvals_reviewer: Some(saved.approvals_reviewer),
            ..Default::default()
        };
        let environments = self.services.turn_environments.selections();
        let mut settings = self
            .prepare_step_settings_activation(
                turn,
                &turn.next_step_settings.load(),
                &update,
                &environments,
            )
            .await
            .map_err(codex_protocol::error::CodexErr::InvalidRequest)?;
        let state = self.state.lock().await;
        self.validate_active_step_settings(
            turn,
            &settings,
            &state.session_configuration,
            &environments,
        )
        .map_err(|error| codex_protocol::error::CodexErr::InvalidRequest(error.to_string()))?;
        settings.mcp_approvals_reviewer_override = saved.mcp_approvals_reviewer_override;
        turn.next_step_settings.store(Arc::new(settings));
        if let Some(path) = saved.initiating_agent_path {
            turn.turn_metadata_state.set_initiating_agent_path(path);
        }
        Ok(())
    }
}
