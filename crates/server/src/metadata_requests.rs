//! Incoming catalogue/artwork diagnostics shared by the API and administration UI.
//! Bodies remain streamed; only bounded JSON error messages are retained.
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::{Body, HttpBody};
use axum::extract::{MatchedPath, Request};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

const ERROR_PREVIEW_BYTES: usize = 4096;

pub(crate) fn request_kind(path: &str) -> Option<&'static str> {
    let path = path.strip_prefix("/api/").unwrap_or(path);
    let path = path.trim_start_matches('/');
    if path.starts_with("browse/") || path == "library/search" || path == "library" {
        Some("catalogue")
    } else if path.starts_with("covers/")
        || (path.starts_with("library/") && path.ends_with("/cover"))
    {
        Some("artwork")
    } else if path == "library/events" || path == "library/metadata-events" {
        Some("metadata_events")
    } else if path.starts_with("library/metadata/snapshots/")
        || (path.starts_with("library/tracks/") && path.ends_with("/offline-metadata"))
        || path == "library/match-tracks"
        || path
            .strip_prefix("library/albums/")
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
    {
        Some("metadata")
    } else if path.starts_with("settings/metadata/")
        || matches!(path, "settings/scan" | "settings/reindex")
    {
        Some("metadata_control")
    } else {
        None
    }
}

#[derive(Clone)]
pub(crate) struct MetadataResponse;

struct RequestLog {
    request_id: u64,
    kind: &'static str,
    method: String,
    route: String,
    path: String,
    started: Instant,
    response_ready: bool,
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        if !self.response_ready {
            tracing::warn!(target: "metadata_request", event = "metadata_request_cancelled",
                request_id = self.request_id, kind = self.kind, method = self.method,
                route = self.route, path = self.path,
                elapsed_ms = crate::performance::milliseconds(self.started.elapsed()));
        }
    }
}

pub(crate) async fn log_request(request: Request, next: Next) -> Response {
    let Some(kind) = request_kind(request.uri().path()) else {
        return next.run(request).await;
    };
    let mut log = RequestLog {
        request_id: crate::performance::next_id(),
        kind,
        method: request.method().to_string(),
        route: request
            .extensions()
            .get::<MatchedPath>()
            .map(|path| path.as_str().to_owned())
            .unwrap_or_else(|| "unmatched".into()),
        // Retain the item ID for diagnosis, without query strings or credentials.
        path: request.uri().path().chars().take(1024).collect(),
        started: Instant::now(),
        response_ready: false,
    };
    tracing::info!(target: "metadata_request", event = "metadata_request_started",
        request_id = log.request_id, kind, method = log.method, route = log.route, path = log.path);
    let mut response = next.run(request).await;
    log.response_ready = true;
    response.extensions_mut().insert(MetadataResponse);
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(128)
        .collect::<String>();
    let response_bytes = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| response.body().size_hint().exact());
    let retry_after_secs = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let outcome = response_outcome(status);
    let elapsed_ms = crate::performance::milliseconds(log.started.elapsed());
    macro_rules! ready {
        ($level:ident) => { tracing::$level!(target: "metadata_request", event = "metadata_response_ready",
            request_id = log.request_id, kind, method = log.method, route = log.route, path = log.path,
            status = status.as_u16(), response = status.canonical_reason().unwrap_or("Unknown"),
            outcome, elapsed_ms, content_type, response_bytes, retry_after_secs); };
    }
    if status.is_server_error() {
        ready!(error);
    } else if status.is_client_error() {
        ready!(warn);
    } else {
        ready!(info);
    }

    let bodyless = log.method == "HEAD"
        || status.is_informational()
        || matches!(status, StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED)
        || response.body().is_end_stream();
    let mut body_log = BodyLog {
        request: log,
        status,
        received_bytes: 0,
        expected_bytes: response_bytes,
        finished: false,
        capture_error: status.is_client_error() || status.is_server_error(),
        json_error: content_type.starts_with("application/json"),
        error_preview: Vec::new(),
    };
    if bodyless {
        body_log.finish("completed", None);
    }
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(LoggedBody {
            body,
            log: body_log,
        }),
    )
}

