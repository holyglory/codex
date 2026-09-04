use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_event_subscriptions::Clock;
use codex_event_subscriptions::EventFilter;
use codex_event_subscriptions::EventSubscriptionService;
use codex_event_subscriptions::EventSubscriptionStore;
use codex_event_subscriptions::HeartbeatSpec;
use codex_event_subscriptions::ListSubscriptionsQuery;
use codex_event_subscriptions::NewSubscription;
use codex_event_subscriptions::PublishedEvent;
use codex_event_subscriptions::SourceCursor;
use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeDisposition;
use codex_event_subscriptions::WakeReason;
use codex_event_subscriptions::WakeSink;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use tokio::sync::watch;

use crate::SqliteConfig;
use crate::StateRuntime;

const STARTED: u8 = 0;
const DEFERRED: u8 = 1;
const FAILED: u8 = 2;

#[derive(Clone)]
struct TestClock {
    now_ms: Arc<AtomicI64>,
    changed: watch::Sender<i64>,
    scheduled: watch::Sender<i64>,
}

impl TestClock {
    fn new(now_ms: i64) -> Self {
        Self {
            now_ms: Arc::new(AtomicI64::new(now_ms)),
            changed: watch::channel(now_ms).0,
            scheduled: watch::channel(now_ms).0,
        }
    }

    fn advance_to(&self, now_ms: i64) {
        self.now_ms.store(now_ms, Ordering::Release);
        self.changed.send_replace(now_ms);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::Acquire)
    }

    fn sleep_until(&self, deadline_ms: i64) -> impl Future<Output = ()> + Send {
        self.scheduled.send_replace(deadline_ms);
        let mut changed = self.changed.subscribe();
        async move {
            while *changed.borrow_and_update() < deadline_ms {
                if changed.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}

#[derive(Clone)]
struct RecordingWakeSink {
    disposition: Arc<AtomicU8>,
    batches: Arc<Mutex<Vec<WakeBatch>>>,
    count: watch::Sender<usize>,
}

impl RecordingWakeSink {
    fn new(disposition: WakeDisposition) -> Self {
        Self {
            disposition: Arc::new(AtomicU8::new(match disposition {
                WakeDisposition::Started => STARTED,
                WakeDisposition::DeferredUntilIdle => DEFERRED,
            })),
            batches: Arc::new(Mutex::new(Vec::new())),
            count: watch::channel(0).0,
        }
    }

    fn set_disposition(&self, disposition: WakeDisposition) {
        self.disposition.store(
            match disposition {
                WakeDisposition::Started => STARTED,
                WakeDisposition::DeferredUntilIdle => DEFERRED,
            },
            Ordering::Release,
        );
    }

    fn batches(&self) -> Vec<WakeBatch> {
        self.batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn wait_for_count(&self, expected: usize) {
        let mut count = self.count.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            while *count.borrow_and_update() < expected {
                count.changed().await.expect("wake sink should remain open");
            }
        })
        .await
        .expect("wake dispatch timed out");
    }
}

impl WakeSink for RecordingWakeSink {
    async fn wake(&self, wake: WakeBatch) -> Result<WakeDisposition, String> {
        let count = {
            let mut batches = self
                .batches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            batches.push(wake);
            batches.len()
        };
        self.count.send_replace(count);
        if self.disposition.load(Ordering::Acquire) == FAILED {
            return Err("temporary wake failure".to_string());
        }
        Ok(if self.disposition.load(Ordering::Acquire) == STARTED {
            WakeDisposition::Started
        } else {
            WakeDisposition::DeferredUntilIdle
        })
    }
}

#[tokio::test]
async fn new_events_during_retry_wait_for_the_retry_deadline() {
    let (runtime, _home) = runtime().await;
    let clock = TestClock::new(/*now_ms*/ 1_000);
    let sink = RecordingWakeSink::new(WakeDisposition::Started);
    sink.disposition.store(FAILED, Ordering::Release);
    let service = EventSubscriptionService::spawn(
        runtime.event_subscriptions().clone(),
        sink.clone(),
        clock.clone(),
    );
    service
        .create(event_subscription(
            ThreadId::new(),
            /*heartbeat_at*/ None,
        ))
        .await
        .unwrap();
    service
        .publish(event(/*sequence*/ 1, /*occurred_at_ms*/ 1_000))
        .await
        .unwrap();
    sink.wait_for_count(/*expected*/ 1).await;
    let mut deadlines = clock.scheduled.subscribe();
    tokio::time::timeout(Duration::from_secs(2), async {
        while *deadlines.borrow_and_update() != 2_000 {
            deadlines.changed().await.unwrap();
        }
    })
    .await
    .expect("failed dispatch should schedule a future retry");

    service
        .publish(event(/*sequence*/ 2, /*occurred_at_ms*/ 1_000))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), deadlines.changed())
        .await
        .expect("scheduler should reconsider the new event")
        .unwrap();
    assert_eq!(*deadlines.borrow_and_update(), 2_000);
    assert_eq!(sink.batches().len(), 1);

    sink.set_disposition(WakeDisposition::Started);
    clock.advance_to(/*now_ms*/ 2_000);
    sink.wait_for_count(/*expected*/ 2).await;
    assert_eq!(
        sink.batches()[1].items[0]
            .event
            .as_ref()
            .map(|event| (event.cursor.sequence, event.coalesced_event_count)),
        Some((2, 2))
    );
    service.shutdown().await;
}

