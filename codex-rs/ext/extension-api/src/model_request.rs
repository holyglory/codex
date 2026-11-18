//! Request-scoped interception of model response streams.
use crate::ExtensionFuture;
pub use codex_api::ApiError as ModelResponseError;
pub use codex_api::GuardianAssessment;
pub use codex_api::ResponseEvent;
use futures::stream::BoxStream;
use std::collections::HashMap;
use std::sync::Arc;

/// Model response events owned by an interceptor or its downstream consumer.
pub type ModelResponseStream = BoxStream<'static, Result<ResponseEvent, ModelResponseError>>;

/// Whether a request generates output or only prepares a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelRequestKind {
    /// Generate model output.
    Generation,
    /// Prepare the session without generating output.
    Warmup,
}

/// Request identity and additional metadata supplied before sending to the provider.
pub struct ModelRequestInput<'a> {
    /// Whether this request generates output or warms up the session.
    pub kind: ModelRequestKind,
    /// Thread making the request.
    pub thread_id: &'a str,
    /// Additional provider metadata for request tracking; existing and reserved keys cannot be replaced.
    pub client_metadata: &'a mut Option<HashMap<String, String>>,
    /// Requested model identifier.
    pub model: &'a str,
    /// Host-local state shared only by attempts of this logical request; never provider metadata.
    pub request_store: &'a crate::ExtensionData,
}

/// Checks model requests and creates request-scoped response interceptors.
pub trait ModelRequestContributor: Send + Sync + std::fmt::Debug {
    /// Core awaits this check before submitting generation to the transport.
    /// Return true to fail with `SessionBudgetExceeded`. Requests already handed to the
    /// transport can still complete or retry. WebSocket warmup skips this check.
    fn budget_exhausted<'a>(&'a self, _thread_id: &'a str) -> ExtensionFuture<'a, bool> {
        Box::pin(async { false })
    }

    /// Return None for requests this contributor does not intercept.
    fn request(&self, input: ModelRequestInput<'_>) -> Option<Box<dyn ModelResponseInterceptor>>;
}

/// Owns request state until consumed by the response stream or dropped on failure.
/// Interceptors compose in registration order; dropping their stream must release unfinished work.
/// Trusted host implementations must bound any model-visible content they synthesize.
pub trait ModelResponseInterceptor: Send + Sync {
    /// Optional content-free transport observation for this request. Existing interceptors need
    /// not implement it; successful HTTP headers do not complete the response stream.
    fn request_telemetry(&self) -> Option<Arc<dyn codex_api::RequestTelemetry>> {
        None
    }

    /// Wrap the response stream, optionally buffering, inspecting, or transforming events.
    /// The returned stream must own request state and propagate cancellation by dropping upstream.
    fn intercept(self: Box<Self>, stream: ModelResponseStream) -> ModelResponseStream;
}