struct LoggedBody {
    body: Body,
    log: BodyLog,
}

impl HttpBody for LoggedBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.body).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(bytes) = frame.data_ref() {
                    this.log.received_bytes =
                        this.log.received_bytes.saturating_add(bytes.len() as u64);
                    if this.log.capture_error && this.log.json_error {
                        let keep = bytes
                            .len()
                            .min(ERROR_PREVIEW_BYTES.saturating_sub(this.log.error_preview.len()));
                        this.log.error_preview.extend_from_slice(&bytes[..keep]);
                    }
                }
                if frame.is_trailers() || this.body.is_end_stream() {
                    this.log.finish("completed", None);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.log.finish("body_error", Some(&error.to_string()));
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.log.finish("completed", None);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

fn response_outcome(status: StatusCode) -> &'static str {
    match status {
        StatusCode::NOT_MODIFIED => "not_modified",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::TOO_MANY_REQUESTS => "throttled",
        StatusCode::SERVICE_UNAVAILABLE => "unavailable",
        _ if status.is_server_error() => "server_error",
        _ if status.is_client_error() => "rejected",
        _ if status.is_redirection() => "redirected",
        _ => "success",
    }
}

struct BodyLog {
    request: RequestLog,
    status: StatusCode,
    received_bytes: u64,
    expected_bytes: Option<u64>,
    finished: bool,
    capture_error: bool,
    json_error: bool,
    error_preview: Vec<u8>,
}

impl BodyLog {
    fn finish(&mut self, outcome: &'static str, stream_error: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let response_error = serde_json::from_slice::<serde_json::Value>(&self.error_preview)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|value| value.as_str())
                    .map(|value| value.chars().take(1024).collect::<String>())
            });
        let error = stream_error.or(response_error.as_deref()).unwrap_or("");
        macro_rules! finished {
            ($level:ident) => { tracing::$level!(target: "metadata_request", event = "metadata_response_finished",
                request_id = self.request.request_id, kind = self.request.kind,
                method = self.request.method, route = self.request.route, path = self.request.path,
                status = self.status.as_u16(), outcome, response_bytes = self.received_bytes, error,
                elapsed_ms = crate::performance::milliseconds(self.request.started.elapsed())); };
        }
        if self.status.is_server_error() || stream_error.is_some() {
            finished!(error);
        } else if self.status.is_client_error() || outcome == "cancelled" {
            finished!(warn);
        } else {
            finished!(info);
        }
    }
}

impl Drop for BodyLog {
    fn drop(&mut self) {
        let outcome = if self.expected_bytes == Some(self.received_bytes) {
            "completed"
        } else if self.request.kind == "metadata_events" {
            "closed"
        } else {
            "cancelled"
        };
        self.finish(outcome, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{self, Write};
    use std::sync::Arc;

    use axum::http::{HeaderMap, Method};
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;
    use tracing::instrument::WithSubscriber;

    #[derive(Clone, Default)]
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

    impl Capture {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().clone()).unwrap()
        }
        fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync {
            let writer = self.clone();
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish()
        }
    }

