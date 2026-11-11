use super::CaptureContext;
use super::bounded_label;
use super::model_transport::CaptureTransport;
use codex_extension_api::ModelRequestContributor;
use codex_extension_api::ModelRequestInput;
use codex_extension_api::ModelResponseError;
use codex_extension_api::ModelResponseInterceptor;
use codex_extension_api::ModelResponseStream;
use codex_extension_api::ResponseEvent;
use codex_usage::UsageOutcome;
use codex_usage::UsageRecord;
use futures::StreamExt;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;

const MAX_RESPONSE_OPERATIONS: usize = 256;

#[derive(Eq, PartialEq)]
struct ResponseOperation {
    operation_id: String,
    account_id: Option<String>,
}

/// Turn-local observed links, shared across account retargets. Conflicts remain
/// unknown; a full map never evicts an in-flight response or guesses a new link.
#[derive(Default)]
pub(super) struct ResponseOperations(Mutex<HashMap<String, Option<ResponseOperation>>>);

impl ResponseOperations {
    fn observe(&self, response_id: &str, operation: &super::Operation) {
        if operation.record.event.as_deref() != Some("model_request_started")
            || response_id.is_empty()
            || response_id.len() > 160
            || response_id.chars().any(char::is_control)
        {
            return;
        }
        let Some(operation_id) = &operation.record.operation_id else {
            return;
        };
        let observed = Some(ResponseOperation {
            operation_id: operation_id.clone(),
            account_id: operation.context.account_id.clone(),
        });
        let mut links = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let at_capacity = links.len() >= MAX_RESPONSE_OPERATIONS;
        match links.entry(response_id.to_string()) {
            Entry::Occupied(mut entry) => {
                if entry.get() != &observed {
                    entry.insert(/*value*/ None);
                }
            }
            Entry::Vacant(entry) => {
                if !at_capacity {
                    entry.insert(observed);
                }
            }
        }
    }

    fn discard_without_usage(&self, operation: &super::Operation) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, link| {
                link.as_ref().is_none_or(|link| {
                    Some(&link.operation_id) != operation.record.operation_id.as_ref()
                })
            });
    }

    pub(super) fn annotate_token_record(&self, record: &mut UsageRecord) {
        // Session supplies the provider response ID as a temporary operation key.
        // Resolve only a unique observed request; never retain a guessed operation/account.
        let response_id = record.operation_id.take();
        record.account_id = None;
        let mut links = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(response_id) = response_id
            && links.get(&response_id).is_some_and(Option::is_some)
            && let Some(Some(link)) = links.remove(&response_id)
        {
            record.operation_id = Some(link.operation_id);
            record.account_id = link.account_id;
        }
    }
}

pub(super) struct CaptureRequests(
    pub(super) Arc<CaptureContext>,
    pub(super) Arc<ResponseOperations>,
);

impl std::fmt::Debug for CaptureRequests {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContentFreeUsageCapture")
    }
}

impl ModelRequestContributor for CaptureRequests {
    fn request(&self, input: ModelRequestInput<'_>) -> Option<Box<dyn ModelResponseInterceptor>> {
        Some(Box::new(CaptureResponse(
            Arc::new(CaptureTransport::new(
                Arc::clone(&self.0),
                input.request_store,
                input.model,
                input.kind,
            )),
            Arc::clone(&self.1),
        )))
    }
}

struct CaptureResponse(Arc<CaptureTransport>, Arc<ResponseOperations>);

impl ModelResponseInterceptor for CaptureResponse {
    fn request_telemetry(&self) -> Option<Arc<dyn codex_api::RequestTelemetry>> {
        Some(self.0.clone())
    }

    fn intercept(self: Box<Self>, stream: ModelResponseStream) -> ModelResponseStream {
        let CaptureResponse(capture, responses) = *self;
        {
            let mut state = capture
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.stream_owned = true;
            if let Some(operation) = &mut state.operation {
                operation.drop_outcome = UsageOutcome::Cancelled;
            }
        }
        Box::pin(futures::stream::unfold(
            (stream, capture, responses),
            |(mut stream, capture, responses)| async move {
                let Some(event) = stream.next().await else {
                    {
                        let state = capture
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(operation) = &state.operation
                            && !operation
                                .finished
                                .load(std::sync::atomic::Ordering::Acquire)
                        {
                            responses.discard_without_usage(operation);
                        }
                    }
                    capture.finish(UsageOutcome::Incomplete);
                    return None;
                };
                let mut state = capture
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(operation) = &mut state.operation {
                    match &event {
                        Ok(ResponseEvent::Created { response_id }) => {
                            if let Some(response_id) = response_id {
                                responses.observe(response_id, operation);
                            }
                            *operation
                                .response_id
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                response_id.as_deref().map(bounded_label);
                        }
                        Ok(ResponseEvent::Completed {
                            response_id,
                            token_usage,
                            ..
                        }) => {
                            responses.observe(response_id, operation);
                            if token_usage.is_none() {
                                responses.discard_without_usage(operation);
                            }
                            *operation
                                .response_id
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(bounded_label(response_id));
                            operation.finish(UsageOutcome::Completed);
                        }
                        Err(error) => {
                            if let ModelResponseError::ResponseIncomplete(incomplete) = error {
                                if let Some(response_id) = &incomplete.response_id {
                                    responses.observe(response_id, operation);
                                }
                                if incomplete.token_usage.is_none() {
                                    responses.discard_without_usage(operation);
                                }
                                *operation
                                    .response_id
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    incomplete.response_id.as_deref().map(bounded_label);
                                operation.finish(UsageOutcome::Incomplete);
                            } else {
                                responses.discard_without_usage(operation);
                                operation.finish(UsageOutcome::Failed);
                            }
                        }
                        Ok(_) => {}
                    }
                }
                drop(state);
                Some((event, (stream, capture, responses)))
            },
        ))
    }
}
