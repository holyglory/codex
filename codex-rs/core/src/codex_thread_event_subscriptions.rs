use codex_event_subscriptions::EventSubscriptionStore;
use codex_event_subscriptions::PendingWakeBatch;
use codex_event_subscriptions::WakeBatch;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::turn_input::StartIfIdleSubmission;
use codex_protocol::turn_input::TurnInput;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnStartOptions;

use crate::CodexThread;
use crate::context::ContextualUserFragment;
use crate::context::EventSubscriptionWakeContext;

#[derive(Default)]
struct PendingSubscriptionInputs(tokio::sync::Mutex<Vec<(ResponseItem, PendingWakeBatch)>>);

impl CodexThread {
    pub async fn has_running_user_work(&self) -> bool {
        let run = self.subscription_run_state();
        run.running.load(std::sync::atomic::Ordering::Acquire)
            && run.user_work.load(std::sync::atomic::Ordering::Acquire)
            && !run.stop_pending.load(std::sync::atomic::Ordering::Acquire)
            && self.session.active_turn.lock().await.is_some()
    }

    pub fn subscription_run_state(
        &self,
    ) -> std::sync::Arc<codex_event_subscriptions::SubscriptionRunState> {
        self.session
            .services
            .thread_extension_data
            .get_or_init(codex_event_subscriptions::SubscriptionRunState::default)
    }
    /// Injects the alarm into the current turn without creating a continuation.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "subscription admission and input insertion must be atomic"
    )]
    pub async fn inject_subscription_wake_if_running(&self, wake: WakeBatch) -> bool {
        let is_manual = wake.items.iter().any(|item| {
            item.reasons
                .contains(&codex_event_subscriptions::WakeReason::Manual)
        });
        let expected: ResponseItem =
            ContextualUserFragment::into(EventSubscriptionWakeContext::new(wake.clone()));
        let history = self.session.clone_history().await;
        let delivered = history.raw_items().any(|item| match (item, &expected) {
            (
                ResponseItem::Message { role, content, .. },
                ResponseItem::Message {
                    role: expected_role,
                    content: expected_content,
                    ..
                },
            ) => role == expected_role && content == expected_content,
            _ => false,
        });
        if delivered && !is_manual {
            self.session
                .remember_subscription_wake_input(&expected, wake)
                .await;
            self.session
                .record_subscription_wake_delivery(&expected)
                .await;
            return true;
        }
        let run = self.subscription_run_state();
        let active = self.session.active_turn.lock().await;
        if !run.running.load(std::sync::atomic::Ordering::Acquire)
            || run.stop_pending.load(std::sync::atomic::Ordering::Acquire)
        {
            return false;
        }
        let Some(active) = active.as_ref() else {
            return false;
        };
        let newly_queued = self
            .session
            .remember_subscription_wake_input(&expected, wake)
            .await;
        if !newly_queued {
            return true;
        }
        self.session
            .input_queue
            .extend_pending_input_for_turn_state(
                active.turn_state.as_ref(),
                vec![crate::session::TurnInput::ResponseItem(
                    codex_history::ResponseItemEnvelope::new(expected),
                )],
            )
            .await;
        true
    }

    /// Starts one automatic continuation containing a coalesced subscription wake.
    pub async fn start_event_subscription_wake_if_idle(
        &self,
        wake: WakeBatch,
    ) -> CodexResult<StartIfIdleSubmission> {
        let response_item =
            ContextualUserFragment::into(EventSubscriptionWakeContext::new(wake.clone()));
        let newly_queued = self
            .session
            .remember_subscription_wake_input(&response_item, wake)
            .await;
        let result = self
            .start_turn_if_idle(
                TurnInputRequest::new(TurnInput::ResponseItem(response_item.clone())).on_start(
                    TurnStartOptions {
                        turn_trigger: Some("event_subscription".to_string()),
                        ..Default::default()
                    },
                ),
            )
            .await;
        if newly_queued && !matches!(result, Ok(StartIfIdleSubmission::Started { .. })) {
            self.session
                .services
                .thread_extension_data
                .get_or_init(PendingSubscriptionInputs::default)
                .0
                .lock()
                .await
                .retain(|(item, _)| item != &response_item);
        }
        result
    }

    /// Repeats a turn after a terminal model-capacity alarm without adding a
    /// synthetic user message to the conversation.
    pub async fn start_capacity_retry_if_idle(&self) -> CodexResult<StartIfIdleSubmission> {
        self.start_turn_if_idle(TurnInputRequest::user_input(Vec::new()).on_start(
            TurnStartOptions {
                turn_trigger: Some("capacity_retry".to_string()),
                ..Default::default()
            },
        ))
        .await
    }
}

