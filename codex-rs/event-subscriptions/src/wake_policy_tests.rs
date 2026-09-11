use super::*;
use crate::WakeReason;
use pretty_assertions::assert_eq;

fn alarm(subscription_id: Uuid) -> WakeItem {
    WakeItem {
        subscription_id,
        reasons: [WakeReason::Heartbeat].into(),
        event: None,
        heartbeat_due_at_ms: Some(1000),
    }
}

#[test]
fn stop_suspends_existing_grants_and_resume_restores_them() {
    let item = alarm(Uuid::now_v7());
    let mut state = ThreadWakePolicy::default();
    assert!(!state.allows_background(&item));
    state.policies.push(ScopedWakePolicy {
        scope: WakeScope::Thread,
        policy: WakePolicy::AllowBackground,
        revision: 1,
        authorization_ref: "user-request".into(),
    });
    let allowed = state.allows_background(&item);
    state.stopped_revision = 2;
    let stopped = state.allows_background(&item);
    state.resumed_revision = 3;
    assert_eq!(
        [allowed, stopped, state.allows_background(&item)],
        [true, false, true]
    );
}

#[test]
fn new_grant_while_stopped_applies_only_to_its_alarm() {
    let first = alarm(Uuid::now_v7());
    let second = alarm(Uuid::now_v7());
    let mut state = ThreadWakePolicy {
        stopped_revision: 2,
        policies: vec![ScopedWakePolicy {
            scope: WakeScope::Thread,
            policy: WakePolicy::AllowBackground,
            revision: 1,
            authorization_ref: "old-request".into(),
        }],
        ..ThreadWakePolicy::default()
    };
    state.policies.push(ScopedWakePolicy {
        scope: WakeScope::for_wake(&first),
        policy: WakePolicy::AllowBackground,
        revision: 3,
        authorization_ref: "new-request".into(),
    });
    assert_eq!(
        [
            state.allows_background(&first),
            state.allows_background(&second)
        ],
        [true, false]
    );
    state.resumed_revision = 4;
    state.policies[1].policy = WakePolicy::RunningOnly;
    assert_eq!(
        [
            state.allows_background(&first),
            state.allows_background(&second)
        ],
        [false, true]
    );
}
