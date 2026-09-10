use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;

use codex_core::NotSubmittedReason;
use codex_core::ThreadManager;
use codex_event_subscriptions::EventSubscriptionService;
use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeDisposition;
use codex_event_subscriptions::WakeSink;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ThreadIdleInput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadReadyInput;
use codex_extension_api::ThreadResumeInput;
use codex_protocol::ThreadId;
use futures::future::join_all;

use crate::request_processors::ThreadRequestProcessor;

#[derive(Clone)]
pub(crate) struct AppServerSubscriptionWakeSink {
    thread_manager: Weak<ThreadManager>,
    background_loader: Arc<OnceLock<Weak<ThreadRequestProcessor>>>,
}

impl AppServerSubscriptionWakeSink {
    pub(crate) fn new(
        thread_manager: Weak<ThreadManager>,
        background_loader: Arc<OnceLock<Weak<ThreadRequestProcessor>>>,
    ) -> Self {
        Self {
            thread_manager,
            background_loader,
        }
    }
}

impl WakeSink for AppServerSubscriptionWakeSink {
    async fn wake(&self, wake: WakeBatch) -> Result<WakeDisposition, String> {
        let thread_manager = self
            .thread_manager
            .upgrade()
            .ok_or_else(|| "thread manager is no longer available".to_string())?;
        let thread = match thread_manager.get_thread(wake.thread_id).await {
            Ok(thread) => thread,
            Err(_) => {
                let loader = self
                    .background_loader
                    .get()
                    .and_then(Weak::upgrade)
                    .ok_or_else(|| "background thread loader is not ready".to_string())?;
                loader
                    .ensure_background_thread_loaded(wake.thread_id)
                    .await?
            }
        };
        let projects = wake
            .items
            .iter()
            .filter_map(|item| {
                let event = item.event.as_ref()?;
                (event.source == "codex.project")
                    .then(|| event.labels.get("project").cloned())
                    .flatten()
            })
            .collect::<BTreeSet<_>>();
        if !projects.is_empty() {
            let mut normal_wake = wake.clone();
            normal_wake.items.retain(|item| {
                !item.event.as_ref().is_some_and(|event| {
                    event.source == "codex.project" && event.event_type == "performance_review_due"
                })
            });
            let normal = if normal_wake.items.is_empty() {
                WakeDisposition::Started
            } else {
                thread
                    .start_project_subscription_wake_once(normal_wake)
                    .await
                    .map_err(|error| error.to_string())?
            };
            let reviews = join_all(projects.iter().map(|project_id| {
                thread_manager.run_project_review_worker(wake.thread_id, project_id)
            }))
            .await;
            let mut disposition = normal;
            for review in reviews {
                if review.map_err(|error| error.to_string())? == WakeDisposition::DeferredUntilIdle
                {
                    disposition = WakeDisposition::DeferredUntilIdle;
                }
            }
            return Ok(disposition);
        }
        match thread
            .start_event_subscription_wake_if_idle(wake)
            .await
            .map_err(|error| error.to_string())?
        {
            codex_core::StartIfIdleSubmission::Started { .. } => Ok(WakeDisposition::Started),
            codex_core::StartIfIdleSubmission::NotSubmitted {
                reason: NotSubmittedReason::NotIdle | NotSubmittedReason::PendingTriggerTurn,
            } => Ok(WakeDisposition::DeferredUntilIdle),
            codex_core::StartIfIdleSubmission::NotSubmitted { reason } => {
                Err(format!("Core declined a subscription wake: {reason:?}"))
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct EventSubscriptionLifecycle {
    service: EventSubscriptionService,
}

impl EventSubscriptionLifecycle {
    pub(crate) fn new(service: EventSubscriptionService) -> Self {
        Self { service }
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
        self.notify(input.thread_store.level_id());
        Box::pin(async {})
    }

    fn on_thread_resume<'a>(&'a self, input: ThreadResumeInput<'a>) -> ExtensionFuture<'a, ()> {
        self.notify(input.thread_store.level_id());
        Box::pin(async {})
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
