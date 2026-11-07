use crate::error::ApiError;
use codex_client::Request;
use codex_client::RequestTelemetry;
use codex_client::Response;
use codex_client::RetryPolicy;
use codex_client::StreamResponse;
use codex_client::TransportError;
use codex_client::run_with_retry_and_telemetry;
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
    F: Clone + Fn(Request, u64) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    // Wraps `run_with_retry` to attach per-attempt request telemetry for both
    // unary and streaming HTTP calls.
    let retry_telemetry = telemetry.clone();
    run_with_retry_and_telemetry(
        policy,
        make_request,
        move |req, attempt| {
            let telemetry = telemetry.clone();
            let send = send.clone();
            async move {
                let start = Instant::now();
                let result = send(req, attempt).await;
                if let Some(t) = &telemetry {
                    let (status, err) = match &result {
                        Ok(resp) => (Some(resp.status()), None),
                        Err(err) => (http_status(err), Some(err)),
                    };
                    t.on_request(attempt, status, err, start.elapsed());
                }
                result
            }
        },
        retry_telemetry,
    )
    .await
}

/// Cancels only a transport that has actually started; authentication preflight stays separate.
pub(crate) struct RequestAttemptGuard {
    telemetry: Option<Arc<dyn RequestTelemetry>>,
    attempt: u64,
    start: Instant,
}

impl RequestAttemptGuard {
    pub(crate) fn start(telemetry: Option<Arc<dyn RequestTelemetry>>, attempt: u64) -> Self {
        let start = Instant::now();
        if let Some(telemetry) = &telemetry {
            telemetry.on_request_start(attempt);
        }
        Self {
            telemetry,
            attempt,
            start,
        }
    }

    pub(crate) fn finish(mut self) {
        self.telemetry = None;
    }
}

impl Drop for RequestAttemptGuard {
    fn drop(&mut self) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.on_request_cancelled(self.attempt, self.start.elapsed());
        }
    }
}