    #[test]
    fn classifies_catalogue_artwork_metadata_and_controls_without_audio_or_log_polls() {
        for path in [
            "/api/browse/artists",
            "/browse/artists/id/releases",
            "/browse/albums/id/tracks",
            "/library/search",
            "/library/albums/id",
            "/library/tracks/id/offline-metadata",
            "/library/metadata/snapshots/album/id",
            "/library/match-tracks",
            "/library/events",
            "/library/metadata-events",
            "/api/library/albums/id/cover",
            "/library/artists/id/cover",
            "/library/playlists/id/cover",
            "/covers/albums/id",
            "/covers/artists/id",
            "/library",
            "/settings/metadata/test",
            "/settings/metadata/id/update",
            "/settings/scan",
            "/settings/reindex",
        ] {
            assert!(
                request_kind(path).is_some(),
                "missing metadata route {path}"
            );
        }
        for path in [
            "/download/tracks/id",
            "/api/download/tracks/id/file",
            "/auth/opaque/start",
            "/api/stats",
            "/player",
            "/logs",
            "/logs/tail",
            "/status/library",
            "/assets/logo.png",
            "/health",
            "/library/albums/id/unrelated",
        ] {
            assert!(
                request_kind(path).is_none(),
                "unexpected metadata route {path}"
            );
        }
    }