async fn runtime() -> (Arc<StateRuntime>, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("temporary SQLite home");
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let runtime = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("state runtime");
    (runtime, home)
}

fn clock_subscription(thread_id: ThreadId, deadline_ms: i64) -> NewSubscription {
    NewSubscription {
        thread_id,
        filter: None,
        source_cursor: None,
        heartbeat: Some(HeartbeatSpec {
            interval_ms: 1_000,
            first_deadline_at_ms: Some(deadline_ms),
        }),
    }
}

fn event_subscription(thread_id: ThreadId, heartbeat_at: Option<i64>) -> NewSubscription {
    NewSubscription {
        thread_id,
        filter: Some(EventFilter {
            source: "build".to_string(),
            event_types: BTreeSet::from(["completed".to_string()]),
            labels: BTreeMap::from([("branch".to_string(), "main".to_string())]),
        }),
        source_cursor: Some(SourceCursor {
            sequence: 0,
            value: Some("start".to_string()),
        }),
        heartbeat: heartbeat_at.map(|deadline| HeartbeatSpec {
            interval_ms: 1_000,
            first_deadline_at_ms: Some(deadline),
        }),
    }
}

fn event(sequence: u64, occurred_at_ms: i64) -> PublishedEvent {
    PublishedEvent {
        id: format!("event-{sequence}"),
        source: "build".to_string(),
        event_type: "completed".to_string(),
        cursor: SourceCursor {
            sequence,
            value: Some(format!("cursor-{sequence}")),
        },
        labels: BTreeMap::from([("branch".to_string(), "main".to_string())]),
        occurred_at_ms,
    }
}

#[tokio::test]
async fn one_clock_serves_multiple_subscriptions_without_model_work_while_waiting() {
    let (runtime, _home) = runtime().await;
    let clock = TestClock::new(/*now_ms*/ 1_000);
    let sink = RecordingWakeSink::new(WakeDisposition::Started);
    let service = EventSubscriptionService::spawn(
        runtime.event_subscriptions().clone(),
        sink.clone(),
        clock.clone(),
    );
    let thread_id = ThreadId::new();
    let first = service
        .create(clock_subscription(thread_id, /*deadline_ms*/ 2_000))
        .await
        .unwrap();
    let second = service
        .create(clock_subscription(thread_id, /*deadline_ms*/ 2_000))
        .await
        .unwrap();

    tokio::task::yield_now().await;
    assert!(sink.batches().is_empty(), "waiting must not request a wake");
    clock.advance_to(/*now_ms*/ 2_000);
    sink.wait_for_count(/*expected*/ 1).await;

    let batches = sink.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].thread_id, thread_id);
    assert_eq!(
        batches[0]
            .items
            .iter()
            .map(|item| item.subscription_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([first.id, second.id])
    );
    assert!(
        batches[0]
            .items
            .iter()
            .all(|item| item.reasons == BTreeSet::from([WakeReason::Heartbeat]))
    );
    service.shutdown().await;
}

