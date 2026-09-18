use crate::error::ApiError;
use codex_client::Request;
use codex_client::RequestTelemetry;
use codex_client::Response;
use codex_client::RetryPolicy;
use codex_client::StreamResponse;
use codex_client::TransportError;
use codex_client::run_with_retry;
use http::StatusCode;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Error;
use tokio_tungstenite::tungstenite::Message;

/// Generic telemetry.
pub trait SseTelemetry: Send + Sync {
    fn on_sse_poll(
        &self,
        result: &Result<
            Option<
                Result<
                    eventsource_stream::Event,
                    eventsource_stream::EventStreamError<TransportError>,
                >,
            >,
            tokio::time::error::Elapsed,
        >,
        duration: Duration,
    );
}

/// Telemetry for Responses WebSocket transport.
pub trait WebsocketTelemetry: Send + Sync {
    fn on_ws_request(&self, duration: Duration, error: Option<&ApiError>, connection_reused: bool);

    fn on_ws_event(
        &self,
        result: &Result<Option<Result<Message, Error>>, ApiError>,
        duration: Duration,
    );
}

pub(crate) trait WithStatus {
    fn status(&self) -> StatusCode;
}

fn http_status(err: &TransportError) -> Option<StatusCode> {
    match err {
        TransportError::Http { status, .. } => Some(*status),
        _ => None,
    }
}

impl WithStatus for Response {
    fn status(&self) -> StatusCode {
        self.status
    }
}

impl WithStatus for StreamResponse {
    fn status(&self) -> StatusCode {
        self.status
    }
}

pub(crate) async fn run_with_request_telemetry<T, F, Fut>(
    policy: RetryPolicy,
    telemetry: Option<Arc<dyn RequestTelemetry>>,
    make_request: impl FnMut() -> Request,
    send: F,
) -> Result<T, TransportError>
where
    T: WithStatus,
    F: Clone + Fn(Request) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    // Wraps `run_with_retry` to attach per-attempt request telemetry for both
    // unary and streaming HTTP calls.
    run_with_retry(policy, make_request, move |req, attempt| {
        let telemetry = telemetry.clone();
        let send = send.clone();
        async move {
            let start = Instant::now();
            let origin = url::Url::parse(&req.url).ok().map(|url| url.origin().ascii_serialization());
            let result = send(req).await;
            if let Err(TransportError::Http { status, headers, body, .. }) = &result {
                let parsed = body.as_ref().filter(|body| body.len() <= 1024 * 1024)
                    .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok());
                let code = parsed.as_ref().and_then(|body| body.pointer("/error/code")).and_then(serde_json::Value::as_str)
                    .and_then(crate::diagnostics::error_code);
                tracing::warn!(target: "codex.network_diagnostics", event = "http_failure",
                    transport = "http", attempt, http_status = status.as_u16(), error_code = code,
                    origin = origin.as_deref(),
                    request_id = headers.as_ref().and_then(|headers| headers.get("x-request-id").or_else(|| headers.get("x-oai-request-id"))).and_then(|value| value.to_str().ok()),
                    elapsed_ms = start.elapsed().as_millis() as u64,
                );
            } else if let Err(error) = &result {
                let kind = match error {
                    TransportError::Connection(_) => "connection",
                    TransportError::Network(_) => "network",
                    TransportError::Timeout => "timeout",
                    TransportError::Build(_) => "request_build",
                    TransportError::RetryLimit => "retry_exhausted",
                    TransportError::Http { .. } => "http_status",
                };
                tracing::warn!(target: "codex.network_diagnostics", event = "http_transport_failure",
                    transport = "http", attempt, kind, origin = origin.as_deref(),
                    elapsed_ms = start.elapsed().as_millis() as u64,
                );
            }
            if let Some(t) = telemetry.as_ref() {
                let (status, err) = match &result {
                    Ok(resp) => (Some(resp.status()), None),
                    Err(err) => (http_status(err), Some(err)),
                };
                t.on_request(attempt, status, err, start.elapsed());
            }
            result
        }
    })
    .await
}
