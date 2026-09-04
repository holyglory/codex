use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_protocol::ThreadId;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::ListSubscriptionsQuery;
use crate::NewSubscription;
use crate::PendingWakeBatch;
use crate::PublishEventOutcome;
use crate::PublishedEvent;
use crate::Subscription;
use crate::SubscriptionPage;
use crate::TriggerOutcome;
use crate::ValidationError;
use crate::WakeBatch;
use crate::types::validate_trigger_ids;

const COMMAND_CAPACITY: usize = 256;
const RETRY_DELAY_MS: i64 = 1_000;
const MAX_RETRY_DELAY_MS: i64 = 60_000;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StoreError {
    #[error("the subscription store is unavailable: {0}")]
    Unavailable(String),
    #[error("the maximum number of event subscriptions has been reached")]
    TotalCapacity,
    #[error("the thread already has the maximum number of event subscriptions")]
    ThreadCapacity,
    #[error("stored event subscription data is invalid")]
    InvalidData,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ServiceError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the event subscription scheduler is unavailable")]
    SchedulerUnavailable,
}

/// Durable storage contract used by the shared subscription scheduler.
///
/// Implementations must make event cursor advancement and pending-wake creation
/// atomic, keep acknowledgement revision-aware, and return all due heartbeats
/// in one collection operation.
pub trait EventSubscriptionStore: Clone + Send + Sync + 'static {
    fn create(
        &self,
        subscription: NewSubscription,
        now_ms: i64,
    ) -> impl Future<Output = Result<Subscription, StoreError>> + Send;

    fn list(
        &self,
        query: ListSubscriptionsQuery,
    ) -> impl Future<Output = Result<SubscriptionPage, StoreError>> + Send;

    fn cancel(&self, id: Uuid) -> impl Future<Output = Result<bool, StoreError>> + Send;

    fn publish(
        &self,
        event: PublishedEvent,
        now_ms: i64,
    ) -> impl Future<Output = Result<PublishEventOutcome, StoreError>> + Send;

    fn trigger(
        &self,
        subscription_ids: Vec<Uuid>,
        now_ms: i64,
    ) -> impl Future<Output = Result<TriggerOutcome, StoreError>> + Send;

    fn collect_due_heartbeats(
        &self,
        now_ms: i64,
    ) -> impl Future<Output = Result<Vec<ThreadId>, StoreError>> + Send;

    fn next_heartbeat_deadline(
        &self,
    ) -> impl Future<Output = Result<Option<i64>, StoreError>> + Send;

    fn pending_thread_ids(&self) -> impl Future<Output = Result<Vec<ThreadId>, StoreError>> + Send;

    fn pending_wake(
        &self,
        thread_id: ThreadId,
    ) -> impl Future<Output = Result<Option<PendingWakeBatch>, StoreError>> + Send;

    fn acknowledge_wake(
        &self,
        thread_id: ThreadId,
        through_revision: i64,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeDisposition {
    Started,
    DeferredUntilIdle,
}

/// Host bridge that resolves the target thread and submits one automatic turn.
///
/// Implementations return `DeferredUntilIdle` when the thread is already active;
/// they must not steer an event notification into the active model call.
pub trait WakeSink: Clone + Send + Sync + 'static {
    fn wake(&self, wake: WakeBatch)
    -> impl Future<Output = Result<WakeDisposition, String>> + Send;
}

/// Clock abstraction used by the one shared deadline scheduler.
pub trait Clock: Clone + Send + Sync + 'static {
    fn now_ms(&self) -> i64;

    fn sleep_until(&self, deadline_ms: i64) -> impl Future<Output = ()> + Send;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or_default()
    }

    fn sleep_until(&self, deadline_ms: i64) -> impl Future<Output = ()> + Send {
        let delay_ms = deadline_ms.saturating_sub(self.now_ms());
        async move {
            tokio::time::sleep(Duration::from_millis(
                u64::try_from(delay_ms).unwrap_or_default(),
            ))
            .await;
        }
    }
}

#[derive(Clone)]
pub struct EventSubscriptionService {
    command_tx: mpsc::Sender<Command>,
}