#[tokio::test]
async fn synthetic_events_advance_cursors_and_ignore_duplicates_and_out_of_order_events() {
    let (runtime, _home) = runtime().await;
    let clock = TestClock::new(/*now_ms*/ 1_000);
    let sink = RecordingWakeSink::new(WakeDisposition::Started);
    let service =
        EventSubscriptionService::spawn(runtime.event_subscriptions().clone(), sink.clone(), clock);
    let subscription = service
        .create(event_subscription(
            ThreadId::new(),
            /*heartbeat_at*/ None,
        ))
        .await
        .unwrap();

    let mut unmatched = event(/*sequence*/ 1, /*occurred_at_ms*/ 1_050);
    unmatched
        .labels
        .insert("branch".to_string(), "other".to_string());
    let unmatched = service.publish(unmatched).await.unwrap();
    assert!(unmatched.accepted_subscription_ids.is_empty());
    assert!(unmatched.ignored_subscription_ids.is_empty());
    let accepted = service
        .publish(event(/*sequence*/ 2, /*occurred_at_ms*/ 1_100))
        .await
        .unwrap();
    assert_eq!(accepted.accepted_subscription_ids, vec![subscription.id]);
    sink.wait_for_count(/*expected*/ 1).await;
    for sequence in [2, 1] {
        let ignored = service
            .publish(event(sequence, /*occurred_at_ms*/ 1_200))
            .await
            .unwrap();
        assert_eq!(ignored.ignored_subscription_ids, vec![subscription.id]);
    }
    tokio::task::yield_now().await;
    assert_eq!(sink.batches().len(), 1);

    service
        .publish(event(/*sequence*/ 3, /*occurred_at_ms*/ 1_300))
        .await
        .unwrap();
    sink.wait_for_count(/*expected*/ 2).await;
    assert_eq!(sink.batches().len(), 2);
    let listed = service
        .list(ListSubscriptionsQuery {
            thread_id: None,
            offset: 0,
            limit: 20,
        })
        .await
        .unwrap();
    assert_eq!(
        listed.data[0]
            .source_cursor
            .as_ref()
            .map(|cursor| (cursor.sequence, cursor.value.as_deref())),
        Some((3, Some("cursor-3")))
    );
    service.shutdown().await;
}

#[tokio::test]
async fn simultaneous_event_and_heartbeat_are_one_continuation() {
    let (runtime, _home) = runtime().await;
    let clock = TestClock::new(/*now_ms*/ 1_000);
    let sink = RecordingWakeSink::new(WakeDisposition::Started);
    let service = EventSubscriptionService::spawn(
        runtime.event_subscriptions().clone(),
        sink.clone(),
        clock.clone(),
    );
    service
        .create(event_subscription(ThreadId::new(), Some(2_000)))
        .await
        .unwrap();

    clock.advance_to(/*now_ms*/ 2_000);
    service
        .publish(event(/*sequence*/ 1, /*occurred_at_ms*/ 2_000))
        .await
        .unwrap();
    sink.wait_for_count(/*expected*/ 1).await;
    let batches = sink.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0].items[0].reasons,
        BTreeSet::from([WakeReason::Event, WakeReason::Heartbeat])
    );
    service.shutdown().await;
}

#[tokio::test]
async fn active_thread_coalesces_events_until_idle_without_a_wake_storm() {
    let (runtime, _home) = runtime().await;
    let clock = TestClock::new(/*now_ms*/ 1_000);
    let sink = RecordingWakeSink::new(WakeDisposition::DeferredUntilIdle);
    let service =
        EventSubscriptionService::spawn(runtime.event_subscriptions().clone(), sink.clone(), clock);
    let thread_id = ThreadId::new();
    service
        .create(event_subscription(thread_id, /*heartbeat_at*/ None))
        .await
        .unwrap();

    for sequence in 1..=20 {
        service
            .publish(event(sequence, /*occurred_at_ms*/ 1_000))
            .await
            .unwrap();
    }
    sink.wait_for_count(/*expected*/ 1).await;
    tokio::task::yield_now().await;
    assert_eq!(sink.batches().len(), 1);

    sink.set_disposition(WakeDisposition::Started);
    service.notify_thread_ready(thread_id);
    sink.wait_for_count(/*expected*/ 2).await;
    let batches = sink.batches();
    assert_eq!(batches.len(), 2);
    assert_eq!(
        batches[1].items[0]
            .event
            .as_ref()
            .map(|event| (event.cursor.sequence, event.coalesced_event_count)),
        Some((20, 20))
    );
    service.shutdown().await;
}

