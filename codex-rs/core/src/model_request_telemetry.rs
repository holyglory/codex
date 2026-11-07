//! Forward transport observations without replacing the provider's existing telemetry.
use codex_api::RequestTelemetry;
use codex_api::TransportError;
use codex_client::RetryWaitOutcome;
use http::StatusCode;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct RequestTelemetrySet(pub(crate) Vec<Arc<dyn RequestTelemetry>>);

impl RequestTelemetry for RequestTelemetrySet {
    fn on_request_start(&self, attempt: u64) {
        for observer in &self.0 {
            observer.on_request_start(attempt);
        }
    }

    fn on_request(
        &self,
        attempt: u64,
        status: Option<StatusCode>,
        error: Option<&TransportError>,
        duration: Duration,
    ) {
        for observer in &self.0 {
            observer.on_request(attempt, status, error, duration);
        }
    }

    fn on_request_cancelled(&self, attempt: u64, duration: Duration) {
        for observer in &self.0 {
            observer.on_request_cancelled(attempt, duration);
        }
    }

    fn on_retry_wait_started(&self, attempt: u64) {
        for observer in &self.0 {
            observer.on_retry_wait_started(attempt);
        }
    }

    fn on_retry_wait_finished(&self, attempt: u64, duration: Duration, outcome: RetryWaitOutcome) {
        for observer in &self.0 {
            observer.on_retry_wait_finished(attempt, duration, outcome);
        }
    }
}
