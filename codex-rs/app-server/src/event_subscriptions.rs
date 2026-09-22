use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::Ordering;

use codex_core::NotSubmittedReason;
use codex_core::ThreadManager;
use codex_event_subscriptions::CAPACITY_RETRY_EVENT_TYPE;
use codex_event_subscriptions::CAPACITY_RETRY_SOURCE;
use codex_event_subscriptions::EventFilter;
use codex_event_subscriptions::EventSubscriptionService;
use codex_event_subscriptions::EventSubscriptionStore;
use codex_event_subscriptions::HeartbeatSpec;
use codex_event_subscriptions::NewSubscription;
use codex_event_subscriptions::UserStartedSubscriptionWork;
use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeDisposition;
use codex_event_subscriptions::WakeReason;
use codex_event_subscriptions::WakeSink;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ThreadIdleInput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadReadyInput;
use codex_extension_api::ThreadResumeInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_protocol::ThreadId;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use uuid::Uuid;

use crate::request_processors::ThreadRequestProcessor;

const CAPACITY_RETRY_MIN_DELAY_SECONDS: i64 = 30;
const CAPACITY_RETRY_JITTER_SECONDS: i64 = 60;

fn capacity_retry_delay_seconds() -> i64 {
    CAPACITY_RETRY_MIN_DELAY_SECONDS
        + i64::from(Uuid::now_v7().as_bytes()[15]) % CAPACITY_RETRY_JITTER_SECONDS
}

fn is_guardian_review_source(source: &SessionSource) -> bool {
    matches!(
        source,
        SessionSource::Internal(InternalSessionSource::Guardian)
    ) || matches!(
        source,
        SessionSource::SubAgent(SubAgentSource::Other(label)) if label == "guardian"
    )
}

#[derive(Clone)]
pub(crate) struct AppServerSubscriptionWakeSink {
    thread_manager: Weak<ThreadManager>,
    background_loader: Arc<OnceLock<Weak<ThreadRequestProcessor>>>,
    store: codex_state::SqliteEventSubscriptionStore,
}

impl AppServerSubscriptionWakeSink {
    pub(crate) fn new(
        thread_manager: Weak<ThreadManager>,
        background_loader: Arc<OnceLock<Weak<ThreadRequestProcessor>>>,
        store: codex_state::SqliteEventSubscriptionStore,
    ) -> Self {
        Self {
            thread_manager,
            background_loader,
            store,
        }
    }

    async fn select_wakes(
        &self,
        wake: &WakeBatch,
        running: bool,
    ) -> Result<
        (
            Vec<codex_event_subscriptions::WakeItem>,
            Vec<codex_event_subscriptions::WakeItem>,
        ),
        String,
    > {
        let policy = self
            .store
            .read_wake_policy(wake.thread_id)
            .await
            .map_err(|error| error.to_string())?;
        let pending = self
            .store
            .pending_wake(wake.thread_id)
            .await
            .map_err(|error| error.to_string())?;
        let mut eligible = Vec::new();
        let mut obsolete = Vec::new();
        for item in &wake.items {
            let current = pending.as_ref().is_some_and(|pending| {
                pending.wake.items.iter().any(|current| {
                    current.subscription_id == item.subscription_id
                        && current.event == item.event
                        && current.heartbeat_due_at_ms == item.heartbeat_due_at_ms
                })
            });
            if !current {
                obsolete.push(item.clone());
                continue;
            }
            if let Some(event) = &item.event
                && event.source == "codex.project"
            {
                let project = match event.labels.get("project") {
                    Some(id) => self
                        .store
                        .project_status(id)
                        .await
                        .map_err(|error| error.to_string())?,
                    None => None,
                };
                let valid = project.is_some_and(|project| {
                    !project.paused
                        && !project.completed
                        && project.owner_thread_id == wake.thread_id
                        && if event.event_type == "performance_review_due" {
                            project.review.as_ref().is_some_and(|job| {
                                job.id.to_string() == event.id && job.decision_ref.is_none()
                            })
                        } else {
                            project.delivery.values().any(|target| {
                                !target.paused
                                    && target.job.as_ref().is_some_and(|job| {
                                        job.id.to_string() == event.id
                                            && !job.notification_delivered
                                    })
                            })
                        }
                });
                if !valid {
                    obsolete.push(item.clone());
                    continue;
                }
            }
            let manual = item.reasons.contains(&WakeReason::Manual)
                && self
                    .store
                    .manual_wake_pending(wake.thread_id, item.subscription_id)
                    .await
                    .map_err(|error| error.to_string())?;
            if running || manual || policy.allows_background(item) {
                eligible.push(item.clone());
            }
        }
        Ok((eligible, obsolete))
    }
}