#[tokio::test]
async fn pending_wake_recovers_after_scheduler_restart_and_cancelled_work_does_not() {
    let (runtime, _home) = runtime().await;
    let clock = TestClock::new(/*now_ms*/ 1_000);
    let deferred_sink = RecordingWakeSink::new(WakeDisposition::DeferredUntilIdle);
    let first_service = EventSubscriptionService::spawn(
        runtime.event_subscriptions().clone(),
        deferred_sink.clone(),
        clock.clone(),
    );
    let recover = first_service
        .create(event_subscription(
            ThreadId::new(),
            /*heartbeat_at*/ None,
        ))
        .await
        .unwrap();
    let cancel = first_service
        .create(clock_subscription(
            ThreadId::new(),
            /*deadline_ms*/ 2_000,
        ))
        .await
        .unwrap();
    first_service.trigger(vec![recover.id]).await.unwrap();
    deferred_sink.wait_for_count(/*expected*/ 1).await;
    assert!(first_service.cancel(cancel.id).await.unwrap());
    first_service.shutdown().await;

    clock.advance_to(/*now_ms*/ 2_000);
    let recovered_sink = RecordingWakeSink::new(WakeDisposition::Started);
    let recovered_service = EventSubscriptionService::spawn(
        runtime.event_subscriptions().clone(),
        recovered_sink.clone(),
        clock,
    );
    recovered_sink.wait_for_count(/*expected*/ 1).await;
    assert_eq!(
        recovered_sink.batches()[0].items[0].subscription_id,
        recover.id
    );
    tokio::task::yield_now().await;
    assert_eq!(recovered_sink.batches().len(), 1);
    recovered_service.shutdown().await;
}

#[tokio::test]
async fn many_due_threads_each_receive_at_most_one_initial_wake() {
    let (runtime, _home) = runtime().await;
    let clock = TestClock::new(/*now_ms*/ 1_000);
    let sink = RecordingWakeSink::new(WakeDisposition::Started);
    let service = EventSubscriptionService::spawn(
        runtime.event_subscriptions().clone(),
        sink.clone(),
        clock.clone(),
    );
    let thread_ids = (0..64).map(|_| ThreadId::new()).collect::<Vec<_>>();
    for thread_id in &thread_ids {
        service
            .create(clock_subscription(*thread_id, /*deadline_ms*/ 2_000))
            .await
            .unwrap();
    }
    clock.advance_to(/*now_ms*/ 2_000);
    sink.wait_for_count(thread_ids.len()).await;
    let observed = sink
        .batches()
        .into_iter()
        .map(|batch| batch.thread_id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(observed.len(), thread_ids.len());
    service.shutdown().await;
}

#[tokio::test]
async fn overdue_heartbeat_coalesces_missed_intervals_instead_of_storming() {
    let (runtime, _home) = runtime().await;
    let clock = TestClock::new(/*now_ms*/ 1_000);
    let sink = RecordingWakeSink::new(WakeDisposition::Started);
    let service = EventSubscriptionService::spawn(
        runtime.event_subscriptions().clone(),
        sink.clone(),
        clock.clone(),
    );
    service
        .create(clock_subscription(
            ThreadId::new(),
            /*deadline_ms*/ 2_000,
        ))
        .await
        .unwrap();

    clock.advance_to(/*now_ms*/ 10_000);
    sink.wait_for_count(/*expected*/ 1).await;
    tokio::task::yield_now().await;
    assert_eq!(sink.batches().len(), 1);
    let listed = service
        .list(ListSubscriptionsQuery {
            thread_id: None,
            offset: 0,
            limit: 20,
        })
        .await
        .unwrap();
    assert_eq!(listed.data[0].next_heartbeat_at_ms, Some(11_000));
    service.shutdown().await;
}

#[tokio::test]
async fn deleting_thread_state_removes_its_subscriptions_and_pending_wakes() {
    let (runtime, _home) = runtime().await;
    let thread_id = ThreadId::new();
    let clock = TestClock::new(/*now_ms*/ 1_000);
    let sink = RecordingWakeSink::new(WakeDisposition::DeferredUntilIdle);
    let service =
        EventSubscriptionService::spawn(runtime.event_subscriptions().clone(), sink.clone(), clock);
    let subscription = service
        .create(event_subscription(thread_id, /*heartbeat_at*/ None))
        .await
        .unwrap();
    service.trigger(vec![subscription.id]).await.unwrap();
    sink.wait_for_count(/*expected*/ 1).await;

    assert_eq!(
        runtime.delete_threads_strict(&[thread_id]).await.unwrap(),
        0
    );
    let listed = service
        .list(ListSubscriptionsQuery {
            thread_id: Some(thread_id),
            offset: 0,
            limit: 20,
        })
        .await
        .unwrap();
    assert!(listed.data.is_empty());
    assert!(
        runtime
            .event_subscriptions()
            .pending_wake(thread_id)
            .await
            .unwrap()
            .is_none()
    );
    service.shutdown().await;
}
