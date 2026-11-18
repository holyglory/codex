//! Keeps required compaction and its continuing turn on the same authentication owner.

use super::session::Session;
use super::step_context::StepContext;
use super::turn_context::TurnContext;
use crate::responses_metadata::CompactionTurnMetadata;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionTrigger;
use codex_login::AuthManagerLease;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result;
use std::sync::Arc;

impl Session {
    pub(crate) async fn try_compaction_auth_failover(
        &self,
        turn: &Arc<TurnContext>,
        metadata: CompactionTurnMetadata,
        response_started: bool,
        error: &CodexErr,
    ) -> Result<Option<Arc<TurnContext>>> {
        let CodexErrorDetails::UsageLimitReached(error) = error.details() else {
            return Ok(None);
        };
        if let Some(limits) = error.rate_limits.as_deref() {
            self.update_rate_limits(turn, limits.clone()).await;
        }
        self.record_usage_limit_rejection(turn, error);
        if response_started
            || !matches!(metadata.trigger(), CompactionTrigger::Auto)
            || !matches!(
                metadata.phase(),
                CompactionPhase::PreTurn | CompactionPhase::MidTurn | CompactionPhase::PostTurn
            )
        {
            return Ok(None);
        }
        let previous = self
            .turn_context_for_sub_id(&turn.sub_id)
            .await
            .ok_or(CodexErr::TurnAborted)?;
        let Some(next) = Box::pin(self.failover_turn_auth(turn)).await else {
            return Ok(None);
        };
        let next = Arc::new(next);
        // A previous-model compactor has different settings from the running task.
        // Publish its new auth without replacing the task's model or live settings.
        let authority = if Arc::ptr_eq(&previous, turn) {
            Arc::clone(&next)
        } else {
            let lease = next
                .extension_data
                .get::<AuthManagerLease>()
                .ok_or(CodexErr::TurnAborted)?;
            Arc::new(previous.with_auth_lease(self, lease.as_ref().clone()))
        };
        self.publish_failover_turn_auth(&previous, &authority)
            .await?;
        Ok(Some(next))
    }

    pub(crate) async fn reconcile_turn_auth(&self, turn: &Arc<TurnContext>) -> Arc<TurnContext> {
        if let Some(current) = self.turn_context_for_sub_id(&turn.sub_id).await
            && current.local_profile_id() != turn.local_profile_id()
        {
            return current;
        }
        Arc::clone(turn)
    }

    pub(crate) async fn reconcile_compaction_turn_auth(
        &self,
        turn: &Arc<TurnContext>,
    ) -> Arc<TurnContext> {
        let current = self.reconcile_turn_auth(turn).await;
        if !Arc::ptr_eq(&current, turn)
            && let Some(lease) = current.extension_data.get::<AuthManagerLease>()
        {
            return Arc::new(turn.with_auth_lease(self, lease.as_ref().clone()));
        }
        Arc::clone(turn)
    }

    pub(crate) async fn reconcile_compaction_step_auth(
        &self,
        step: &Arc<StepContext>,
    ) -> Arc<StepContext> {
        let turn = self.reconcile_compaction_turn_auth(&step.turn).await;
        if Arc::ptr_eq(&turn, &step.turn) {
            return Arc::clone(step);
        }
        Arc::new(StepContext {
            turn,
            ..step.as_ref().clone()
        })
    }
}