impl crate::session::session::Session {
    async fn remember_subscription_wake_input(&self, item: &ResponseItem, wake: WakeBatch) -> bool {
        let Some(state) = self.state_db() else {
            return true;
        };
        let pending = match state
            .event_subscriptions()
            .pending_wake(self.thread_id)
            .await
        {
            Ok(Some(pending))
                if wake
                    .items
                    .iter()
                    .all(|item| pending.wake.items.contains(item)) =>
            {
                pending
            }
            _ => return false,
        };
        let inputs = self
            .services
            .thread_extension_data
            .get_or_init(PendingSubscriptionInputs::default);
        let mut inputs = inputs.0.lock().await;
        if inputs.iter().any(|(queued, _)| queued == item) {
            return false;
        }
        if inputs.len() >= codex_event_subscriptions::MAX_SUBSCRIPTIONS_PER_THREAD {
            return false;
        }
        inputs.push((
            item.clone(),
            PendingWakeBatch {
                wake,
                through_revision: pending.through_revision,
            },
        ));
        true
    }

    pub(crate) async fn clear_subscription_wake_inputs(&self) {
        self.services
            .thread_extension_data
            .get_or_init(PendingSubscriptionInputs::default)
            .0
            .lock()
            .await
            .clear();
    }

    /// A queued alarm stays pending through Stop or a crash until its input is durable.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "receipt acknowledgement and Stop cleanup must be serialized through the durability barrier"
    )]
    pub(crate) async fn record_subscription_wake_delivery(&self, item: &ResponseItem) {
        let inputs = self
            .services
            .thread_extension_data
            .get_or_init(PendingSubscriptionInputs::default);
        let mut inputs = inputs.0.lock().await;
        let Some(index) = inputs.iter().position(|(queued, _)| queued == item) else {
            return;
        };
        let Some(state) = self.state_db() else {
            return;
        };
        if let Err(error) = self.flush_rollout().await {
            tracing::warn!(%error, "failed to persist alarm input; keeping it pending");
            return;
        }
        let pending = &inputs[index].1;
        match state
            .event_subscriptions()
            .complete_delivery(
                self.thread_id,
                pending.through_revision,
                &pending.wake.items,
                &[],
            )
            .await
        {
            Ok(()) => {
                inputs.remove(index);
            }
            Err(error) => tracing::warn!(%error, "failed to acknowledge persisted alarm input"),
        }
    }

    pub(crate) async fn resume_subscription_work(&self) {
        let run = self
            .services
            .thread_extension_data
            .get_or_init(codex_event_subscriptions::SubscriptionRunState::default);
        run.user_work
            .store(/*val*/ true, std::sync::atomic::Ordering::Release);
        if let Some(state) = self.state_db() {
            match state
                .event_subscriptions()
                .record_wake_lifecycle(
                    self.thread_id,
                    codex_event_subscriptions::WakeLifecycle::UserStarted,
                )
                .await
            {
                Ok(()) => run
                    .stop_pending
                    .store(/*val*/ false, std::sync::atomic::Ordering::Release),
                Err(error) => tracing::warn!(%error, "failed to persist user wake resumption"),
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Stop must suspend permissions and workers atomically with alarm admission"
    )]
    pub(crate) async fn stop_subscription_work(&self) {
        let run = self
            .services
            .thread_extension_data
            .get_or_init(codex_event_subscriptions::SubscriptionRunState::default);
        let _dispatch = run.dispatch.lock().await;
        run.stop_pending
            .store(/*val*/ true, std::sync::atomic::Ordering::Release);
        run.running
            .store(/*val*/ false, std::sync::atomic::Ordering::Release);
        run.user_work
            .store(/*val*/ false, std::sync::atomic::Ordering::Release);
        self.clear_subscription_wake_inputs().await;
        self.mark_interrupted();
        let Some(state) = self.state_db() else {
            return;
        };
        let store = state.event_subscriptions();
        match store
            .record_wake_lifecycle(
                self.thread_id,
                codex_event_subscriptions::WakeLifecycle::UserStopped,
            )
            .await
        {
            Ok(()) => run
                .stop_pending
                .store(/*val*/ false, std::sync::atomic::Ordering::Release),
            Err(error) => tracing::warn!(%error, "failed to persist suspended wake permissions"),
        }
        match store.project_review_workers_for_owner(self.thread_id).await {
            Ok(workers) => {
                for worker in workers {
                    let _ = self.services.agent_control.interrupt_agent(worker).await;
                }
            }
            Err(error) => tracing::warn!(%error, "failed to find owned project review workers"),
        }
    }
}

#[cfg(test)]
#[path = "codex_thread_event_subscriptions_tests.rs"]
mod tests;
