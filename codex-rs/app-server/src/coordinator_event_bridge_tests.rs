use super::*;
use codex_event_subscriptions::EventFilter;
use codex_event_subscriptions::EventSubscriptionStore;
use codex_event_subscriptions::NewSubscription;
use codex_event_subscriptions::Subscription;
use codex_event_subscriptions::SystemClock;
use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeDisposition;
use codex_event_subscriptions::WakeSink;
use codex_protocol::ThreadId;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;

#[derive(Clone, Default)]
struct IdleSink(Arc<AtomicUsize>);

impl WakeSink for IdleSink {
    async fn wake(&self, _wake: WakeBatch) -> Result<WakeDisposition, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(WakeDisposition::DeferredUntilIdle)
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    runtime: Arc<StateRuntime>,
    store: Arc<SqliteEventSubscriptionStore>,
    service: Arc<EventSubscriptionService>,
    sink: IdleSink,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let home = AbsolutePathBuf::from_absolute_path(directory.path()).unwrap();
        let runtime = StateRuntime::init(SqliteConfig::new_for_testing(home), "test".into())
            .await
            .unwrap();
        let store = Arc::new(runtime.event_subscriptions().clone());
        let sink = IdleSink::default();
        let service = Arc::new(EventSubscriptionService::spawn(
            store.as_ref().clone(),
            sink.clone(),
            SystemClock,
        ));
        Self {
            _directory: directory,
            runtime,
            store,
            service,
            sink,
        }
    }

    fn start(&self, source: impl EventSource) -> CoordinatorEventBridge {
        self.start_with_activation(source, /*activation*/ None)
    }

    fn start_with_activation(
        &self,
        source: impl EventSource,
        activation: Option<activation::Activation>,
    ) -> CoordinatorEventBridge {
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run_bridge(
            Arc::clone(&self.store),
            Arc::clone(&self.service),
            source,
            cancellation.clone(),
            activation,
        ));
        CoordinatorEventBridge {
            store: Arc::clone(&self.store),
            cancellation,
            task: Mutex::new(Some(task)),
        }
    }

    async fn subscribe(&self, job_key: &str, job_id: &str, kind: &str, after: u64) -> Subscription {
        self.store
            .create(
                NewSubscription {
                    thread_id: ThreadId::new(),
                    filter: Some(EventFilter {
                        source: "devcoordinator".into(),
                        event_types: BTreeSet::from([
                            kind.into(),
                            "source.unavailable".into(),
                            "source.cursor_stale".into(),
                        ]),
                        labels: BTreeMap::from([
                            ("repository_id".into(), "r1234567890abcdef".into()),
                            (job_key.into(), job_id.into()),
                        ]),
                    }),
                    source_cursor: Some(SourceCursor {
                        sequence: after,
                        value: None,
                    }),
                    heartbeat: None,
                },
                /*now_ms*/ 1_000,
            )
            .await
            .unwrap()
    }

    async fn finish(self, bridge: CoordinatorEventBridge) {
        timeout(Duration::from_secs(5), bridge.shutdown())
            .await
            .unwrap()
            .unwrap();
        self.service.shutdown().await;
        self.runtime.close().await;
    }
}

struct Request {
    cursor: Option<u64>,
    reply: oneshot::Sender<Result<SourceBatch, SourceError>>,
    cancellation: CancellationToken,
}

struct FixtureSource(mpsc::Sender<Request>);

impl EventSource for FixtureSource {
    async fn wait(
        &mut self,
        cursor: Option<u64>,
        cancellation: CancellationToken,
    ) -> Result<SourceBatch, SourceError> {
        let (reply, response) = oneshot::channel();
        self.0
            .send(Request {
                cursor,
                reply,
                cancellation: cancellation.clone(),
            })
            .await
            .unwrap();
        tokio::select! {
            _ = cancellation.cancelled() => Err(SourceError::Cancelled),
            result = response => result.unwrap(),
        }
    }
}

