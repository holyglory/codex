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
use futures::StreamExt;
use std::sync::Arc;

pub(super) struct CaptureRequests(pub(super) Arc<CaptureContext>);

impl std::fmt::Debug for CaptureRequests {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContentFreeUsageCapture")
    }
}

impl ModelRequestContributor for CaptureRequests {
    fn request(&self, input: ModelRequestInput<'_>) -> Option<Box<dyn ModelResponseInterceptor>> {
        Some(Box::new(CaptureResponse(Arc::new(CaptureTransport::new(
            Arc::clone(&self.0),
            input.request_store,
            input.model,
            input.kind,
        )))))
    }
}

struct CaptureResponse(Arc<CaptureTransport>);

impl ModelResponseInterceptor for CaptureResponse {
    fn request_telemetry(&self) -> Option<Arc<dyn codex_api::RequestTelemetry>> {
        Some(self.0.clone())
    }

    fn intercept(self: Box<Self>, stream: ModelResponseStream) -> ModelResponseStream {
        let CaptureResponse(capture) = *self;
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
            (stream, capture),
            |(mut stream, capture)| async move {
                let Some(event) = stream.next().await else {
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
                            *operation
                                .response_id
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                response_id.as_deref().map(bounded_label);
                        }
                        Ok(ResponseEvent::Completed { response_id, .. }) => {
                            *operation
                                .response_id
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(bounded_label(response_id));
                            operation.finish(UsageOutcome::Completed);
                        }
                        Err(error) => {
                            if let ModelResponseError::ResponseIncomplete(incomplete) = error {
                                *operation
                                    .response_id
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    incomplete.response_id.as_deref().map(bounded_label);
                                operation.finish(UsageOutcome::Incomplete);
                            } else {
                                operation.finish(UsageOutcome::Failed);
                            }
                        }
                        Ok(_) => {}
                    }
                }
                drop(state);
                Some((event, (stream, capture)))
            },
        ))
    }
}