enum Command {
    Create {
        subscription: NewSubscription,
        response: oneshot::Sender<Result<Subscription, ServiceError>>,
    },
    List {
        query: ListSubscriptionsQuery,
        response: oneshot::Sender<Result<SubscriptionPage, ServiceError>>,
    },
    Cancel {
        id: Uuid,
        response: oneshot::Sender<Result<bool, ServiceError>>,
    },
    Publish {
        event: PublishedEvent,
        response: oneshot::Sender<Result<PublishEventOutcome, ServiceError>>,
    },
    Trigger {
        subscription_ids: Vec<Uuid>,
        response: oneshot::Sender<Result<TriggerOutcome, ServiceError>>,
    },
    ThreadReady(ThreadId),
    Shutdown(oneshot::Sender<()>),
}

struct DispatchFinished {
    thread_id: ThreadId,
    through_revision: i64,
    result: Result<WakeDisposition, String>,
}

#[derive(Clone, Copy)]
struct RetryState {
    retry_at_ms: i64,
    failures: u32,
}

impl EventSubscriptionService {
    pub fn spawn<S, W, C>(store: S, wake_sink: W, clock: C) -> Self
    where
        S: EventSubscriptionStore,
        W: WakeSink,
        C: Clock,
    {
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        tokio::spawn(run_scheduler(store, wake_sink, clock, command_rx));
        Self { command_tx }
    }

    pub async fn create(
        &self,
        subscription: NewSubscription,
    ) -> Result<Subscription, ServiceError> {
        let (response, receiver) = oneshot::channel();
        self.send(Command::Create {
            subscription,
            response,
        })
        .await?;
        receiver
            .await
            .map_err(|_| ServiceError::SchedulerUnavailable)?
    }

    pub async fn list(
        &self,
        query: ListSubscriptionsQuery,
    ) -> Result<SubscriptionPage, ServiceError> {
        let (response, receiver) = oneshot::channel();
        self.send(Command::List { query, response }).await?;
        receiver
            .await
            .map_err(|_| ServiceError::SchedulerUnavailable)?
    }

    pub async fn cancel(&self, id: Uuid) -> Result<bool, ServiceError> {
        let (response, receiver) = oneshot::channel();
        self.send(Command::Cancel { id, response }).await?;
        receiver
            .await
            .map_err(|_| ServiceError::SchedulerUnavailable)?
    }

    pub async fn publish(
        &self,
        event: PublishedEvent,
    ) -> Result<PublishEventOutcome, ServiceError> {
        let (response, receiver) = oneshot::channel();
        self.send(Command::Publish { event, response }).await?;
        receiver
            .await
            .map_err(|_| ServiceError::SchedulerUnavailable)?
    }

    pub async fn trigger(
        &self,
        subscription_ids: Vec<Uuid>,
    ) -> Result<TriggerOutcome, ServiceError> {
        let (response, receiver) = oneshot::channel();
        self.send(Command::Trigger {
            subscription_ids,
            response,
        })
        .await?;
        receiver
            .await
            .map_err(|_| ServiceError::SchedulerUnavailable)?
    }

    pub fn notify_thread_ready(&self, thread_id: ThreadId) {
        match self.command_tx.try_send(Command::ThreadReady(thread_id)) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(command)) => {
                let command_tx = self.command_tx.clone();
                tokio::spawn(async move {
                    let _ = command_tx.send(command).await;
                });
            }
        }
    }

    pub async fn shutdown(&self) {
        let (response, receiver) = oneshot::channel();
        if self
            .command_tx
            .send(Command::Shutdown(response))
            .await
            .is_ok()
        {
            let _ = receiver.await;
        }
    }

    async fn send(&self, command: Command) -> Result<(), ServiceError> {
        self.command_tx
            .send(command)
            .await
            .map_err(|_| ServiceError::SchedulerUnavailable)
    }
}

