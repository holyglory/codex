use super::*;
use crate::session::step_context::StepContext;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_event_subscriptions::EventSubscriptionService;
use codex_event_subscriptions::Subscription;
use codex_event_subscriptions::SystemClock;
use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeDisposition;
use codex_event_subscriptions::WakeSink;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct DeferredSink;

impl WakeSink for DeferredSink {
    async fn wake(&self, _wake: WakeBatch) -> Result<WakeDisposition, String> {
        Ok(WakeDisposition::DeferredUntilIdle)
    }
}

enum Cancellation {
    Token,
    DroppedHandler,
}

async fn assert_durable_cleanup(cancellation: Cancellation) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = StateRuntime::init(
        SqliteConfig::new_for_testing(
            AbsolutePathBuf::from_absolute_path(directory.path()).unwrap(),
        ),
        "test".into(),
    )
    .await
    .unwrap();
    let store = runtime.event_subscriptions().clone();
    let service = EventSubscriptionService::spawn(store.clone(), DeferredSink, SystemClock);
    let (mut session, turn) = crate::session::tests::make_session_and_context().await;
    session.services.state_db = Some(Arc::clone(&runtime));
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let token = CancellationToken::new();
    let invocation = ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: token.clone(),
        tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        call_id: "native-wait".into(),
        tool_name: ToolName::plain("await_work"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: json!({
                "source":"devcoordinator", "event_types":["test.finished"],
                "labels":{"repository_id":"repo", "run_id":"run"},
                "after_sequence":41, "deadline_at_ms":project_automation_now_ms()+60_000,
            })
            .to_string(),
        },
    };
    let handler = tokio::spawn(async move { AwaitWorkHandler.handle(invocation).await });
    let subscription = timeout(Duration::from_secs(5), async {
        loop {
            let subscriptions = store.coordinator_subscriptions().await.unwrap();
            if let Some(subscription) = subscriptions.into_iter().next() {
                break subscription;
            }
            store.await_source_change().await;
        }
    })
    .await
    .unwrap();
    match cancellation {
        Cancellation::Token => {
            token.cancel();
            assert!(
                timeout(Duration::from_secs(5), handler)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
        }
        Cancellation::DroppedHandler => {
            handler.abort();
            assert!(
                matches!(timeout(Duration::from_secs(5), handler).await.unwrap(), Err(error) if error.is_cancelled())
            );
            assert!(!token.is_cancelled());
        }
    }
    let removed = timeout(
        Duration::from_secs(5),
        store.await_subscription(subscription.thread_id, subscription.id),
    )
    .await
    .unwrap();
    assert!(removed.is_err());
    assert_eq!(
        store.coordinator_subscriptions().await.unwrap(),
        Vec::<Subscription>::new()
    );
    service.shutdown().await;
    runtime.close().await;
}

#[tokio::test]
async fn cancelled_await_work_handler_removes_its_durable_subscription() {
    assert_durable_cleanup(Cancellation::Token).await;
}

#[tokio::test]
async fn dropped_await_work_handler_removes_its_durable_subscription() {
    assert_durable_cleanup(Cancellation::DroppedHandler).await;
}