    #[tokio::test]
    async fn records_request_status_response_errors_and_bytes_without_buffering_success_payloads() {
        let capture = Capture::default();
        async {
            for status in [200, 304, 400, 401, 404, 409, 429, 500, 503] {
                let app = Router::new()
                    .route(
                        "/library/albums/:album_id/cover",
                        get(move || async move {
                            let mut response = Response::new(Body::from(if status >= 400 {
                                r#"{"error":"cover not found"}"#
                            } else {
                                "private successful payload"
                            }));
                            *response.status_mut() = StatusCode::from_u16(status).unwrap();
                            response
                                .headers_mut()
                                .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
                            response
                                .headers_mut()
                                .insert(header::RETRY_AFTER, "1".parse().unwrap());
                            response
                        }),
                    )
                    .layer(middleware::from_fn(log_request));
                let response = app
                    .oneshot(
                        Request::builder()
                            .uri("/library/albums/album-123/cover?token=secret-query")
                            .header(header::AUTHORIZATION, "Bearer secret-auth")
                            .header(header::COOKIE, "secret-cookie")
                            .header("x-request-id", "secret-request-id")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert!(response.extensions().get::<MetadataResponse>().is_some());
                axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
            }
        }
        .with_subscriber(capture.subscriber())
        .await;
        let output = capture.contents();
        assert_eq!(output.matches("metadata_request_started").count(), 9);
        assert_eq!(output.matches("metadata_response_ready").count(), 9);
        assert_eq!(output.matches("metadata_response_finished").count(), 9);
        for status in [200, 304, 400, 401, 404, 409, 429, 500, 503] {
            assert!(
                output.contains(&format!("status={status}")),
                "missing {status}"
            );
        }
        assert!(output.contains("not_modified"));
        assert!(output.contains("throttled"));
        assert!(output.contains("unavailable"));
        assert!(output.contains("cover not found"));
        assert!(output.contains("retry_after_secs=1"));
        assert!(output.contains("album-123"));
        for secret in [
            "secret-query",
            "secret-auth",
            "secret-cookie",
            "secret-request-id",
            "private successful payload",
        ] {
            assert!(!output.contains(secret), "logged {secret}");
        }
    }

    #[tokio::test]
    async fn streams_sse_without_waiting_for_body_and_reports_normal_close() {
        let capture = Capture::default();
        async {
            let app = Router::new()
                .route(
                    "/library/events",
                    get(|| async {
                        Response::new(Body::from_stream(futures_util::stream::pending::<
                            Result<bytes::Bytes, io::Error>,
                        >()))
                    }),
                )
                .layer(middleware::from_fn(log_request));
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/library/events")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(capture.contents().contains("metadata_response_ready"));
            assert!(!capture.contents().contains("metadata_response_finished"));
            drop(response);
        }
        .with_subscriber(capture.subscriber())
        .await;
        let output = capture.contents();
        assert!(output.contains("outcome=\"closed\""));
        assert!(!output.contains("WARN"));
    }

    #[tokio::test]
    async fn preserves_frames_trailers_and_size_hints_and_completes_without_extra_poll() {
        struct Frames(VecDeque<http_body::Frame<bytes::Bytes>>);
        impl HttpBody for Frames {
            type Data = bytes::Bytes;
            type Error = io::Error;
            fn poll_frame(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
                Poll::Ready(self.0.pop_front().map(Ok))
            }
            fn is_end_stream(&self) -> bool {
                self.0.is_empty()
            }
            fn size_hint(&self) -> http_body::SizeHint {
                http_body::SizeHint::with_exact(
                    self.0
                        .iter()
                        .filter_map(|frame| frame.data_ref())
                        .map(|bytes| bytes.len() as u64)
                        .sum(),
                )
            }
        }
        let capture = Capture::default();
        async {
            let app = Router::new()
                .route(
                    "/browse/artists",
                    get(|| async {
                        let mut trailers = HeaderMap::new();
                        trailers.insert("x-test-trailer", "preserved".parse().unwrap());
                        Response::new(Body::new(Frames(VecDeque::from([
                            http_body::Frame::data(bytes::Bytes::from_static(b"abc")),
                            http_body::Frame::trailers(trailers),
                        ]))))
                    }),
                )
                .layer(middleware::from_fn(log_request));
            let mut body = app
                .oneshot(
                    Request::builder()
                        .uri("/browse/artists")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .into_body();
            assert_eq!(body.size_hint().exact(), Some(3));
            let frame = futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(frame.data_ref().unwrap().as_ref(), b"abc");
            let frame = futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(frame.trailers_ref().unwrap()["x-test-trailer"], "preserved");
            assert!(body.is_end_stream());
            drop(body);
        }
        .with_subscriber(capture.subscriber())
        .await;
        let output = capture.contents();
        assert!(output.contains("response_bytes=3"));
        assert!(output.contains("outcome=\"completed\""));
        assert!(!output.contains("cancelled"));
    }

    #[tokio::test]
    async fn bodyless_responses_and_declared_length_completion_are_not_cancellations() {
        let capture = Capture::default();
        async {
            for method in [Method::GET, Method::HEAD] {
                for status in [
                    StatusCode::OK,
                    StatusCode::NO_CONTENT,
                    StatusCode::NOT_MODIFIED,
                ] {
                    let app = Router::new()
                        .route(
                            "/covers/albums/:id",
                            get(move || async move {
                                let mut response =
                                    Response::new(Body::from_stream(futures_util::stream::iter([
                                        Ok::<_, io::Error>(bytes::Bytes::from_static(b"abc")),
                                    ])));
                                *response.status_mut() = status;
                                response
                                    .headers_mut()
                                    .insert(header::CONTENT_LENGTH, "3".parse().unwrap());
                                response
                            }),
                        )
                        .layer(middleware::from_fn(log_request));
                    let mut response = app
                        .oneshot(
                            Request::builder()
                                .method(method.clone())
                                .uri("/covers/albums/id")
                                .body(Body::empty())
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    if method == Method::GET && status == StatusCode::OK {
                        let frame = futures_util::future::poll_fn(|cx| {
                            Pin::new(response.body_mut()).poll_frame(cx)
                        })
                        .await
                        .unwrap()
                        .unwrap();
                        assert_eq!(frame.data_ref().unwrap().len(), 3);
                    }
                    drop(response);
                }
            }
        }
        .with_subscriber(capture.subscriber())
        .await;
        let output = capture.contents();
        assert_eq!(output.matches("metadata_response_finished").count(), 6);
        assert!(!output.contains("cancelled"));
    }

    #[tokio::test]
    async fn nested_api_and_admin_failures_use_metadata_diagnostics_without_generic_trace_duplicates(
    ) {
        let capture = Capture::default();
        async {
            let api = Router::new().route("/library/albums/:album_id/cover", get(|| async {
                (StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({ "error": "request capacity reached; retry shortly" })))
            }));
            let api = Router::new().nest("/api", api)
                .layer(middleware::from_fn(crate::performance::http_request))
                .layer(tower_http::trace::TraceLayer::new_for_http()
                    .make_span_with(crate::performance::trace_span)
                    .on_request(crate::performance::trace_request)
                    .on_response(crate::performance::trace_response)
                    .on_eos(crate::performance::trace_end)
                    .on_failure(crate::performance::trace_failure));
            let admin = Router::new().route("/covers/albums/:album_id", get(|| async {
                (StatusCode::TOO_MANY_REQUESTS, axum::Json(serde_json::json!({ "error": "request capacity reached; retry shortly" })))
            })).layer(middleware::from_fn(log_request));
            for (app, path) in [(api, "/api/library/albums/album-123/cover"), (admin, "/covers/albums/album-123")] {
                let response = app.oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
                axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
            }
        }.with_subscriber(capture.subscriber()).await;
        let output = capture.contents();
        assert_eq!(output.matches("metadata_request_started").count(), 2);
        assert_eq!(output.matches("metadata_response_ready").count(), 2);
        assert_eq!(output.matches("metadata_response_finished").count(), 2);
        assert!(output.contains("/api/library/albums/:album_id/cover"));
        assert!(output.contains("/covers/albums/:album_id"));
        assert!(output.contains("status=503"));
        assert!(output.contains("status=429"));
        assert!(output.contains("request capacity reached; retry shortly"));
        assert!(!output.contains("finished processing request"));
        assert!(!output.contains("response failed"));
        assert!(!output.contains("http_response_ready"));
    }

    #[tokio::test]
    async fn cancelled_pending_requests_are_logged_without_a_response() {
        use std::future::Future;
        let capture = Capture::default();
        async {
            let app = Router::new()
                .route(
                    "/browse/artists",
                    get(|| async { std::future::pending::<Response>().await }),
                )
                .layer(middleware::from_fn(log_request));
            let mut pending = Box::pin(
                app.oneshot(
                    Request::builder()
                        .uri("/browse/artists")
                        .body(Body::empty())
                        .unwrap(),
                ),
            );
            let waker = futures_util::task::noop_waker();
            let mut context = Context::from_waker(&waker);
            assert!(pending.as_mut().poll(&mut context).is_pending());
            drop(pending);
        }
        .with_subscriber(capture.subscriber())
        .await;
        let output = capture.contents();
        assert!(output.contains("metadata_request_started"));
        assert!(output.contains("metadata_request_cancelled"));
        assert!(!output.contains("metadata_response_ready"));
    }

    #[tokio::test]
    async fn records_body_failures_and_partial_delivery_cancellation() {
        let capture = Capture::default();
        async {
            for failed in [false, true] {
                let app = Router::new()
                    .route(
                        "/covers/albums/:id",
                        get(move || async move {
                            let stream = futures_util::stream::iter([
                                Ok(bytes::Bytes::from_static(b"abc")),
                                Err(io::Error::other("source read failed")),
                            ]);
                            let mut response = Response::new(Body::from_stream(stream));
                            response
                                .headers_mut()
                                .insert(header::CONTENT_LENGTH, "10".parse().unwrap());
                            response
                        }),
                    )
                    .layer(middleware::from_fn(log_request));
                let mut body = app
                    .oneshot(
                        Request::builder()
                            .uri("/covers/albums/id")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .into_body();
                let _ = futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
                    .await
                    .unwrap()
                    .unwrap();
                if failed {
                    assert!(
                        futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
                            .await
                            .unwrap()
                            .is_err()
                    );
                }
                drop(body);
            }
        }
        .with_subscriber(capture.subscriber())
        .await;
        let output = capture.contents();
        assert!(output.contains("body_error"));
        assert!(output.contains("source read failed"));
        assert!(output.contains("outcome=\"cancelled\""));
        assert_eq!(output.matches("metadata_response_finished").count(), 2);
    }
}
