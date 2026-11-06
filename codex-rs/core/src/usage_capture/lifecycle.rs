use super::CaptureContext;
use super::Operation;
use super::ThreadCapture;
use super::ThreadLineage;
use super::TurnCapture;
use super::sink::CaptureSink;
use crate::config::Config;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ThreadStopInput;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_usage::UsageOutcome;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

pub(super) struct CaptureLifecycle(pub(super) Arc<CaptureSink>);

impl ThreadLifecycleContributor<Config> for CaptureLifecycle {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(lineage) = input.thread_store.get::<ThreadLineage>() {
                let context = Arc::new(CaptureContext {
                    sink: Arc::clone(&self.0),
                    thread_id: lineage.thread_id.clone(),
                    parent_thread_id: lineage.parent_thread_id.clone(),
                    turn_id: None,
                    account_id: None,
                    repository_id: None,
                    activity: Arc::default(),
                });
                let operation =
                    Operation::start(Arc::clone(&context), "thread_started", "thread_stopped");
                operation.publish_started();
                input.thread_store.insert(ThreadCapture {
                    context,
                    operation: Mutex::new(Some(operation)),
                });
            }
        })
    }

    fn on_thread_stop<'a>(&'a self, input: ThreadStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(capture) = input.thread_store.get::<ThreadCapture>() {
                capture.context.activity.stop(&capture.context);
                if let Some(operation) = capture
                    .operation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    operation.finish(UsageOutcome::Completed);
                }
                capture.context.sink.flush().await;
            }
        })
    }
}

impl TurnLifecycleContributor for CaptureLifecycle {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(capture) = input.turn_store.get::<TurnCapture>() {
                let operation = Operation::start(
                    Arc::clone(&capture.context),
                    "turn_started",
                    "turn_finished",
                );
                operation.publish_started();
                *capture
                    .operation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(operation);
            }
        })
    }

    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(capture) = input.turn_store.get::<TurnCapture>() {
                capture.failed.store(true, Ordering::Release);
            }
        })
    }

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(capture) = input.turn_store.get::<TurnCapture>() {
                let outcome = if capture.failed.load(Ordering::Acquire) {
                    UsageOutcome::Failed
                } else {
                    UsageOutcome::Completed
                };
                if let Some(operation) = capture
                    .operation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    operation.finish(outcome);
                }
                capture.context.sink.flush().await;
            }
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(capture) = input.turn_store.get::<TurnCapture>() {
                if let Some(operation) = capture
                    .operation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    operation.finish(UsageOutcome::Cancelled);
                }
                capture.context.sink.flush().await;
            }
        })
    }
}
