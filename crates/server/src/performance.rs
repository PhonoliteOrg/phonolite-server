//! Opt-in, monotonic performance measurements. Never records request bodies or tokens.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use tower_http::trace::{
    DefaultMakeSpan, DefaultOnEos, DefaultOnRequest, DefaultOnResponse, MakeSpan, OnEos, OnRequest,
    OnResponse,
};
use tower_http::LatencyUnit;
use tracing::{Level, Span};

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("PHONOLITE_PROFILE")
            .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    })
}

pub fn next_id() -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub async fn http_request(request: Request, next: Next) -> Response {
    let polling = is_console_poll(request.method(), request.uri().path());
    // MatchedPath is the route template, never the URL's IDs, search terms or query string.
    let measurement = enabled().then(|| {
        let route = request
            .extensions()
            .get::<MatchedPath>()
            .map(|path| path.as_str().to_owned())
            .unwrap_or_else(|| "unmatched".to_owned());
        (route, request.method().to_string(), Instant::now())
    });
    let mut response = next.run(request).await;
    if polling {
        response.extensions_mut().insert(ConsolePollResponse);
    }
    if let Some((route, method, started)) = measurement {
        log_http_measurement(&response, &method, &route, started);
    }
    response
}

#[derive(Clone)]
struct ConsolePollResponse;

fn is_console_poll(method: &axum::http::Method, path: &str) -> bool {
    matches!(*method, axum::http::Method::GET | axum::http::Method::HEAD)
        && matches!(path, "/logs/tail" | "/logs/issues" | "/status/library")
}

fn quiet_poll_response<B>(response: &axum::http::Response<B>) -> bool {
    response.status().is_success() && response.extensions().get::<ConsolePollResponse>().is_some()
}

// Successful observation must not generate new records in the log it is
// observing. Keep failures and ordinary requests visible in HTTP diagnostics.
pub fn trace_span<B>(request: &axum::http::Request<B>) -> Span {
    if is_console_poll(request.method(), request.uri().path()) {
        Span::none()
    } else {
        DefaultMakeSpan::new().make_span(request)
    }
}

pub fn trace_request<B>(request: &axum::http::Request<B>, span: &Span) {
    if !is_console_poll(request.method(), request.uri().path()) {
        DefaultOnRequest::new().on_request(request, span);
    }
}

pub fn trace_end(trailers: Option<&axum::http::HeaderMap>, duration: Duration, span: &Span) {
    // Poll spans are deliberately absent. Without this check the default DEBUG
    // end-of-stream event would restart the feedback loop when debug is enabled.
    if span.id().is_some() {
        DefaultOnEos::new().on_eos(trailers, duration, span);
    }
}

pub fn trace_response<B>(response: &axum::http::Response<B>, latency: Duration, span: &Span) {
    if !quiet_poll_response(response) {
        DefaultOnResponse::new()
            .level(Level::INFO)
            .latency_unit(LatencyUnit::Millis)
            .on_response(response, latency, span);
    }
}

fn log_http_measurement(response: &Response, method: &str, route: &str, started: Instant) {
    if quiet_poll_response(response) {
        return;
    }
    let declared_response_bytes = response
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    tracing::info!(
        target: "performance",
        event = "http_response_ready",
        method,
        route,
        status = response.status().as_u16(),
        elapsed_ms = milliseconds(started.elapsed()),
        declared_response_bytes,
    );
}

pub fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Timings for external lookup, artwork and local persistence, without entity data.
pub struct MetadataTiming {
    operation_id: u64,
    kind: &'static str,
    started: Instant,
    checkpoint: Instant,
}

impl MetadataTiming {
    pub fn new(kind: &'static str) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let started = Instant::now();
        Some(Self {
            operation_id: next_id(),
            kind,
            started,
            checkpoint: started,
        })
    }

