use std::collections::BTreeSet;

use codex_event_subscriptions::EventMetadata;
use codex_event_subscriptions::SourceCursor;
use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeItem;
use codex_event_subscriptions::WakeReason;
use codex_protocol::ThreadId;
use uuid::Uuid;

use super::*;

#[test]
fn wake_context_is_typed_bounded_and_includes_every_subscription_id() {
    let thread_id = ThreadId::new();
    let ids = (0..128).map(|_| Uuid::now_v7()).collect::<Vec<_>>();
    let wake = WakeBatch {
        thread_id,
        items: ids
            .iter()
            .map(|id| WakeItem {
                subscription_id: *id,
                reasons: BTreeSet::from([WakeReason::Heartbeat]),
                event: Some(EventMetadata {
                    id: "external-event-body-like-id".to_string(),
                    source: "build".to_string(),
                    event_type: "completed".to_string(),
                    cursor: SourceCursor {
                        sequence: 7,
                        value: Some("opaque-external-cursor".to_string()),
                    },
                    labels: std::collections::BTreeMap::from([(
                        "untrusted".to_string(),
                        "raw-external-label-value".to_string(),
                    )]),
                    occurred_at_ms: 1_000,
                    coalesced_event_count: 1,
                }),
                heartbeat_due_at_ms: Some(1_000),
            })
            .collect(),
    };
    let rendered = EventSubscriptionWakeContext::new(wake).render();

    assert!(rendered.len() <= MAX_BODY_BYTES + OPEN_TAG.len() + CLOSE_TAG.len());
    assert!(rendered.contains("Raw external content was not retained or injected"));
    assert!(!rendered.contains("opaque-external-cursor"));
    assert!(!rendered.contains("raw-external-label-value"));
    assert!(!rendered.contains("external-event-body-like-id"));
    for id in ids {
        assert!(rendered.contains(&id.to_string()));
    }
}
