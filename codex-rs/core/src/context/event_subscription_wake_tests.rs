use std::collections::BTreeSet;

use codex_event_subscriptions::EventMetadata;
use codex_event_subscriptions::MAX_SUBSCRIPTIONS_PER_THREAD;
use codex_event_subscriptions::SourceCursor;
use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeItem;
use codex_event_subscriptions::WakeReason;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use uuid::Uuid;

use super::*;

#[test]
fn project_alarm_identifies_only_the_admitted_target() {
    let job_id = Uuid::now_v7();
    let context = EventSubscriptionWakeContext::new(WakeBatch {
        thread_id: ThreadId::new(),
        items: vec![WakeItem {
            subscription_id: Uuid::now_v7(),
            reasons: BTreeSet::from([WakeReason::Event]),
            event: Some(EventMetadata {
                id: job_id.to_string(),
                source: "codex.project".into(),
                event_type: "delivery_due".into(),
                cursor: SourceCursor {
                    sequence: 42,
                    value: None,
                },
                labels: std::collections::BTreeMap::from([
                    ("project".into(), "suit-project".into()),
                    ("target".into(), "permitted-preview".into()),
                    ("external".into(), "must-not-be-injected".into()),
                ]),
                occurred_at_ms: 1000,
                coalesced_event_count: 1,
            }),
            heartbeat_due_at_ms: None,
        }],
    });
    let json: serde_json::Value = serde_json::from_str(&context.bounded_json()).unwrap();
    assert_eq!(
        json["notifications"][0]["event"]["projectAlarm"],
        serde_json::json!({"scope":{"type":"projectDelivery","projectId":"suit-project","target":"permitted-preview"},"jobId":job_id})
    );
    assert!(!context.render().contains("must-not-be-injected"));
}

#[test]
fn wake_context_is_typed_bounded_and_includes_every_subscription_id() {
    let thread_id = ThreadId::new();
    let ids = (0..MAX_SUBSCRIPTIONS_PER_THREAD)
        .map(|_| Uuid::now_v7())
        .collect::<Vec<_>>();
    let wake = WakeBatch {
        thread_id,
        items: ids
            .iter()
            .map(|id| WakeItem {
                subscription_id: *id,
                reasons: BTreeSet::from([WakeReason::Heartbeat]),
                event: Some(EventMetadata {
                    id: "external-event-body-like-id".to_string(),
                    source: "🦀".repeat(128),
                    event_type: "𐀀".repeat(128),
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
    let context = EventSubscriptionWakeContext::new(wake);
    let rendered = context.render();

    assert!(context.body().len() <= MAX_BODY_BYTES);
    assert!(rendered.len() <= MAX_BODY_BYTES + OPEN_TAG.len() + CLOSE_TAG.len());
    assert!(rendered.contains("raw external content was not retained or injected"));
    assert!(!rendered.contains("opaque-external-cursor"));
    assert!(!rendered.contains("raw-external-label-value"));
    assert!(!rendered.contains("external-event-body-like-id"));
    let json: serde_json::Value = serde_json::from_str(&context.bounded_json()).unwrap();
    assert_eq!(
        json["subscriptionIds"],
        serde_json::json!(ids.iter().map(Uuid::to_string).collect::<Vec<_>>())
    );
    let visible = json["notifications"].as_array().unwrap().len();
    assert!(visible > 0);
    assert!(visible < ids.len());
    assert_eq!(
        json["omittedNotificationMetadata"],
        serde_json::json!(ids.len() - visible)
    );
}
