//! Request-local attempt chains and actual retry timer lifetimes.
use super::CaptureContext;
use super::Operation;
use super::bounded_label;
use codex_api::RequestTelemetry;
use codex_api::TransportError;
use codex_client::RetryWaitOutcome;
use codex_extension_api::ExtensionData;
use codex_extension_api::ModelRequestKind;
use codex_usage::UsageOutcome;
use http::StatusCode;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

struct RequestChain {
    context: Arc<CaptureContext>,
    state: Mutex<ChainState>,
}

struct GenerationChain(Arc<RequestChain>);
struct WarmupChain(Arc<RequestChain>);

#[derive(Default)]
struct ChainState {
    attempt: u64,
    previous_operation_id: Option<String>,
    model: Option<String>,
}

pub(super) struct CaptureTransport {
    context: Arc<CaptureContext>,
    chain: Arc<RequestChain>,
    model: String,
    kind: ModelRequestKind,
    pub(super) state: Mutex<ResponseState>,
    wait: Mutex<Option<Operation>>,
}

#[derive(Default)]
pub(super) struct ResponseState {
    pub(super) operation: Option<Operation>,
    pub(super) stream_owned: bool,
}

impl CaptureTransport {
    pub(super) fn new(
        context: Arc<CaptureContext>,
        store: &ExtensionData,
        model: &str,
        kind: ModelRequestKind,
    ) -> Self {
        let chain = match kind {
            ModelRequestKind::Generation => store
                .get_or_init(|| GenerationChain(Arc::new(RequestChain::new(Arc::clone(&context)))))
                .0
                .clone(),
            ModelRequestKind::Warmup => store
                .get_or_init(|| WarmupChain(Arc::new(RequestChain::new(Arc::clone(&context)))))
                .0
                .clone(),
        };
        Self {
            context,
            chain,
            model: bounded_label(model),
            kind,
            state: Mutex::new(ResponseState::default()),
            wait: Mutex::new(/*t*/ None),
        }
    }

    pub(super) fn finish(&self, outcome: UsageOutcome) {
        if let Some(operation) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .operation
            .take()
        {
            operation.finish(outcome);
        }
    }
}

impl RequestTelemetry for CaptureTransport {
    fn on_request_start(&self, _attempt: u64) {
        let (started, finished) = match self.kind {
            ModelRequestKind::Generation => ("model_request_started", "model_request_finished"),
            ModelRequestKind::Warmup => ("model_warmup_started", "model_warmup_finished"),
        };
        let mut operation = Operation::start(Arc::clone(&self.context), started, finished);
        operation.record.model = Some(self.model.clone());
        let mut state = self
            .chain
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.attempt = state.attempt.saturating_add(1);
        operation.record.attempt_number = Some(state.attempt);
        operation.record.retry_of_operation_id = state.previous_operation_id.clone();
        state.previous_operation_id = operation.record.operation_id.clone();
        state.model = operation.record.model.clone();
        drop(state);
        match self.kind {
            ModelRequestKind::Generation => self
                .context
                .activity
                .model_started(&self.context, &mut operation.record),
            ModelRequestKind::Warmup => self.context.activity.annotate(&mut operation.record),
        }
        operation.drop_outcome = UsageOutcome::Failed;
        operation.publish_started();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stream_owned {
            operation.drop_outcome = UsageOutcome::Cancelled;
        }
        state.operation = Some(operation);
    }

    fn on_request(
        &self,
        _attempt: u64,
        _status: Option<StatusCode>,
        error: Option<&TransportError>,
        _duration: Duration,
    ) {
        // Successful headers transfer lifetime ownership to CaptureResponse's stream.
        if error.is_some() {
            self.finish(UsageOutcome::Failed);
        }
    }

    fn on_request_cancelled(&self, _attempt: u64, _duration: Duration) {
        self.finish(UsageOutcome::Cancelled);
    }

    fn on_retry_wait_started(&self, _attempt: u64) {
        *self
            .wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(self.chain.retry_wait("http_retry"));
    }

    fn on_retry_wait_finished(
        &self,
        _attempt: u64,
        _duration: Duration,
        outcome: RetryWaitOutcome,
    ) {
        if let Some(wait) = self
            .wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            wait.finish(match outcome {
                RetryWaitOutcome::Completed => UsageOutcome::Completed,
                RetryWaitOutcome::Cancelled => UsageOutcome::Cancelled,
            });
        }
    }
}

impl RequestChain {
    fn new(context: Arc<CaptureContext>) -> Self {
        Self {
            context,
            state: Mutex::new(ChainState::default()),
        }
    }

    fn retry_wait(&self, source: &str) -> Operation {
        let mut operation = Operation::start(
            Arc::clone(&self.context),
            "retry_wait_started",
            "retry_wait_finished",
        );
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        operation.record.retry_of_operation_id = state.previous_operation_id.clone();
        operation.record.model = state.model.clone();
        operation.record.tool = Some(source.to_owned());
        self.context.activity.annotate(&mut operation.record);
        operation.publish_started();
        operation
    }
}

pub(crate) fn retry_wait(store: &ExtensionData, source: &str) -> Option<Operation> {
    Some(store.get::<GenerationChain>()?.0.retry_wait(source))
}

#[cfg(test)]
#[path = "model_transport_tests.rs"]
mod tests;
