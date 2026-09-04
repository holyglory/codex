use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeReason;
use codex_protocol::models::ContentItemKind;
use serde::Serialize;

use super::ContextualUserFragment;

// Keep the entire fragment below 10K tokens even for byte-level tokenization.
// All 128 subscription IDs fit; only optional notification metadata is pruned.
const MAX_BODY_BYTES: usize = 8 * 1024;
const OPEN_TAG: &str = "<event_subscription_wake>";
const CLOSE_TAG: &str = "</event_subscription_wake>";
const INTRO: &str = "A background subscription wake occurred. Continue this thread using only the bounded typed metadata below. Raw external content was not retained or injected.";

#[derive(Clone, Debug)]
pub(crate) struct EventSubscriptionWakeContext {
    wake: WakeBatch,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelWake<'a> {
    subscription_ids: Vec<String>,
    notifications: &'a [ModelWakeItem],
    omitted_notification_metadata: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelWakeItem {
    subscription_id: String,
    reasons: Vec<WakeReason>,
    event: Option<ModelEventMetadata>,
    heartbeat_due_at_ms: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelEventMetadata {
    source: String,
    event_type: String,
    sequence: u64,
    occurred_at_ms: i64,
    coalesced_event_count: u32,
}

impl EventSubscriptionWakeContext {
    pub(crate) fn new(wake: WakeBatch) -> Self {
        Self { wake }
    }

    fn bounded_json(&self) -> String {
        let subscription_ids = self
            .wake
            .items
            .iter()
            .map(|item| item.subscription_id.to_string())
            .collect::<Vec<_>>();
        let notifications = self
            .wake
            .items
            .iter()
            .map(|item| ModelWakeItem {
                subscription_id: item.subscription_id.to_string(),
                reasons: item.reasons.iter().copied().collect(),
                event: item.event.as_ref().map(|event| ModelEventMetadata {
                    source: event.source.clone(),
                    event_type: event.event_type.clone(),
                    sequence: event.cursor.sequence,
                    occurred_at_ms: event.occurred_at_ms,
                    coalesced_event_count: event.coalesced_event_count,
                }),
                heartbeat_due_at_ms: item.heartbeat_due_at_ms,
            })
            .collect::<Vec<_>>();
        let mut visible = notifications.len();
        loop {
            let model_wake = ModelWake {
                subscription_ids: subscription_ids.clone(),
                notifications: &notifications[..visible],
                omitted_notification_metadata: notifications.len().saturating_sub(visible),
            };
            let json = serde_json::to_string(&model_wake)
                .unwrap_or_else(|_| "{\"metadataUnavailable\":true}".to_string());
            if json.len().saturating_add(INTRO.len()).saturating_add(3) <= MAX_BODY_BYTES
                || visible == 0
            {
                return json;
            }
            visible -= 1;
        }
    }
}

impl ContextualUserFragment for EventSubscriptionWakeContext {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("event_subscription.wake".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (OPEN_TAG, CLOSE_TAG)
    }

    fn body(&self) -> String {
        format!("\n{INTRO}\n{}\n", self.bounded_json())
    }
}

#[cfg(test)]
#[path = "event_subscription_wake_tests.rs"]
mod tests;