async fn next_request(requests: &mut mpsc::Receiver<Request>) -> Request {
    timeout(Duration::from_secs(5), async {
        loop {
            let request = requests
                .recv()
                .await
                .expect("fixture source remains available");
            if request.cancellation.is_cancelled() {
                continue;
            }
            assert!(
                !request.reply.is_closed(),
                "live fixture request unexpectedly lost its receiver"
            );
            return request;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn pending_source_change_does_not_start_an_already_cancelled_wait() {
    let fixture = Fixture::new().await;
    fixture
        .subscribe("run_id", "run-a", "test.finished", /*after*/ 41)
        .await;
    let (requests, mut received) = mpsc::channel(8);
    let bridge = fixture.start(FixtureSource(requests));
    let request = timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!request.cancellation.is_cancelled());
    assert!(!request.reply.is_closed());
    assert_eq!(request.cursor, Some(41));
    fixture.finish(bridge).await;
}

#[tokio::test]
async fn fixture_discards_cancelled_requests_without_filtering_live_cursors() {
    let (requests, mut received) = mpsc::channel(2);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let (stale_reply, stale_response) = oneshot::channel();
    drop(stale_response);
    requests
        .send(Request {
            cursor: Some(41),
            reply: stale_reply,
            cancellation: cancelled,
        })
        .await
        .unwrap();
    let (live_reply, live_response) = oneshot::channel();
    requests
        .send(Request {
            cursor: Some(99),
            reply: live_reply,
            cancellation: CancellationToken::new(),
        })
        .await
        .unwrap();
    let request = next_request(&mut received).await;
    assert_eq!(request.cursor, Some(99));
    request.reply.send(Err(SourceError::Unavailable)).unwrap();
    assert!(matches!(
        live_response.await.unwrap(),
        Err(SourceError::Unavailable)
    ));
}

#[tokio::test]
#[should_panic(expected = "live fixture request unexpectedly lost its receiver")]
async fn fixture_rejects_closed_receivers_without_cancellation() {
    let (requests, mut received) = mpsc::channel(1);
    let (reply, response) = oneshot::channel();
    drop(response);
    requests
        .send(Request {
            cursor: Some(41),
            reply,
            cancellation: CancellationToken::new(),
        })
        .await
        .unwrap();
    let _ = next_request(&mut received).await;
}

fn completion(cursor: u64, category: &str, job_key: &str, job_id: &str, kind: &str) -> Vec<u8> {
    let mut metadata = json!({
        "kind": kind, "repository_id": "r1234567890abcdef", (job_key): job_id,
    });
    let details = if category == "test" {
        json!({"worktree_id":"w1234567890abcdef", "test":"fixture", "status":"passed", "exit_code":0, "duration_seconds":1.0})
    } else {
        json!({"component":null, "state":"running"})
    };
    metadata
        .as_object_mut()
        .unwrap()
        .extend(details.as_object().unwrap().clone());
    serde_json::to_vec(&json!({
        "protocol": 2, "id": "fixture", "ok": true,
        "data": { "cursor": cursor, "heartbeat_due": [], "events": [{
            "filter_ids": ["codex"], "event": {
                "cursor": cursor, "occurred_at": "2026-09-09T19:55:11Z",
                "event": { "category": category, "data": metadata }
            }
        }]}
    }))
    .unwrap()
}

#[tokio::test]
async fn completion_before_registration_replays_the_source_journal() {
    let fixture = Fixture::new().await;
    let (requests, mut received) = mpsc::channel(8);
    let bridge = fixture.start(FixtureSource(requests));
    let bytes = completion(42, "test", "run_id", "run-a", "test.finished");
    let subscription = fixture
        .subscribe("run_id", "run-a", "test.finished", /*after*/ 41)
        .await;
    let request = next_request(&mut received).await;
    assert_eq!(request.cursor, Some(41));
    let batch = decode_envelope(&bytes, request.cursor).unwrap();
    let expected = batch.events[0].clone();
    request.reply.send(Ok(batch)).unwrap();
    let wake = timeout(
        Duration::from_secs(5),
        fixture
            .store
            .await_subscription(subscription.thread_id, subscription.id),
    )
    .await
    .unwrap()
    .unwrap();
    let event = wake.event.unwrap();
    assert_eq!(
        event,
        codex_event_subscriptions::EventMetadata {
            id: expected.id,
            source: expected.source,
            event_type: expected.event_type,
            cursor: expected.cursor,
            labels: expected.labels,
            occurred_at_ms: expected.occurred_at_ms,
            coalesced_event_count: 1,
        }
    );
    fixture.finish(bridge).await;
}

#[tokio::test]
async fn completion_after_registration_ignores_other_jobs_and_blocks_without_wakes() {
    let fixture = Fixture::new().await;
    let subscription = fixture
        .subscribe(
            "deployment_id",
            "deployment-a",
            "deployment.applied",
            /*after*/ 41,
        )
        .await;
    let (requests, mut received) = mpsc::channel(8);
    let bridge = fixture.start(FixtureSource(requests));
    let request = next_request(&mut received).await;
    let other = completion(
        42,
        "deployment",
        "deployment_id",
        "deployment-b",
        "deployment.applied",
    );
    request
        .reply
        .send(decode_envelope(&other, request.cursor))
        .unwrap();
    let request = next_request(&mut received).await;
    assert_eq!(request.cursor, Some(42));
    assert!(
        fixture
            .store
            .pending_wake(subscription.thread_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        timeout(Duration::from_millis(25), received.recv())
            .await
            .is_err()
    );
    assert_eq!(fixture.sink.0.load(Ordering::SeqCst), 0);
    let matching = completion(
        43,
        "deployment",
        "deployment_id",
        "deployment-a",
        "deployment.applied",
    );
    request
        .reply
        .send(decode_envelope(&matching, request.cursor))
        .unwrap();
    let wake = timeout(
        Duration::from_secs(5),
        fixture
            .store
            .await_subscription(subscription.thread_id, subscription.id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        wake.event.unwrap().labels,
        BTreeMap::from([
            ("repository_id".into(), "r1234567890abcdef".into()),
            ("deployment_id".into(), "deployment-a".into()),
        ])
    );
    fixture.finish(bridge).await;
}

#[tokio::test]
async fn new_subscription_rewinds_without_redelivering_to_another_run() {
    let fixture = Fixture::new().await;
    let first = fixture
        .subscribe("run_id", "run-a", "test.finished", /*after*/ 99)
        .await;
    let (requests, mut received) = mpsc::channel(8);
    let bridge = fixture.start(FixtureSource(requests));
    let original = next_request(&mut received).await;
    let second = fixture
        .subscribe("run_id", "run-b", "test.finished", /*after*/ 41)
        .await;
    timeout(Duration::from_secs(5), original.cancellation.cancelled())
        .await
        .unwrap();
    let request = next_request(&mut received).await;
    assert_eq!(request.cursor, Some(41));
    let bytes = completion(42, "test", "run_id", "run-b", "test.finished");
    request
        .reply
        .send(decode_envelope(&bytes, request.cursor))
        .unwrap();
    timeout(
        Duration::from_secs(5),
        fixture
            .store
            .await_subscription(second.thread_id, second.id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        fixture
            .store
            .pending_wake(first.thread_id)
            .await
            .unwrap()
            .is_none()
    );
    fixture.finish(bridge).await;
}

#[tokio::test]
async fn unavailable_source_recovers_without_advancing_the_cursor() {
    let fixture = Fixture::new().await;
    let subscription = fixture
        .subscribe("run_id", "run-a", "test.finished", /*after*/ 41)
        .await;
    let (requests, mut received) = mpsc::channel(8);
    let bridge = fixture.start(FixtureSource(requests));
    next_request(&mut received)
        .await
        .reply
        .send(Err(SourceError::Unavailable))
        .unwrap();
    let request = next_request(&mut received).await;
    assert_eq!(request.cursor, Some(41));
    let attention = fixture
        .store
        .pending_wake(subscription.thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        attention.wake.items[0].event.as_ref().unwrap().event_type,
        "source.unavailable"
    );
    request.reply.send(Err(SourceError::Unavailable)).unwrap();
    let request = next_request(&mut received).await;
    assert_eq!(request.cursor, Some(41));
    assert_eq!(
        fixture
            .store
            .pending_wake(subscription.thread_id)
            .await
            .unwrap(),
        Some(attention.clone())
    );
    assert_eq!(
        fixture.store.coordinator_subscriptions().await.unwrap()[0].source_cursor,
        subscription.source_cursor
    );
    fixture
        .store
        .acknowledge_wake(subscription.thread_id, attention.through_revision)
        .await
        .unwrap();
    let bytes = completion(42, "test", "run_id", "run-a", "test.finished");
    request
        .reply
        .send(decode_envelope(&bytes, request.cursor))
        .unwrap();
    timeout(
        Duration::from_secs(5),
        fixture
            .store
            .await_subscription(subscription.thread_id, subscription.id),
    )
    .await
    .unwrap()
    .unwrap();
    fixture.finish(bridge).await;
}

#[tokio::test]
async fn stale_cursor_reports_attention_without_poisoning_another_subscription() {
    let fixture = Fixture::new().await;
    let stale = fixture
        .subscribe("run_id", "run-a", "test.finished", /*after*/ 41)
        .await;
    let current = fixture
        .subscribe("run_id", "run-a", "test.finished", /*after*/ 99)
        .await;
    let (requests, mut received) = mpsc::channel(8);
    let bridge = fixture.start(FixtureSource(requests));
    next_request(&mut received)
        .await
        .reply
        .send(Err(SourceError::CursorStale))
        .unwrap();
    let waiting = next_request(&mut received).await;
    let attention = fixture
        .store
        .pending_wake(stale.thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        attention.wake.items[0].event.as_ref().unwrap().event_type,
        "source.cursor_stale"
    );
    assert!(
        fixture
            .store
            .pending_wake(current.thread_id)
            .await
            .unwrap()
            .is_none()
    );
    fixture.store.cancel(stale.id).await.unwrap();
    timeout(Duration::from_secs(5), waiting.cancellation.cancelled())
        .await
        .unwrap();
    let request = next_request(&mut received).await;
    assert_eq!(request.cursor, Some(99));
    let bytes = completion(100, "test", "run_id", "run-a", "test.finished");
    request
        .reply
        .send(decode_envelope(&bytes, request.cursor))
        .unwrap();
    let wake = timeout(
        Duration::from_secs(5),
        fixture
            .store
            .await_subscription(current.thread_id, current.id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(wake.event.unwrap().event_type, "test.finished");
    fixture.finish(bridge).await;
}

#[tokio::test]
async fn no_source_process_without_coordinator_subscriptions_and_last_cancel_stops_it() {
    let fixture = Fixture::new().await;
    let (requests, mut received) = mpsc::channel(8);
    let bridge = fixture.start(FixtureSource(requests));
    assert!(
        timeout(Duration::from_millis(25), received.recv())
            .await
            .is_err()
    );
    let subscription = fixture
        .subscribe("run_id", "run-a", "test.finished", /*after*/ 41)
        .await;
    let request = next_request(&mut received).await;
    fixture.store.cancel(subscription.id).await.unwrap();
    timeout(Duration::from_secs(5), request.cancellation.cancelled())
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_millis(25), received.recv())
            .await
            .is_err()
    );
    fixture.finish(bridge).await;
}

#[test]
fn metadata_projection_rejects_invalid_cursors_and_never_invents_repository_ids() {
    let bytes = completion(42, "test", "run_id", "run-a", "test.finished");
    assert_eq!(
        decode_envelope(&bytes, Some(42)).unwrap_err(),
        SourceError::InvalidEnvelope
    );
    let mut envelope: serde_json::Value = serde_json::from_slice(&completion(
        42,
        "deployment",
        "deployment_id",
        "deployment-a",
        "deployment.applied",
    ))
    .unwrap();
    let data = &mut envelope["data"]["events"][0]["event"]["event"]["data"];
    data["repository_id"] = serde_json::Value::Null;
    data["raw_log"] = json!("untrusted raw fixture output");
    let batch = decode_envelope(&serde_json::to_vec(&envelope).unwrap(), Some(41)).unwrap();
    assert_eq!(
        batch.events[0].labels,
        BTreeMap::from([("deployment_id".into(), "deployment-a".into())])
    );
    assert_eq!(
        decode_envelope(&vec![b' '; MAX_ENVELOPE_BYTES + 1], None).unwrap_err(),
        SourceError::InvalidEnvelope
    );
    let stale = br#"{"protocol":2,"ok":false,"error":{"code":"cursor_stale","message":"refresh required","detail":"not retained"}}"#;
    assert_eq!(
        decode_envelope(stale, Some(41)).unwrap_err(),
        SourceError::CursorStale
    );
}

#[test]
fn owned_child_fixture() {
    if std::env::var_os("CODEX_COORDINATOR_BRIDGE_CHILD").is_none() {
        return;
    }
    use std::io::Read;
    use std::io::Write;
    std::io::stdout()
        .write_all(b"bridge-child-ready\n")
        .unwrap();
    std::io::stdout().flush().unwrap();
    let mut input = [0_u8; 1];
    std::io::stdin().read_exact(&mut input).unwrap();
}

struct ChildSource {
    child: Child,
    started: Option<oneshot::Sender<()>>,
    stopped: mpsc::Sender<bool>,
}

impl EventSource for ChildSource {
    async fn wait(
        &mut self,
        cursor: Option<u64>,
        cancellation: CancellationToken,
    ) -> Result<SourceBatch, SourceError> {
        self.started.take().unwrap().send(()).unwrap();
        let result = read_child(&mut self.child, cursor, cancellation).await;
        self.stopped
            .send(self.child.try_wait().unwrap().is_some())
            .await
            .unwrap();
        result
    }
}

#[tokio::test]
async fn shutdown_joins_the_watcher_and_reaps_its_owned_child() {
    let fixture = Fixture::new().await;
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "coordinator_event_bridge::tests::owned_child_fixture",
            "--nocapture",
        ])
        .env("CODEX_COORDINATOR_BRIDGE_CHILD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let _input = child.stdin.take().unwrap();
    let mut output = Vec::new();
    timeout(Duration::from_secs(5), async {
        while !output.ends_with(b"bridge-child-ready\n") && output.len() < 1024 {
            output.push(child.stdout.as_mut().unwrap().read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert!(output.ends_with(b"bridge-child-ready\n"));
    let (started, ready) = oneshot::channel();
    let (stopped, mut reaped) = mpsc::channel(1);
    let bridge = fixture.start(ChildSource {
        child,
        started: Some(started),
        stopped,
    });
    fixture
        .subscribe("run_id", "run-a", "test.finished", /*after*/ 41)
        .await;
    timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    bridge.cancel();
    bridge.shutdown().await.unwrap();
    assert_eq!(reaped.recv().await, Some(true));
    fixture.finish(bridge).await;
}

#[tokio::test]
async fn coordinator_review_event_becomes_one_durable_alarm_without_a_worker() {
    let fixture = Fixture::new().await;
    let owner = ThreadId::new();
    fixture
        .store
        .ensure_event_route(
            owner,
            EventFilter {
                source: "devcoordinator".into(),
                event_types: BTreeSet::from([
                    "review.reminder".into(),
                    "source.unavailable".into(),
                    "source.cursor_stale".into(),
                ]),
                labels: BTreeMap::from([("owner_thread_id".into(), owner.to_string())]),
            },
            1000,
        )
        .await
        .unwrap();
    let subscriptions = fixture.store.coordinator_subscriptions().await.unwrap();
    let envelope = json!({"protocol":2,"ok":true,"data":{"cursor":42,"events":[{"event":{"cursor":42,"occurred_at":"2026-09-26T00:00:00Z","event":{"category":"other","data":{"kind":"review.reminder","review":{"version":1,"reminder_id":"review-7","repository_id":"r1234567890abcdef","workstream_id":"implementation","owner_thread_id":owner.to_string(),"alarm_namespace":"codex.review.v1","window_start_ms":1000,"window_end_ms":86401000,"due_at_ms":86401000,"last_completed_receipt":null,"escalation":false}}}}}]}});
    let (requests, mut received) = mpsc::channel(8);
    let bridge = fixture.start(FixtureSource(requests));
    let request = next_request(&mut received).await;
    let batch = decode_envelope(&serde_json::to_vec(&envelope).unwrap(), request.cursor).unwrap();
    let event = batch.events[0].clone();
    request.reply.send(Ok(batch)).unwrap();
    let subsequent = next_request(&mut received).await;
    assert_eq!(subsequent.cursor, Some(42));
    alarm::deliver(&fixture.store, &fixture.service, &subscriptions, &event)
        .await
        .unwrap();
    let page = fixture.store.list_alarms(owner, 0, 5).await.unwrap();
    assert_eq!(page.data.len(), 1);
    let alarm = &page.data[0];
    assert_eq!(alarm.spec.workstream_id.as_deref(), Some("implementation"));
    assert_eq!(alarm.spec.absolute_at_ms, Some(86401000));
    assert!(alarm.spec.summary.contains("review.record"));
    assert!(alarm.spec.summary.contains("from_at_ms=1000"));
    assert!(alarm.spec.summary.contains("explicit user override"));
    let wake = timeout(
        Duration::from_secs(5),
        fixture.store.await_subscription(owner, alarm.id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(wake.event.unwrap().source, "codex.alarm");
    assert!(
        fixture
            .store
            .project_status("r1234567890abcdef")
            .await
            .unwrap()
            .is_none()
    );
    fixture.finish(bridge).await;
}

struct RecoveringSource {
    recovered: Option<SourceBatch>,
    requests: mpsc::Sender<Request>,
}
impl EventSource for RecoveringSource {
    async fn reconcile(
        &mut self,
        _cancellation: CancellationToken,
    ) -> Result<Option<SourceBatch>, SourceError> {
        Ok(self.recovered.take())
    }
    async fn wait(
        &mut self,
        cursor: Option<u64>,
        cancellation: CancellationToken,
    ) -> Result<SourceBatch, SourceError> {
        FixtureSource(self.requests.clone())
            .wait(cursor, cancellation)
            .await
    }
}

#[tokio::test]
async fn review_reconciliation_preserves_another_subscription_cursor() {
    let fixture = Fixture::new().await;
    let other = fixture
        .subscribe("run_id", "run-existing", "test.finished", 41)
        .await;
    let owner = ThreadId::new();
    fixture
        .store
        .ensure_event_route(
            owner,
            EventFilter {
                source: "devcoordinator".into(),
                event_types: BTreeSet::from(["review.reminder".into()]),
                labels: BTreeMap::from([("owner_thread_id".into(), owner.to_string())]),
            },
            1000,
        )
        .await
        .unwrap();
    let reminder:alarm::ReviewReminder=serde_json::from_value(json!({"version":1,"reminder_id":"review-recovered","repository_id":"r1234567890abcdef","workstream_id":null,"owner_thread_id":owner.to_string(),"alarm_namespace":"codex.review.v1","window_start_ms":1000,"window_end_ms":86401000,"due_at_ms":90001000,"last_completed_receipt":null,"escalation":true})).unwrap();
    let event = reminder.event(50, "2026-09-26T00:00:00Z").unwrap();
    let (requests, mut received) = mpsc::channel(8);
    let bridge = fixture.start(RecoveringSource {
        recovered: Some(SourceBatch {
            cursor: 50,
            events: vec![event],
        }),
        requests,
    });
    let request = next_request(&mut received).await;
    assert_eq!(request.cursor, Some(41));
    assert_eq!(
        fixture
            .store
            .list_alarms(owner, 0, 5)
            .await
            .unwrap()
            .data
            .len(),
        1
    );
    request
        .reply
        .send(decode_envelope(
            &completion(43, "test", "run_id", "run-existing", "test.finished"),
            Some(41),
        ))
        .unwrap();
    let wake = timeout(
        Duration::from_secs(5),
        fixture.store.await_subscription(other.thread_id, other.id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(wake.event.unwrap().cursor.sequence, 43);
    fixture.finish(bridge).await;
}

#[tokio::test]
async fn native_cli_activation_is_lazy_and_uses_the_existing_file_watcher() {
    let fixture = Fixture::new().await;
    let home = AbsolutePathBuf::from_absolute_path(fixture._directory.path()).unwrap();
    let watcher = Arc::new(codex_file_watcher::FileWatcher::new().unwrap());
    let activation = activation::Activation::new(&home, watcher);
    let directory = home.join("alarm-activations");
    tokio::fs::create_dir(directory.as_path()).await.unwrap();
    tokio::fs::write(
        directory.join("unrelated.txt").as_path(),
        b"ordinary output",
    )
    .await
    .unwrap();
    activation.drain(&fixture.store).await.unwrap();
    assert!(
        fixture
            .store
            .coordinator_subscriptions()
            .await
            .unwrap()
            .is_empty()
    );
    let (requests, mut received) = mpsc::channel(8);
    let bridge = fixture.start_with_activation(FixtureSource(requests), Some(activation));
    assert!(
        timeout(Duration::from_millis(50), received.recv())
            .await
            .is_err()
    );
    let owner = ThreadId::new();
    let marker = directory.join(owner.to_string());
    tokio::fs::write(marker.as_path(), b"codex.alarm-route.v1\n")
        .await
        .unwrap();
    let request = next_request(&mut received).await;
    assert_eq!(request.cursor, None);
    assert!(!marker.as_path().exists());
    let routes = fixture.store.coordinator_subscriptions().await.unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].thread_id, owner);
    fixture.finish(bridge).await;
}
