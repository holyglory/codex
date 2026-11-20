use codex_http_client::TransportError;
use http::StatusCode;
use std::time::Duration;

/// API specific telemetry.
pub trait RequestTelemetry: Send + Sync {
    /// Called immediately before each transport attempt, including internal retries.
    fn on_request_start(&self, _attempt: u64) {}

    fn on_request(
        &self,
        attempt: u64,
        status: Option<StatusCode>,
        error: Option<&TransportError>,
        duration: Duration,
    );

    /// Pairs an attempt start when its sending future is dropped before returning headers.
    fn on_request_cancelled(&self, _attempt: u64, _duration: Duration) {}

    /// Called immediately before an actual retry timer is awaited.
    fn on_retry_wait_started(&self, _attempt: u64) {}

    /// Pairs a retry wait start with measured elapsed time, including partial cancellation.
    fn on_retry_wait_finished(
        &self,
        _attempt: u64,
        _duration: Duration,
        _outcome: RetryWaitOutcome,
    ) {
    }
}

/// Whether an observed retry timer elapsed or its owning future was dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryWaitOutcome {
    /// The selected timer finished.
    Completed,
    /// Cancellation interrupted the timer.
    Cancelled,
}