async fn run_scheduler<S, W, C>(
    store: S,
    wake_sink: W,
    clock: C,
    mut command_rx: mpsc::Receiver<Command>,
) where
    S: EventSubscriptionStore,
    W: WakeSink,
    C: Clock,
{
    let mut requested_threads = HashSet::new();
    let mut deferred_threads = HashSet::new();
    let mut in_flight_threads = HashSet::new();
    let mut ready_threads = HashSet::new();
    let mut retries = HashMap::<ThreadId, RetryState>::new();
    let mut dispatches = JoinSet::<DispatchFinished>::new();
    match store.pending_thread_ids().await {
        Ok(thread_ids) => requested_threads.extend(thread_ids),
        Err(error) => tracing::warn!(%error, "failed to restore pending subscription wakes"),
    }

    loop {
        let heartbeat_deadline = match store.next_heartbeat_deadline().await {
            Ok(deadline) => deadline,
            Err(error) => {
                tracing::warn!(%error, "failed to read the next subscription heartbeat");
                Some(clock.now_ms().saturating_add(RETRY_DELAY_MS))
            }
        };
        let retry_deadline = retries
            .values()
            .map(|retry| retry.retry_at_ms)
            .filter(|retry_at_ms| *retry_at_ms != i64::MAX)
            .min();
        let stored_deadline = match (heartbeat_deadline, retry_deadline) {
            (Some(heartbeat), Some(retry)) => Some(heartbeat.min(retry)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        };
        let dispatch_ready = requested_threads.iter().any(|thread_id| {
            !deferred_threads.contains(thread_id)
                && !in_flight_threads.contains(thread_id)
                && retries
                    .get(thread_id)
                    .is_none_or(|retry| retry.retry_at_ms == i64::MAX)
        });
        let next_deadline = if dispatch_ready {
            Some(
                stored_deadline
                    .map_or_else(|| clock.now_ms(), |deadline| deadline.min(clock.now_ms())),
            )
        } else {
            stored_deadline
        };
        let timer = async {
            match next_deadline {
                Some(deadline) => clock.sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(timer);

        let mut shutdown = false;
        tokio::select! {
            biased;
            command = command_rx.recv() => {
                let Some(command) = command else {
                    break;
                };
                shutdown = handle_command(
                    &store,
                    &clock,
                    command,
                    &mut requested_threads,
                    &mut deferred_threads,
                    &mut retries,
                    &mut ready_threads,
                ).await;
            }
            completion = dispatches.join_next(), if !dispatches.is_empty() => {
                if let Some(Ok(completion)) = completion {
                    in_flight_threads.remove(&completion.thread_id);
                    handle_dispatch_finished(
                        &store,
                        &clock,
                        completion,
                        &mut requested_threads,
                        &mut deferred_threads,
                        &mut retries,
                        &mut ready_threads,
                    ).await;
                }
            }
            () = &mut timer => {}
        }

        while !shutdown {
            let Ok(command) = command_rx.try_recv() else {
                break;
            };
            shutdown = handle_command(
                &store,
                &clock,
                command,
                &mut requested_threads,
                &mut deferred_threads,
                &mut retries,
                &mut ready_threads,
            )
            .await;
        }
        if shutdown {
            dispatches.abort_all();
            break;
        }

        let now_ms = clock.now_ms();
        match store.collect_due_heartbeats(now_ms).await {
            Ok(thread_ids) => requested_threads.extend(thread_ids),
            Err(error) => tracing::warn!(%error, "failed to collect due subscription heartbeats"),
        }
        let due_retries = retries
            .iter()
            .filter_map(|(thread_id, retry)| (retry.retry_at_ms <= now_ms).then_some(*thread_id))
            .collect::<Vec<_>>();
        for thread_id in due_retries {
            if let Some(retry) = retries.get_mut(&thread_id) {
                retry.retry_at_ms = i64::MAX;
            }
            requested_threads.insert(thread_id);
        }

        let launchable = requested_threads
            .iter()
            .copied()
            .filter(|thread_id| !deferred_threads.contains(thread_id))
            .filter(|thread_id| !in_flight_threads.contains(thread_id))
            .filter(|thread_id| {
                retries
                    .get(thread_id)
                    .is_none_or(|retry| retry.retry_at_ms == i64::MAX)
            })
            .collect::<Vec<_>>();
        for thread_id in launchable {
            requested_threads.remove(&thread_id);
            let batch = match store.pending_wake(thread_id).await {
                Ok(Some(batch)) => batch,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(%thread_id, %error, "failed to load pending subscription wake");
                    schedule_retry(&mut retries, thread_id, now_ms);
                    continue;
                }
            };
            let wake_sink = wake_sink.clone();
            ready_threads.remove(&thread_id);
            in_flight_threads.insert(thread_id);
            dispatches.spawn(async move {
                let result = wake_sink.wake(batch.wake).await;
                DispatchFinished {
                    thread_id,
                    through_revision: batch.through_revision,
                    result,
                }
            });
        }
    }
}

async fn handle_command<S, C>(
    store: &S,
    clock: &C,
    command: Command,
    requested_threads: &mut HashSet<ThreadId>,
    deferred_threads: &mut HashSet<ThreadId>,
    retries: &mut HashMap<ThreadId, RetryState>,
    ready_threads: &mut HashSet<ThreadId>,
) -> bool
where
    S: EventSubscriptionStore,
    C: Clock,
{
    let now_ms = clock.now_ms();
    match command {
        Command::Create {
            subscription,
            response,
        } => {
            let result = subscription.validate(now_ms).map_err(ServiceError::from);
            let result = match result {
                Ok(()) => store
                    .create(subscription, now_ms)
                    .await
                    .map_err(ServiceError::from),
                Err(error) => Err(error),
            };
            let _ = response.send(result);
        }
        Command::List { query, response } => {
            let result = query.validate().map_err(ServiceError::from);
            let result = match result {
                Ok(()) => store.list(query).await.map_err(ServiceError::from),
                Err(error) => Err(error),
            };
            let _ = response.send(result);
        }
        Command::Cancel { id, response } => {
            let _ = response.send(store.cancel(id).await.map_err(ServiceError::from));
        }
        Command::Publish { event, response } => {
            let result = event.validate().map_err(ServiceError::from);
            let result = match result {
                Ok(()) => store
                    .publish(event, now_ms)
                    .await
                    .map_err(ServiceError::from),
                Err(error) => Err(error),
            };
            if let Ok(outcome) = &result {
                requested_threads.extend(outcome.affected_thread_ids.iter().copied());
            }
            let _ = response.send(result);
        }
        Command::Trigger {
            mut subscription_ids,
            response,
        } => {
            subscription_ids.sort_unstable();
            subscription_ids.dedup();
            let result = validate_trigger_ids(&subscription_ids).map_err(ServiceError::from);
            let result = match result {
                Ok(()) => store
                    .trigger(subscription_ids, now_ms)
                    .await
                    .map_err(ServiceError::from),
                Err(error) => Err(error),
            };
            if let Ok(outcome) = &result {
                requested_threads.extend(outcome.affected_thread_ids.iter().copied());
            }
            let _ = response.send(result);
        }
        Command::ThreadReady(thread_id) => {
            ready_threads.insert(thread_id);
            deferred_threads.remove(&thread_id);
            retries.remove(&thread_id);
            requested_threads.insert(thread_id);
        }
        Command::Shutdown(response) => {
            let _ = response.send(());
            return true;
        }
    }
    false
}

async fn handle_dispatch_finished<S, C>(
    store: &S,
    clock: &C,
    completion: DispatchFinished,
    requested_threads: &mut HashSet<ThreadId>,
    deferred_threads: &mut HashSet<ThreadId>,
    retries: &mut HashMap<ThreadId, RetryState>,
    ready_threads: &mut HashSet<ThreadId>,
) where
    S: EventSubscriptionStore,
    C: Clock,
{
    let DispatchFinished {
        thread_id,
        through_revision,
        result,
    } = completion;
    match result {
        Ok(WakeDisposition::Started) => {
            deferred_threads.remove(&thread_id);
            retries.remove(&thread_id);
            if let Err(error) = store.acknowledge_wake(thread_id, through_revision).await {
                tracing::warn!(%thread_id, %error, "failed to acknowledge subscription wake");
                schedule_retry(retries, thread_id, clock.now_ms());
            } else if matches!(store.pending_wake(thread_id).await, Ok(Some(_))) {
                requested_threads.insert(thread_id);
            }
        }
        Ok(WakeDisposition::DeferredUntilIdle) => {
            retries.remove(&thread_id);
            if ready_threads.remove(&thread_id) {
                requested_threads.insert(thread_id);
            } else {
                deferred_threads.insert(thread_id);
                requested_threads.remove(&thread_id);
            }
        }
        Err(error) => {
            tracing::warn!(%thread_id, %error, "failed to dispatch subscription wake");
            schedule_retry(retries, thread_id, clock.now_ms());
        }
    }
}

fn schedule_retry(retries: &mut HashMap<ThreadId, RetryState>, thread_id: ThreadId, now_ms: i64) {
    let failures = retries
        .get(&thread_id)
        .map_or(1, |retry| retry.failures.saturating_add(1));
    let shift = failures.saturating_sub(1).min(6);
    let delay_ms = RETRY_DELAY_MS
        .checked_shl(shift)
        .unwrap_or(MAX_RETRY_DELAY_MS)
        .min(MAX_RETRY_DELAY_MS);
    retries.insert(
        thread_id,
        RetryState {
            retry_at_ms: now_ms.saturating_add(delay_ms),
            failures,
        },
    );
}
