#[path = "coordinator_activation.rs"]
mod activation;
#[path = "coordinator_alarm.rs"]
mod alarm;
use chrono::DateTime;
use codex_event_subscriptions::EventSubscriptionService;
use codex_event_subscriptions::PublishedEvent;
use codex_event_subscriptions::SourceCursor;
use codex_event_subscriptions::Subscription;
use codex_state::SqliteEventSubscriptionStore;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task::JoinError;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_ENVELOPE_BYTES: usize = 65_536;
const RETRY_INITIAL: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Owns a read-only watcher, with parent-death containment on Linux and Windows.
/// Other platforms guarantee cleanup only through cancellation or Rust drop.
pub(crate) struct CoordinatorEventBridge {
    store: Arc<SqliteEventSubscriptionStore>,
    cancellation: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl CoordinatorEventBridge {
    pub(crate) fn spawn(
        store: Arc<SqliteEventSubscriptionStore>,
        service: Arc<EventSubscriptionService>,
        home: codex_utils_absolute_path::AbsolutePathBuf,
        file_watcher: Arc<codex_file_watcher::FileWatcher>,
    ) -> Self {
        let source_installed = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|directory| {
                directory
                    .join(if cfg!(windows) {
                        "devcoordinator2.exe"
                    } else {
                        "devcoordinator2"
                    })
                    .is_file()
            })
        });
        store.set_alarm_delivery_available(source_installed && file_watcher.is_live());
        let activation = activation::Activation::new(&home, file_watcher);
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run_bridge(
            Arc::clone(&store),
            service,
            CliEventSource,
            cancellation.clone(),
            Some(activation),
        ));
        Self {
            store,
            cancellation,
            task: Mutex::new(Some(task)),
        }
    }

    pub(crate) fn cancel(&self) {
        self.store.set_alarm_delivery_available(false);
        self.cancellation.cancel();
    }

    pub(crate) async fn shutdown(&self) -> Result<(), JoinError> {
        self.cancel();
        let task = self.task.lock().await.take();
        match task {
            Some(task) => tokio_util::task::AbortOnDropHandle::new(task).await,
            None => Ok(()),
        }
    }
}