    pub fn stage(&mut self, stage: &'static str) {
        let now = Instant::now();
        tracing::info!(
            target: "performance",
            event = "metadata_pipeline_stage",
            operation_id = self.operation_id,
            kind = self.kind,
            stage,
            stage_ms = milliseconds(now.duration_since(self.checkpoint)),
            elapsed_ms = milliseconds(now.duration_since(self.started)),
        );
        self.checkpoint = now;
    }

    pub fn finish(self, outcome: &'static str, updated: bool) {
        tracing::info!(
            target: "performance",
            event = "metadata_pipeline_finished",
            operation_id = self.operation_id,
            kind = self.kind,
            outcome,
            updated,
            elapsed_ms = milliseconds(self.started.elapsed()),
        );
    }
}

#[derive(Clone, Copy)]
pub struct OperationTiming {
    pub session_id: u64,
    pub operation_id: u64,
    pub kind: &'static str,
    pub request_id: Option<u64>,
    pub seek_id: Option<u32>,
    started: Instant,
}

impl OperationTiming {
    pub fn new(
        session_id: u64,
        kind: &'static str,
        request_id: Option<u64>,
        seek_id: Option<u32>,
    ) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let timing = Self {
            session_id,
            operation_id: next_id(),
            kind,
            request_id,
            seek_id,
            started: Instant::now(),
        };
        timing.stage("command_received");
        Some(timing)
    }

    pub fn stage(&self, event: &'static str) {
        tracing::info!(
            target: "performance",
            event,
            session_id = self.session_id,
            operation_id = self.operation_id,
            operation = self.kind,
            request_id = self.request_id,
            seek_id = self.seek_id,
            elapsed_ms = milliseconds(self.started.elapsed()),
        );
    }
}

pub struct StreamProfile {
    pub timing: OperationTiming,
    first_frame_observed: bool,
    first_frame_submitted: bool,
    submitted_bytes: u64,
    interval_submitted_bytes: u64,
    blocked_calls: u64,
    last_sample: Instant,
}

impl StreamProfile {
    pub fn new(timing: OperationTiming) -> Self {
        Self {
            timing,
            first_frame_observed: false,
            first_frame_submitted: false,
            submitted_bytes: 0,
            interval_submitted_bytes: 0,
            blocked_calls: 0,
            last_sample: Instant::now(),
        }
    }

    pub fn observed(&mut self, bytes: &[u8]) {
        if !self.first_frame_observed && is_audio_frame(bytes) {
            self.first_frame_observed = true;
            self.timing.stage("first_audio_frame_observed");
        }
    }

    pub fn submitted(&mut self, bytes: usize, audio_frame_completed: bool) {
        self.submitted_bytes = self.submitted_bytes.saturating_add(bytes as u64);
        self.interval_submitted_bytes = self.interval_submitted_bytes.saturating_add(bytes as u64);
        if !self.first_frame_submitted && audio_frame_completed {
            self.first_frame_submitted = true;
            self.timing.stage("first_audio_frame_submitted");
        }
    }

    pub fn blocked(&mut self) {
        self.blocked_calls = self.blocked_calls.saturating_add(1);
    }

    pub fn sample(
        &mut self,
        stream_id: u64,
        buffered_bytes: usize,
        client_buffer_ms: u32,
        paused: bool,
    ) {
        let now = Instant::now();
        let interval = now.duration_since(self.last_sample);
        tracing::info!(
            target: "performance",
            event = "quic_stream_sample",
            session_id = self.timing.session_id,
            operation_id = self.timing.operation_id,
            operation = self.timing.kind,
            stream_id,
            elapsed_ms = milliseconds(self.timing.started.elapsed()),
            interval_ms = milliseconds(interval),
            submitted_bytes = self.submitted_bytes,
            interval_submitted_bytes = self.interval_submitted_bytes,
            submitted_bytes_per_second = self.interval_submitted_bytes as f64 / interval.as_secs_f64().max(f64::EPSILON),
            blocked_calls = self.blocked_calls,
            buffered_bytes,
            client_buffer_ms,
            paused,
        );
        self.interval_submitted_bytes = 0;
        self.blocked_calls = 0;
        self.last_sample = now;
    }
}