impl WakeSink for AppServerSubscriptionWakeSink {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "alarm admission must remain serialized with Stop and permission changes"
    )]
    async fn wake(&self, wake: WakeBatch) -> Result<WakeDisposition, String> {
        let thread_manager = self
            .thread_manager
            .upgrade()
            .ok_or_else(|| "thread manager is no longer available".to_string())?;
        let thread = match thread_manager.get_thread(wake.thread_id).await {
            Ok(thread) => thread,
            Err(_) => {
                let (eligible, obsolete) = self.select_wakes(&wake, /*running*/ false).await?;
                if eligible.is_empty() {
                    return Ok(if obsolete.is_empty() {
                        WakeDisposition::DeferredUntilResume
                    } else {
                        WakeDisposition::Handled {
                            delivered: Vec::new(),
                            discarded: obsolete,
                        }
                    });
                }
                self.background_loader
                    .get()
                    .and_then(Weak::upgrade)
                    .ok_or_else(|| "background thread loader is not ready".to_string())?
                    .ensure_background_thread_loaded(wake.thread_id)
                    .await?
            }
        };
        let run = thread.subscription_run_state();
        let dispatch = run.dispatch.lock().await;
        if run.stop_pending.load(Ordering::Acquire) {
            return Ok(WakeDisposition::DeferredUntilResume);
        }
        let (mut eligible, mut discarded) = self
            .select_wakes(&wake, thread.has_running_user_work().await)
            .await?;
        let has_admitted_non_review_wake = eligible.iter().any(|item| {
            !item.event.as_ref().is_some_and(|event| {
                event.source == "codex.project" && event.event_type == "performance_review_due"
            })
        });
        if has_admitted_non_review_wake {
            let (follow_up_reviews, _) = self.select_wakes(&wake, /*running*/ true).await?;
            for review in follow_up_reviews.into_iter().filter(|item| {
                item.event.as_ref().is_some_and(|event| {
                    event.source == "codex.project" && event.event_type == "performance_review_due"
                })
            }) {
                if !eligible
                    .iter()
                    .any(|item| item.subscription_id == review.subscription_id)
                {
                    eligible.push(review);
                }
            }
        }
        let mut delivered = Vec::new();
        let mut queued = false;
        let mut retry_wakes = Vec::new();
        let mut normal = Vec::new();
        let mut reviews = Vec::new();
        for item in eligible {
            if self
                .store
                .is_capacity_retry_subscription(item.subscription_id)
                .await
                .map_err(|error| error.to_string())?
            {
                retry_wakes.push(item);
            } else if let Some(event) = &item.event
                && event.source == "codex.project"
                && event.event_type == "performance_review_due"
                && let Some(project) = event.labels.get("project")
            {
                reviews.push((project.clone(), item));
            } else {
                normal.push(item);
            }
        }
        let mut retry_deferred = false;
        let mut retry_started = false;
        let retry_wake_ids = retry_wakes
            .iter()
            .map(|item| item.subscription_id)
            .collect::<Vec<_>>();
        for item in retry_wakes {
            if retry_started {
                delivered.push(item);
                continue;
            }
            match thread
                .start_capacity_retry_if_idle()
                .await
                .map_err(|error| error.to_string())?
            {
                codex_core::StartIfIdleSubmission::Started { .. } => {
                    for subscription_id in &retry_wake_ids {
                        self.store
                            .cancel(*subscription_id)
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                    delivered.push(item);
                    queued = true;
                    retry_started = true;
                }
                codex_core::StartIfIdleSubmission::NotSubmitted {
                    reason:
                        NotSubmittedReason::NotIdle
                        | NotSubmittedReason::PendingTriggerTurn
                        | NotSubmittedReason::PlanMode,
                } => {
                    retry_deferred = true;
                }
                codex_core::StartIfIdleSubmission::NotSubmitted { reason } => {
                    return Err(format!(
                        "Core declined a permitted capacity retry: {reason:?}"
                    ));
                }
            }
        }
        // Keep every trusted project scope visible within the bounded model fragment.
        for items in normal.chunks(/*chunk_size*/ 4) {
            let batch = WakeBatch {
                thread_id: wake.thread_id,
                items: items.to_vec(),
            };
            if thread
                .inject_subscription_wake_if_running(batch.clone())
                .await
            {
                // Core acknowledges only after the queued input is recorded and flushed.
                queued = true;
            } else {
                let (background, obsolete) = self.select_wakes(&batch, /*running*/ false).await?;
                discarded.extend(obsolete);
                if !background.is_empty() {
                    match thread
                        .start_event_subscription_wake_if_idle(WakeBatch {
                            thread_id: wake.thread_id,
                            items: background.clone(),
                        })
                        .await
                        .map_err(|error| error.to_string())?
                    {
                        codex_core::StartIfIdleSubmission::Started { .. } => {
                            queued = true;
                        }
                        codex_core::StartIfIdleSubmission::NotSubmitted {
                            reason:
                                NotSubmittedReason::NotIdle
                                | NotSubmittedReason::PendingTriggerTurn
                                | NotSubmittedReason::PlanMode,
                        } => {}
                        codex_core::StartIfIdleSubmission::NotSubmitted { reason } => {
                            return Err(format!("Core declined a permitted alarm: {reason:?}"));
                        }
                    }
                }
            }
        }
        drop(dispatch);
        // Review admission is checked again under the same owner's dispatch lock.
        for (project, item) in reviews {
            let result = if has_admitted_non_review_wake {
                thread_manager
                    .run_project_review_worker_after_admitted_wake(wake.thread_id, &project)
                    .await
            } else {
                thread_manager
                    .run_project_review_worker(wake.thread_id, &project)
                    .await
            }
            .map_err(|error| error.to_string())?;
            match result {
                WakeDisposition::Started => delivered.push(item),
                WakeDisposition::Queued
                | WakeDisposition::DeferredUntilIdle
                | WakeDisposition::DeferredUntilResume
                | WakeDisposition::Handled { .. } => {}
            }
        }
        Ok(if !delivered.is_empty() || !discarded.is_empty() {
            WakeDisposition::Handled {
                delivered,
                discarded,
            }
        } else if queued {
            WakeDisposition::Queued
        } else if retry_deferred {
            WakeDisposition::DeferredUntilIdle
        } else {
            WakeDisposition::DeferredUntilResume
        })
    }
}

