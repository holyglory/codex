use super::*;
use pretty_assertions::assert_eq;
use std::sync::atomic::Ordering;

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "the test deliberately pauses idle admission while installing successor state"
)]
async fn old_idle_transition_preserves_successor_turn_and_alarm_receipts() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let run = session
        .services
        .thread_extension_data
        .get_or_init(codex_event_subscriptions::SubscriptionRunState::default);
    let dispatch = run.dispatch.lock().await;
    let idle =
        session.emit_thread_idle_lifecycle_if_idle(codex_extension_api::ThreadIdleCause::Completed);
    tokio::pin!(idle);
    assert!(futures::poll!(idle.as_mut()).is_pending());

    // A successor claims the active turn before the old idle callback can proceed.
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
    run.running.store(/*val*/ true, Ordering::Release);
    let wake = WakeBatch {
        thread_id: session.thread_id,
        items: Vec::new(),
    };
    let item = ContextualUserFragment::into(EventSubscriptionWakeContext::new(wake.clone()));
    let receipt = (
        item,
        PendingWakeBatch {
            wake,
            through_revision: 1,
        },
    );
    let inputs = session
        .services
        .thread_extension_data
        .get_or_init(PendingSubscriptionInputs::default);
    inputs.0.lock().await.push(receipt.clone());
    drop(dispatch);
    idle.await;
    assert_eq!(
        (
            run.running.load(Ordering::Acquire),
            inputs.0.lock().await.clone()
        ),
        (true, vec![receipt])
    );
}
