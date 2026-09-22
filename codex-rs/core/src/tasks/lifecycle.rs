use std::sync::Arc;

use codex_extension_api::ExtensionData;
use codex_extension_api::ThreadIdleCause;
use codex_extension_api::TurnStartPhase;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;

use crate::session::session::Session;
use crate::session::turn::TurnErrorAfterResponseStarted;
use crate::session::turn_context::TurnContext;

impl Session {
    pub(super) async fn emit_turn_start_lifecycle(
        self: &Arc<Self>,
        turn_context: &TurnContext,
        token_usage_at_turn_start: Option<&TokenUsage>,
        phase: TurnStartPhase,
    ) {
        let wake_run = self
            .services
            .thread_extension_data
            .get_or_init(codex_event_subscriptions::SubscriptionRunState::default);
        wake_run
            .running
            .store(/*val*/ true, std::sync::atomic::Ordering::Release);
        if let Some(origin) = turn_context
            .extension_data
            .get::<codex_event_subscriptions::SubscriptionWorkOrigin>()
        {
            wake_run.user_work.store(
                matches!(
                    *origin,
                    codex_event_subscriptions::SubscriptionWorkOrigin::UserWork
                ),
                std::sync::atomic::Ordering::Release,
            );
        }
        if turn_context
            .extension_data
            .get::<codex_event_subscriptions::UserStartedSubscriptionWork>()
            .is_some()
        {
            self.resume_subscription_work().await;
        }
        let collaboration_mode = turn_context.collaboration_mode();
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            if contributor.turn_start_phase(&self.services.thread_extension_data) != phase {
                continue;
            }
            if phase == TurnStartPhase::RegularTaskStart
                && contributor.requires_mcp_runtime(&self.services.thread_extension_data)
            {
                self.refresh_mcp_if_dirty().await;
            }
            contributor
                .on_turn_start(codex_extension_api::TurnStartInput {
                    turn_id: turn_context.sub_id.as_str(),
                    collaboration_mode: &collaboration_mode,
                    token_usage_at_turn_start,
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                })
                .await;
        }
    }

    pub(super) async fn emit_turn_stop_lifecycle(&self, turn_store: &ExtensionData) {
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_turn_stop(codex_extension_api::TurnStopInput {
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store,
                })
                .await;
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "idle admission state and receipt cleanup must observe the same active turn"
    )]
    pub(crate) async fn emit_thread_idle_lifecycle_if_idle(&self, cause: ThreadIdleCause) {
        let wake_run = self
            .services
            .thread_extension_data
            .get_or_init(codex_event_subscriptions::SubscriptionRunState::default);
        let dispatch = wake_run.dispatch.lock().await;
        let active_turn = self.active_turn.lock().await;
        let cause = {
            if active_turn.is_some() {
                return;
            }
            if self.is_interrupted() {
                ThreadIdleCause::Interrupted
            } else {
                cause
            }
        };
        if self.input_queue.has_trigger_turn_mailbox_items().await {
            return;
        }
        self.services
            .thread_extension_data
            .get_or_init(codex_event_subscriptions::SubscriptionRunState::default)
            .running
            .store(/*val*/ false, std::sync::atomic::Ordering::Release);
        self.clear_subscription_wake_inputs().await;
        drop(active_turn);
        drop(dispatch);

        for contributor in self.services.extensions.thread_lifecycle_contributors() {
            contributor
                .on_thread_idle(codex_extension_api::ThreadIdleInput {
                    cause,
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                })
                .await;
        }
    }

    pub(super) async fn emit_turn_abort_lifecycle(
        &self,
        reason: TurnAbortReason,
        turn_store: &ExtensionData,
    ) {
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_turn_abort(codex_extension_api::TurnAbortInput {
                    reason: reason.clone(),
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store,
                })
                .await;
        }
    }

    pub(crate) async fn emit_turn_error_lifecycle(
        &self,
        turn_context: &TurnContext,
        error: CodexErrorInfo,
    ) {
        let retryable_before_response = turn_context
            .extension_data
            .get::<TurnErrorAfterResponseStarted>()
            .is_none();
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_turn_error(codex_extension_api::TurnErrorInput {
                    turn_id: turn_context.sub_id.as_str(),
                    error: error.clone(),
                    session_source: &turn_context.session_source,
                    retryable_before_response,
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                })
                .await;
        }
    }
}