// Producer chunks preserve raw protocol record boundaries. Header, EOS and seek
// reset records must not be reported as the first encoded audio frame.
pub fn is_audio_frame(bytes: &[u8]) -> bool {
    if bytes.starts_with(b"OPUSR01\0") || bytes.len() < 3 {
        return false;
    }
    let length = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
    length != 0 && length != u16::MAX as usize && bytes.len() == length + 2
}

#[cfg(test)]
mod tests {
    use super::is_audio_frame;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use std::io::{self, Write};
    use std::sync::Arc;

    #[derive(Clone)]
    struct Capture(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn idle_console_polls_do_not_write_trace_or_profile_logs_but_failures_do() {
        let output = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let writer = Capture(output.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..100 {
                for path in ["/logs/tail?lines=1000", "/logs/issues", "/status/library"] {
                    let request = Request::builder().uri(path).body(Body::empty()).unwrap();
                    assert!(super::is_console_poll(
                        request.method(),
                        request.uri().path()
                    ));
                    let mut response = axum::response::Response::new(Body::empty());
                    response.extensions_mut().insert(super::ConsolePollResponse);
                    let span = super::trace_span(&request);
                    super::trace_request(&request, &span);
                    super::trace_response(&response, std::time::Duration::from_millis(1), &span);
                    super::trace_end(None, std::time::Duration::from_millis(1), &span);
                    // Exercise the profile-enabled completion path explicitly,
                    // independently of the process-wide environment switch.
                    super::log_http_measurement(
                        &response,
                        "GET",
                        request.uri().path(),
                        std::time::Instant::now(),
                    );
                }
            }
            assert!(
                output.lock().is_empty(),
                "idle observations generated log writes"
            );

            for status in [
                StatusCode::UNAUTHORIZED,
                StatusCode::FORBIDDEN,
                StatusCode::INTERNAL_SERVER_ERROR,
            ] {
                let mut response = axum::response::Response::new(Body::empty());
                response.extensions_mut().insert(super::ConsolePollResponse);
                *response.status_mut() = status;
                super::trace_response(
                    &response,
                    std::time::Duration::from_millis(1),
                    &tracing::Span::none(),
                );
                super::log_http_measurement(
                    &response,
                    "GET",
                    "/logs/tail",
                    std::time::Instant::now(),
                );
            }
            let request = Request::builder()
                .method(Method::POST)
                .uri("/logs/clear")
                .body(Body::empty())
                .unwrap();
            assert!(!super::is_console_poll(
                request.method(),
                request.uri().path()
            ));
            let span = super::trace_span(&request);
            super::trace_request(&request, &span);
            let response = axum::response::Response::new(Body::empty());
            super::trace_response(&response, std::time::Duration::from_millis(1), &span);
            super::trace_end(None, std::time::Duration::from_millis(1), &span);
            super::log_http_measurement(
                &response,
                "POST",
                "/logs/clear",
                std::time::Instant::now(),
            );
        });
        let contents = String::from_utf8(output.lock().clone()).unwrap();
        assert_eq!(contents.matches("finished processing request").count(), 4);
        assert_eq!(contents.matches("http_response_ready").count(), 4);
        assert!(contents.contains("started processing request"));
        assert_eq!(contents.matches("end of stream").count(), 1);
        for status in [401, 403, 500, 200] {
            assert!(contents.contains(&format!("status={status}")));
        }
    }

    #[test]
    fn first_audio_frame_excludes_metadata_eos_seek_reset_and_partial_records() {
        assert!(!is_audio_frame(b"OPUSR01\0metadata"));
        assert!(!is_audio_frame(&[0, 0]));
        assert!(!is_audio_frame(&[0xff, 0xff, 1, 0, 0, 0]));
        assert!(!is_audio_frame(&[3, 0, 1]));
        assert!(is_audio_frame(&[3, 0, 1, 2, 3]));
    }
}
