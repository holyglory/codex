use codex_event_subscriptions::WakeBatch;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::turn_input::StartIfIdleSubmission;
use codex_protocol::turn_input::TurnInput;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnStartOptions;

use crate::CodexThread;
use crate::context::ContextualUserFragment;
use crate::context::EventSubscriptionWakeContext;

impl CodexThread {
    /// Starts one automatic continuation containing a coalesced subscription wake.
    pub async fn start_event_subscription_wake_if_idle(
        &self,
        wake: WakeBatch,
    ) -> CodexResult<StartIfIdleSubmission> {
        let response_item = ContextualUserFragment::into(EventSubscriptionWakeContext::new(wake));
        self.start_turn_if_idle(
            TurnInputRequest::new(TurnInput::ResponseItem(response_item)).on_start(
                TurnStartOptions {
                    turn_trigger: Some("event_subscription".to_string()),
                    ..Default::default()
                },
            ),
        )
        .await
    }
}