impl Drop for CoordinatorEventBridge {
    fn drop(&mut self) {
        self.cancel();
        if let Some(task) = self.task.get_mut().take() {
            task.abort();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceError {
    Unavailable,
    InvalidEnvelope,
    CursorStale,
    Cancelled,
}

/// Read-only source transport; cancellation must stop and reap owned children.
/// Implementations return only bounded event metadata, never source diagnostics.
trait EventSource: Send + 'static {
    /// Reconcile current native obligations without discarding other event cursors.
    fn reconcile(
        &mut self,
        _cancellation: CancellationToken,
    ) -> impl Future<Output = Result<Option<SourceBatch>, SourceError>> + Send {
        std::future::ready(Ok(None))
    }

    fn wait(
        &mut self,
        cursor: Option<u64>,
        cancellation: CancellationToken,
    ) -> impl Future<Output = Result<SourceBatch, SourceError>> + Send;
}

struct CliEventSource;

impl EventSource for CliEventSource {
    async fn reconcile(
        &mut self,
        cancellation: CancellationToken,
    ) -> Result<Option<SourceBatch>, SourceError> {
        alarm::reconcile(&cancellation).await
    }

    async fn wait(
        &mut self,
        cursor: Option<u64>,
        cancellation: CancellationToken,
    ) -> Result<SourceBatch, SourceError> {
        let mut command = Command::new("devcoordinator2");
        command.env_remove("DEVCOORDINATOR_WORK_CONTEXT");
        #[cfg(target_os = "linux")]
        {
            let parent_pid =
                i32::try_from(std::process::id()).map_err(|_| SourceError::Unavailable)?;
            unsafe {
                command.pre_exec(move || {
                    codex_utils_pty::process_group::set_parent_death_signal(parent_pid)
                });
            }
        }
        command.args([
            "event",
            "wait",
            "--client",
            "codex",
            "--format",
            "json",
            "--limit",
            "16",
            "--filter",
            r#"{"filter_id":"codex","categories":["test","deployment","other"]}"#,
        ]);
        if let Some(cursor) = cursor {
            command.args(["--cursor", &cursor.to_string()]);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        let job = codex_utils_pty::JobObject::create().map_err(|_| SourceError::Unavailable)?;
        #[cfg(windows)]
        let mut child = job
            .spawn_contained(&mut command)
            .map_err(|_| SourceError::Unavailable)?;
        #[cfg(not(windows))]
        let mut child = command.spawn().map_err(|_| SourceError::Unavailable)?;
        read_child(&mut child, cursor, cancellation).await
    }
}

async fn read_child(
    child: &mut Child,
    cursor: Option<u64>,
    cancellation: CancellationToken,
) -> Result<SourceBatch, SourceError> {
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(SourceError::Cancelled),
        result = async {
            let stdout = child.stdout.take().ok_or(SourceError::Unavailable)?;
            let mut bytes = Vec::new();
            stdout.take((MAX_ENVELOPE_BYTES + 1) as u64)
                .read_to_end(&mut bytes).await.map_err(|_| SourceError::Unavailable)?;
            if bytes.len() > MAX_ENVELOPE_BYTES {
                return Err(SourceError::InvalidEnvelope);
            }
            let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await.map_err(|_| SourceError::Unavailable)?
                .map_err(|_| SourceError::Unavailable)?;
            let batch = decode_envelope(&bytes, cursor)?;
            if !status.success() {
                return Err(SourceError::Unavailable);
            }
            Ok(batch)
        } => result,
    };
    if child.id().is_some() {
        child.kill().await.map_err(|_| SourceError::Unavailable)?;
    }
    result
}

#[derive(Deserialize)]
struct Envelope {
    protocol: u8,
    ok: bool,
    data: Option<WaitResult>,
    error: Option<SourceFailure>,
}

#[derive(Deserialize)]
struct SourceFailure {
    code: String,
}

#[derive(Deserialize)]
struct WaitResult {
    cursor: u64,
    events: Vec<EventDelivery>,
}

#[derive(Deserialize)]
struct EventDelivery {
    event: OwnedEventRecord,
}

#[derive(Deserialize)]
struct OwnedEventRecord {
    cursor: u64,
    occurred_at: String,
    event: OwnedEvent,
}

#[derive(Deserialize)]
#[serde(tag = "category", content = "data", rename_all = "snake_case")]
enum OwnedEvent {
    Test {
        kind: String,
        repository_id: String,
        run_id: String,
    },
    Deployment {
        kind: String,
        repository_id: Option<String>,
        deployment_id: String,
    },
    Other {
        kind: String,
        review: Option<alarm::ReviewReminder>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug)]
struct SourceBatch {
    cursor: u64,
    events: Vec<PublishedEvent>,
}

fn decode_envelope(bytes: &[u8], after: Option<u64>) -> Result<SourceBatch, SourceError> {
    if bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(SourceError::InvalidEnvelope);
    }
    let envelope: Envelope =
        serde_json::from_slice(bytes).map_err(|_| SourceError::InvalidEnvelope)?;
    if envelope.protocol != 2 {
        return Err(SourceError::InvalidEnvelope);
    }
    if !envelope.ok {
        return Err(match envelope.error.map(|error| error.code).as_deref() {
            Some("cursor_stale") => SourceError::CursorStale,
            _ => SourceError::Unavailable,
        });
    }
    let data = envelope.data.ok_or(SourceError::InvalidEnvelope)?;
    if data.events.is_empty() || data.events.len() > 16 {
        return Err(SourceError::InvalidEnvelope);
    }
    let mut previous = after.unwrap_or_default();
    let mut events = Vec::new();
    for delivery in data.events {
        let record = delivery.event;
        if record.cursor <= previous || record.cursor > data.cursor {
            return Err(SourceError::InvalidEnvelope);
        }
        previous = record.cursor;
        let (kind, repository_id, job_key, job_id) = match record.event {
            OwnedEvent::Test {
                kind,
                repository_id,
                run_id,
            } => (kind, Some(repository_id), "run_id", run_id),
            OwnedEvent::Deployment {
                kind,
                repository_id,
                deployment_id,
            } => (kind, repository_id, "deployment_id", deployment_id),
            OwnedEvent::Other {
                kind,
                review: Some(reminder),
            } if kind == "review.reminder" => {
                events.push(reminder.event(record.cursor, &record.occurred_at)?);
                continue;
            }
            OwnedEvent::Other { .. } | OwnedEvent::Unknown => continue,
        };
        let mut labels = BTreeMap::from([(job_key.to_owned(), job_id)]);
        if let Some(repository_id) = repository_id {
            labels.insert("repository_id".to_owned(), repository_id);
        }
        let event = PublishedEvent {
            id: record.cursor.to_string(),
            source: "devcoordinator".to_owned(),
            event_type: kind,
            cursor: SourceCursor {
                sequence: record.cursor,
                value: None,
            },
            labels,
            occurred_at_ms: DateTime::parse_from_rfc3339(&record.occurred_at)
                .map_err(|_| SourceError::InvalidEnvelope)?
                .timestamp_millis(),
        };
        event.validate().map_err(|_| SourceError::InvalidEnvelope)?;
        events.push(event);
    }
    if previous != data.cursor {
        return Err(SourceError::InvalidEnvelope);
    }
    Ok(SourceBatch {
        cursor: data.cursor,
        events,
    })
}

async fn run_bridge(
    store: Arc<SqliteEventSubscriptionStore>,
    service: Arc<EventSubscriptionService>,
    mut source: impl EventSource,
    cancellation: CancellationToken,
    mut activation: Option<activation::Activation>,
) {
    let mut progress = BTreeMap::<Uuid, Option<u64>>::new();
    let mut subscriptions = Vec::<Subscription>::new();
    let mut reported = BTreeMap::new();
    let mut retry = RETRY_INITIAL;
    let mut last_error = None;
    loop {
        let result = async {
            subscriptions = tokio::select! {
                _ = cancellation.cancelled() => return Err(SourceError::Cancelled),
                result = store.coordinator_subscriptions() => {
                    result.map_err(|_| SourceError::Unavailable)?
                }
            };
            if let Some(activation) = &activation
                && let Err(error) = activation.drain(&store).await
            {
                tracing::warn!(
                    ?error,
                    "native route activation unavailable; clients retain message delivery"
                );
            }
            let active: HashSet<_> = subscriptions
                .iter()
                .map(|subscription| subscription.id)
                .collect();
            progress.retain(|id, _| active.contains(id));
            reported.retain(|id, _| active.contains(id));
            for subscription in &subscriptions {
                progress.entry(subscription.id).or_insert_with(|| {
                    subscription
                        .source_cursor
                        .as_ref()
                        .map(|cursor| cursor.sequence)
                });
            }
            let unresolved_native = subscriptions
                .iter()
                .filter(|subscription| {
                    progress.get(&subscription.id) == Some(&None)
                        && subscription
                            .filter
                            .as_ref()
                            .is_some_and(|filter| filter.event_types.contains("review.reminder"))
                })
                .map(|subscription| subscription.id)
                .collect::<Vec<_>>();
            if !unresolved_native.is_empty()
                && let Some(batch) = source.reconcile(cancellation.child_token()).await?
            {
                for event in batch.events {
                    alarm::deliver(&store, &service, &subscriptions, &event).await?;
                }
                for id in unresolved_native {
                    store
                        .advance_event_route(id, batch.cursor)
                        .await
                        .map_err(|_| SourceError::Unavailable)?;
                    progress.insert(id, Some(batch.cursor));
                }
            }
            if progress.is_empty() {
                tokio::select! {
                    _ = cancellation.cancelled() => return Err(SourceError::Cancelled),
                    _ = store.await_source_change() => return Ok(()),
                    _ = activation::changed(&mut activation) => return Ok(()),
                }
            }
            let cursor = progress.values().flatten().copied().min();
            let request_cancel = cancellation.child_token();
            let waiting = async {
                if request_cancel.is_cancelled() {
                    return Err(SourceError::Cancelled);
                }
                source.wait(cursor, request_cancel.clone()).await
            };
            tokio::pin!(waiting);
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    request_cancel.cancel();
                    let _ = waiting.await;
                    return Err(SourceError::Cancelled);
                }
                _ = activation::changed(&mut activation) => {
                    request_cancel.cancel();
                    let _ = waiting.await;
                    return Ok(());
                }
                _ = store.await_source_change() => {
                    request_cancel.cancel();
                    let _ = waiting.await;
                    return Ok(());
                }
                result = &mut waiting => result,
            };
            let batch = result?;
            for event in batch.events {
                if event.event_type == "review.reminder" {
                    alarm::deliver(&store, &service, &subscriptions, &event).await?;
                    continue;
                }
                if subscriptions.iter().any(|subscription| {
                    subscription
                        .filter
                        .as_ref()
                        .is_some_and(|filter| filter.matches(&event))
                }) {
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err(SourceError::Cancelled),
                        result = service.publish(event) => {
                            result.map_err(|_| SourceError::Unavailable)?;
                        }
                    }
                }
            }
            for cursor in progress.values_mut() {
                *cursor = Some(cursor.unwrap_or_default().max(batch.cursor));
            }
            reported.clear();
            retry = RETRY_INITIAL;
            last_error = None;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {}
            Err(SourceError::Cancelled) => return,
            Err(error) => {
                let kind = match error {
                    SourceError::CursorStale => "source.cursor_stale",
                    SourceError::Unavailable | SourceError::InvalidEnvelope => "source.unavailable",
                    SourceError::Cancelled => return,
                };
                let lowest = progress.values().flatten().copied().min();
                for subscription in &subscriptions {
                    if reported.get(&subscription.id) == Some(&kind)
                        || (error == SourceError::CursorStale
                            && progress.get(&subscription.id).copied().flatten() != lowest)
                    {
                        continue;
                    }
                    let Some(filter) = &subscription.filter else {
                        continue;
                    };
                    let attention = PublishedEvent {
                        id: Uuid::now_v7().to_string(),
                        source: "devcoordinator".to_owned(),
                        event_type: kind.to_owned(),
                        cursor: subscription.source_cursor.clone().unwrap_or(SourceCursor {
                            sequence: 0,
                            value: None,
                        }),
                        labels: filter.labels.clone(),
                        occurred_at_ms: chrono::Utc::now().timestamp_millis(),
                    };
                    let published = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        result = store.publish_source_attention_to(subscription.id, attention, chrono::Utc::now().timestamp_millis()) => result,
                    };
                    if let Ok(outcome) = published {
                        for owner in outcome.affected_thread_ids {
                            service.notify_thread_ready(owner);
                        }
                        reported.insert(subscription.id, kind);
                        for id in outcome.accepted_subscription_ids {
                            reported.insert(id, kind);
                        }
                    }
                }
                if last_error != Some(error) {
                    tracing::warn!(?error, "Coordinator event source unavailable");
                    last_error = Some(error);
                }
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = store.await_source_change() => {},
                    _ = tokio::time::sleep(retry) => {},
                    _ = activation::changed(&mut activation) => {},
                }
                retry = retry.saturating_mul(2).min(RETRY_MAX);
            }
        }
    }
}

#[cfg(test)]
#[path = "coordinator_event_bridge_tests.rs"]
mod tests;
