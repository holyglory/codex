use super::CaptureContext;
use super::Operation;
use super::bounded_label;
use codex_extension_api::ModelRequestContributor;
use codex_extension_api::ModelRequestInput;
use codex_extension_api::ModelRequestKind;
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
        let (started, finished) = match input.kind {
            ModelRequestKind::Generation => ("model_request_started", "model_request_finished"),
            ModelRequestKind::Warmup => ("model_warmup_started", "model_warmup_finished"),
        };
        let mut operation = Operation::start(Arc::clone(&self.0), started, finished);
        operation.record.model = Some(bounded_label(input.model));
        match input.kind {
            ModelRequestKind::Generation => self
                .0
                .activity
                .model_started(&self.0, &mut operation.record),
            ModelRequestKind::Warmup => self.0.activity.annotate(&mut operation.record),
        }
        operation.drop_outcome = UsageOutcome::Failed;
        operation.publish_started();
        Some(Box::new(CaptureResponse(operation)))
    }
}

struct CaptureResponse(Operation);

impl ModelResponseInterceptor for CaptureResponse {
    fn intercept(self: Box<Self>, stream: ModelResponseStream) -> ModelResponseStream {
        let CaptureResponse(mut operation) = *self;
        operation.drop_outcome = UsageOutcome::Cancelled;
        Box::pin(futures::stream::unfold(
            (stream, operation),
            |(mut stream, operation)| async move {
                let Some(event) = stream.next().await else {
                    operation.finish(UsageOutcome::Incomplete);
                    return None;
                };
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
                Some((event, (stream, operation)))
            },
        ))
    }
}