#[derive(Clone)]
pub(crate) struct EventSubscriptionLifecycle {
    service: EventSubscriptionService,
    store: codex_state::SqliteEventSubscriptionStore,
}

impl EventSubscriptionLifecycle {
    pub(crate) fn new(
        service: EventSubscriptionService,
        store: codex_state::SqliteEventSubscriptionStore,
    ) -> Self {
        Self { service, store }
    }

    async fn restore_stopped_scope(&self, thread_store: &codex_extension_api::ExtensionData) {
        let Ok(thread_id) = ThreadId::from_string(thread_store.level_id()) else {
            return;
        };
        let stopped = match self.store.read_wake_policy(thread_id).await {
            Ok(policy) => policy.stopped_revision > policy.resumed_revision,
            Err(_) => true,
        };
        if stopped {
            thread_store
                .get_or_init(codex_event_subscriptions::SubscriptionRunState::default)
                .user_work
                .store(/*val*/ false, Ordering::Release);
        }
    }

    fn notify(&self, level_id: &str) {
        match ThreadId::from_string(level_id) {
            Ok(thread_id) => self.service.notify_thread_ready(thread_id),
            Err(error) => {
                tracing::warn!(%error, level_id, "invalid thread id in subscription lifecycle")
            }
        }
    }
}

