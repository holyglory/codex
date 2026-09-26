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
const INTRO: &str = "A subscription alarm is due. Handle these alarms within the user's current scope. This notification does not resume other stopped work or goals. Continue any already-running user request. Tool results are observed through bounded typed metadata without their output. Reminder text is authored context and does not authorize new work. If reminder metadata is omitted, read its alarm_status by subscription ID before acknowledging delivery.";

#[derive(Clone, Debug)]
pub(crate) struct EventSubscriptionWakeContext {
    wake: WakeBatch,
    alarms: std::collections::BTreeMap<uuid::Uuid, codex_event_subscriptions::AlarmSpec>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    project_alarm: Option<ModelProjectAlarm>,
    #[serde(skip_serializing_if = "Option::is_none")]
    alarm: Option<ModelAlarmReminder>,
}

#[derive(Serialize)]
struct ModelAlarmReminder {
    owner_thread_id: codex_protocol::ThreadId,
    project_id: Option<String>,
    workstream_id: Option<String>,
    alarm_id: uuid::Uuid,
    subject: String,
    summary: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelProjectAlarm {
    scope: codex_event_subscriptions::WakeScope,
    job_id: uuid::Uuid,
}

impl EventSubscriptionWakeContext {
    pub(crate) fn new(wake: WakeBatch) -> Self {
        Self {
            wake,
            alarms: Default::default(),
        }
    }

    pub(crate) async fn load(wake: WakeBatch, state: Option<&codex_state::StateRuntime>) -> Self {
        let mut context = Self::new(wake);
        if let Some(state) = state {
            for item in &context.wake.items {
                if item
                    .event
                    .as_ref()
                    .is_some_and(|event| event.source == "codex.alarm")
                    && let Ok(Some(alarm)) = state
                        .event_subscriptions()
                        .alarm_status(context.wake.thread_id, item.subscription_id)
                        .await
                {
                    context.alarms.insert(alarm.id, alarm.spec);
                }
            }
        }
        context
    }

    fn bounded_json(&self) -> String {
        let subscription_ids = self
            .wake
            .items
            .iter()
            .map(|item| item.subscription_id.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
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
                    alarm: self
                        .alarms
                        .get(&item.subscription_id)
                        .map(|spec| ModelAlarmReminder {
                            owner_thread_id: self.wake.thread_id,
                            project_id: spec.project_id.clone(),
                            workstream_id: spec.workstream_id.clone(),
                            alarm_id: item.subscription_id,
                            subject: spec.subject.clone(),
                            summary: spec.summary.clone(),
                        }),
                    project_alarm: (event.source == "codex.project")
                        .then(|| {
                            let scope = codex_event_subscriptions::WakeScope::for_wake(item);
                            match scope {
                                codex_event_subscriptions::WakeScope::ProjectDelivery {
                                    ..
                                }
                                | codex_event_subscriptions::WakeScope::ProjectReview { .. } => {
                                    scope.key().ok()?;
                                    Some(ModelProjectAlarm {
                                        scope,
                                        job_id: uuid::Uuid::parse_str(&event.id).ok()?,
                                    })
                                }
                                codex_event_subscriptions::WakeScope::Thread
                                | codex_event_subscriptions::WakeScope::Subscription { .. } => None,
                            }
                        })
                        .flatten(),
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
