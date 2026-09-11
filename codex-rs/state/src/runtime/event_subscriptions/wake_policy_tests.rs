use codex_event_subscriptions::EventSubscriptionStore;
use codex_event_subscriptions::HeartbeatSpec;
use codex_event_subscriptions::NewSubscription;
use codex_event_subscriptions::WakePolicy;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use super::*;

#[tokio::test]
async fn wake_permissions_persist_stop_resume_and_reject_stale_changes() {
    let home = tempfile::tempdir().unwrap();
    let config = crate::SqliteConfig::new_for_testing(home.path().abs());
    let runtime = crate::StateRuntime::init(config, "test".into())
        .await
        .unwrap();
    let store = runtime.event_subscriptions();
    let owner = ThreadId::new();
    let granted = store
        .set_wake_policy(WakePolicyChange {
            thread_id: owner,
            scope: WakeScope::Thread,
            policy: WakePolicy::AllowBackground,
            expected_revision: 0,
            authorization_ref: "user-request".into(),
        })
        .await
        .unwrap();
    store
        .record_wake_lifecycle(owner, WakeLifecycle::UserStopped)
        .await
        .unwrap();
    let stopped = store.read_wake_policy(owner).await.unwrap();
    assert!(!stopped.allows_scopes(&[WakeScope::Thread]));
    let stale = store
        .set_wake_policy(WakePolicyChange {
            thread_id: owner,
            scope: WakeScope::Thread,
            policy: WakePolicy::RunningOnly,
            expected_revision: granted.revision,
            authorization_ref: "user-revoke".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        stale,
        StoreError::RevisionConflict {
            expected: granted.revision,
            actual: stopped.revision
        }
    );
    store
        .record_wake_lifecycle(owner, WakeLifecycle::UserStarted)
        .await
        .unwrap();
    let resumed = store.read_wake_policy(owner).await.unwrap();
    assert!(resumed.allows_scopes(&[WakeScope::Thread]));
    assert_eq!(resumed.policies, granted.policies);
    drop(runtime);
    let restored = crate::StateRuntime::init(
        crate::SqliteConfig::new_for_testing(home.path().abs()),
        "test".into(),
    )
    .await
    .unwrap();
    assert_eq!(
        restored
            .event_subscriptions()
            .read_wake_policy(owner)
            .await
            .unwrap(),
        resumed
    );
}

#[tokio::test]
async fn acknowledging_one_alarm_retains_other_and_newer_alarms() {
    let home = tempfile::tempdir().unwrap();
    let runtime = crate::StateRuntime::init(
        crate::SqliteConfig::new_for_testing(home.path().abs()),
        "test".into(),
    )
    .await
    .unwrap();
    let store = runtime.event_subscriptions();
    let owner = ThreadId::new();
    for _ in 0..2 {
        store
            .create(
                NewSubscription {
                    thread_id: owner,
                    filter: None,
                    source_cursor: None,
                    heartbeat: Some(HeartbeatSpec {
                        interval_ms: 1000,
                        first_deadline_at_ms: Some(1000),
                    }),
                },
                /*now_ms*/ 0,
            )
            .await
            .unwrap();
    }
    store.collect_due_heartbeats(1000).await.unwrap();
    let pending = store.pending_wake(owner).await.unwrap().unwrap();
    assert_eq!(pending.wake.items.len(), 2);
    store
        .complete_delivery(
            owner,
            pending.through_revision,
            &pending.wake.items[..1],
            &[],
        )
        .await
        .unwrap();
    let remaining = store.pending_wake(owner).await.unwrap().unwrap();
    assert_eq!(remaining.wake.items, pending.wake.items[1..]);
    store
        .trigger(
            vec![remaining.wake.items[0].subscription_id],
            /*now_ms*/ 1100,
        )
        .await
        .unwrap();
    store
        .complete_delivery(
            owner,
            remaining.through_revision,
            &remaining.wake.items,
            &[],
        )
        .await
        .unwrap();
    assert!(store.pending_wake(owner).await.unwrap().is_some());
    store
        .record_wake_lifecycle(owner, WakeLifecycle::UserStopped)
        .await
        .unwrap();
    let suspended = store.pending_wake(owner).await.unwrap().unwrap();
    assert_eq!(suspended.wake.items, remaining.wake.items);
}

#[tokio::test]
async fn cancelled_alarm_retires_its_permission_and_changes_the_revision() {
    let home = tempfile::tempdir().unwrap();
    let runtime = crate::StateRuntime::init(
        crate::SqliteConfig::new_for_testing(home.path().abs()),
        "test".into(),
    )
    .await
    .unwrap();
    let store = runtime.event_subscriptions();
    let owner = ThreadId::new();
    let subscription = store
        .create(
            NewSubscription {
                thread_id: owner,
                filter: None,
                source_cursor: None,
                heartbeat: Some(HeartbeatSpec {
                    interval_ms: 1000,
                    first_deadline_at_ms: Some(1000),
                }),
            },
            /*now_ms*/ 0,
        )
        .await
        .unwrap();
    let granted = store
        .set_wake_policy(WakePolicyChange {
            thread_id: owner,
            scope: WakeScope::Subscription {
                subscription_id: subscription.id,
            },
            policy: WakePolicy::AllowBackground,
            expected_revision: 0,
            authorization_ref: "user-request".into(),
        })
        .await
        .unwrap();
    assert!(store.cancel(subscription.id).await.unwrap());
    let retired = store.read_wake_policy(owner).await.unwrap();
    assert!(retired.policies.is_empty());
    assert!(retired.revision > granted.revision);
}
