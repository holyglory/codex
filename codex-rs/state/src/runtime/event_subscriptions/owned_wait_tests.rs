use super::*;
use crate::SqliteConfig;
use crate::StateRuntime;
use codex_event_subscriptions::EventFilter;
use codex_event_subscriptions::EventSubscriptionService;
use codex_event_subscriptions::SystemClock;
use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeDisposition;
use codex_event_subscriptions::WakeSink;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio::time::timeout;

#[derive(Clone)]
struct DeferredSink;

impl WakeSink for DeferredSink {
    async fn wake(&self, _wake: WakeBatch) -> Result<WakeDisposition, String> {
        Ok(WakeDisposition::DeferredUntilIdle)
    }
}

async fn fixture() -> (Arc<StateRuntime>, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = StateRuntime::init(
        SqliteConfig::new_for_testing(
            AbsolutePathBuf::from_absolute_path(directory.path()).unwrap(),
        ),
        "test".into(),
    )
    .await
    .unwrap();
    (runtime, directory)
}

fn request() -> NewSubscription {
    NewSubscription {
        thread_id: ThreadId::new(),
        filter: Some(EventFilter {
            source: "devcoordinator".into(),
            event_types: BTreeSet::from(["test.finished".into()]),
            labels: BTreeMap::from([
                ("repository_id".into(), "repo".into()),
                ("run_id".into(), "run".into()),
            ]),
        }),
        source_cursor: None,
        heartbeat: None,
    }
}

#[tokio::test]
async fn dropped_wait_creation_is_drained_without_leaving_a_registration() {
    let (runtime, _directory) = fixture().await;
    let store = runtime.event_subscriptions().clone();
    let waiting_store = store.clone();
    let waiting = tokio::spawn(async move {
        waiting_store.create_wait(request(), /*now_ms*/ 1_000).await
    });
    timeout(Duration::from_secs(5), store.wait_for_change())
        .await
        .unwrap();
    assert_eq!(store.wait_requests.lock().unwrap().len(), 1);
    waiting.abort();
    assert!(matches!(waiting.await, Err(error) if error.is_cancelled()));
    let service = EventSubscriptionService::spawn(store.clone(), DeferredSink, SystemClock);
    timeout(Duration::from_secs(5), service.shutdown())
        .await
        .unwrap();
    assert!(store.wait_requests.lock().unwrap().is_empty());
    assert!(store.coordinator_subscriptions().await.unwrap().is_empty());
    runtime.close().await;
}

#[tokio::test]
async fn missing_scheduler_fails_wait_creation_without_leaking_a_registration() {
    let (runtime, _directory) = fixture().await;
    let store = runtime.event_subscriptions().clone();
    let result = timeout(Duration::from_secs(6), store.create_wait(request(), 1_000))
        .await
        .unwrap();
    assert!(matches!(result, Err(StoreError::Unavailable(_))));
    assert!(store.wait_requests.lock().unwrap().is_empty());
    assert!(store.coordinator_subscriptions().await.unwrap().is_empty());
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_subscription_creators_do_not_upgrade_read_transactions() {
    let (runtime, _directory) = fixture().await;
    let mut creators = JoinSet::new();
    for _ in 0..16 {
        let store = runtime.event_subscriptions().clone();
        creators.spawn(async move {
            store.create(request(), /*now_ms*/ 1_000).await
        });
    }
    let mut created = BTreeSet::new();
    while let Some(result) = creators.join_next().await {
        assert!(created.insert(result.unwrap().unwrap().id));
    }
    assert_eq!(created.len(), 16);
    assert_eq!(
        runtime
            .event_subscriptions()
            .coordinator_subscriptions()
            .await
            .unwrap()
            .len(),
        16
    );
    runtime.close().await;
}