impl<C> ThreadLifecycleContributor<C> for EventSubscriptionLifecycle
where
    C: Send + Sync + 'static,
{
    fn on_thread_ready<'a>(&'a self, input: ThreadReadyInput<'a, C>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.restore_stopped_scope(input.thread_store).await;
            self.notify(input.thread_store.level_id());
        })
    }

    fn on_thread_resume<'a>(&'a self, input: ThreadResumeInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.restore_stopped_scope(input.thread_store).await;
            self.notify(input.thread_store.level_id());
        })
    }

    fn on_thread_idle<'a>(&'a self, input: ThreadIdleInput<'a>) -> ExtensionFuture<'a, ()> {
        self.notify(input.thread_store.level_id());
        Box::pin(async move {
            match codex_core::CodexThread::project_review_worker_idle(input.thread_store).await {
                Ok(Some(owner_thread_id)) => self.service.notify_thread_ready(owner_thread_id),
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "project review idle handling failed"),
            }
        })
    }
}

impl TurnLifecycleContributor for EventSubscriptionLifecycle {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id()) else {
                return;
            };
            if input
                .turn_store
                .get::<UserStartedSubscriptionWork>()
                .is_some()
            {
                if let Err(error) = self
                    .store
                    .cancel_capacity_retry_subscriptions(thread_id)
                    .await
                {
                    tracing::warn!(%error, "capacity retry alarm cleanup failed at user turn start");
                }
                if let Err(error) = self.service.notify_user_started(thread_id).await {
                    tracing::warn!(%error, "user-start alarm dispatch failed");
                }
            } else {
                self.service.notify_thread_ready(thread_id);
            }
        })
    }

    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if !matches!(input.error, CodexErrorInfo::ServerOverloaded)
                || !input.retryable_before_response
                || is_guardian_review_source(input.session_source)
            {
                return;
            }
            let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id()) else {
                return;
            };
            if let Err(error) = self
                .store
                .cancel_capacity_retry_subscriptions(thread_id)
                .await
            {
                tracing::warn!(%error, "previous capacity retry alarm cleanup failed");
            }
            let now_ms = codex_core::project_automation_now_ms();
            let delay_seconds = capacity_retry_delay_seconds();
            let first_deadline_at_ms = now_ms.saturating_add(delay_seconds * 1_000);
            let filter = EventFilter {
                source: CAPACITY_RETRY_SOURCE.to_string(),
                event_types: BTreeSet::from([CAPACITY_RETRY_EVENT_TYPE.to_string()]),
                labels: BTreeMap::new(),
            };
            if let Err(error) = self
                .service
                .create(NewSubscription {
                    thread_id,
                    filter: Some(filter),
                    source_cursor: None,
                    heartbeat: Some(HeartbeatSpec {
                        interval_ms: 365 * 86_400_000,
                        first_deadline_at_ms: Some(first_deadline_at_ms),
                    }),
                })
                .await
            {
                tracing::warn!(
                    %error,
                    %thread_id,
                    delay_seconds,
                    "failed to schedule capacity retry alarm"
                );
            }
        })
    }
}

#[cfg(test)]
#[path = "event_subscriptions_tests.rs"]
mod tests;
