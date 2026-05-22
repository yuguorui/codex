use codex_http_client::TransportError;
use http::StatusCode;
use std::time::Duration;

/// API specific telemetry.
pub trait RequestTelemetry: Send + Sync {
    fn on_request(
        &self,
        attempt: u64,
        status: Option<StatusCode>,
        error: Option<&TransportError>,
        duration: Duration,
    );

    fn on_retry(
        &self,
        next_attempt: u64,
        status: Option<StatusCode>,
        error: &TransportError,
        delay: Duration,
    ) {
        let _ = (next_attempt, status, error, delay);
    }
}
