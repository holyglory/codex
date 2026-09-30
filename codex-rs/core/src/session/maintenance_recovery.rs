//! Restores the active turn's choices without changing the thread's future defaults.
use super::session::Session;
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
    pub async fn maintenance_stopped_turn(&self) -> Option<String> {
        if !self.session.is_interrupted() {
            return None;
        }
        self.session.state.lock().await.last_started_turn_id.clone()
    }

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
        let current = turn.next_step_settings.load_full();
        let mut selected = current.selected().clone();
        selected.collaboration_mode = saved.collaboration_mode;
        selected.reasoning_summary = saved.summary;
        selected.service_tier = saved.service_tier;
        selected.personality = saved.personality;
        selected
            .approval_policy
            .set(saved.approval_policy)
            .map_err(|error| codex_protocol::error::CodexErr::InvalidRequest(error.to_string()))?;
        selected.approvals_reviewer = saved.approvals_reviewer;
        let overrides = self
            .state
            .lock()
            .await
            .session_configuration
            .model_info_overrides
            .clone();
        let model_info = selected
            .resolve_model_info(self.services.models_manager.as_ref(), &overrides)
            .await;
        let mut settings = super::step_settings::ResolvedStepSettings::new(
            Arc::new(selected),
            Arc::new(model_info),
            self.features.enabled(codex_features::Feature::FastMode),
        );
        let environments = self.services.turn_environments.selections();
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

impl Session {
    /// Runs through the ordered submission loop, so a newer user start wins over
    /// a delayed Stop belonging to the previous server generation.
    pub(super) async fn apply_maintenance_stop(
        self: &Arc<Self>,
        turn_id: String,
    ) -> codex_protocol::error::Result<()> {
        use codex_protocol::protocol::Event;
        use codex_protocol::protocol::EventMsg;
        use codex_protocol::protocol::TurnAbortReason;
        use codex_protocol::protocol::TurnAbortedEvent;
        use codex_rollout::RolloutItem;
        if self
            .state
            .lock()
            .await
            .last_started_turn_id
            .as_ref()
            .is_some_and(|id| id != &turn_id)
        {
            return Ok(());
        }
        if let Some(path) = self
            .current_rollout_path()
            .await
            .map_err(|error| codex_protocol::error::CodexErr::Fatal(error.to_string()))?
        {
            let (history, _, _) = codex_rollout::RolloutRecorder::load_rollout_items(&path)
                .await
                .map_err(|error| codex_protocol::error::CodexErr::Fatal(error.to_string()))?;
            if history
                .iter()
                .rev()
                .find_map(|item| match item {
                    RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => Some(&event.turn_id),
                    _ => None,
                })
                .is_some_and(|latest| latest != &turn_id)
            {
                return Ok(());
            }
        }
        let active = self.active_turn.lock().await;
        if let Some(task) = active.as_ref().and_then(|active| active.task.as_ref()) {
            let same = task.turn_context.sub_id == turn_id;
            drop(active);
            if same {
                self.interrupt_task().await;
            }
            return Ok(());
        }
        drop(active);
        self.stop_subscription_work().await;
        if self.state_db().is_some()
            && self
                .services
                .thread_extension_data
                .get::<codex_event_subscriptions::SubscriptionRunState>()
                .is_some_and(|run| run.stop_pending.load(std::sync::atomic::Ordering::Acquire))
        {
            return Err(codex_protocol::error::CodexErr::Fatal(
                "failed to persist suspended wake permissions".into(),
            ));
        }
        self.emit_turn_abort_lifecycle(
            TurnAbortReason::Interrupted,
            &codex_extension_api::ExtensionData::new(turn_id.clone()),
        )
        .await;
        self.send_event_raw(Event {
            id: turn_id.clone(),
            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(turn_id),
                reason: TurnAbortReason::Interrupted,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            }),
        })
        .await;
        self.flush_rollout()
            .await
            .map_err(|error| codex_protocol::error::CodexErr::Fatal(error.to_string()))
    }
}
