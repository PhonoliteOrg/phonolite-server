use axum::body::HttpBody;
use common::Track;
use library::Library;

use crate::transcode::{RawOpusMeta, TranscodeMode, TranscodeQuality};

/// These are the HTTP audio file routes. Other downloads, metadata events,
/// artwork and idle authenticated clients are not audio transfers.
pub(crate) fn is_http_audio_transfer(method: &axum::http::Method, path: &str) -> bool {
    if *method != axum::http::Method::GET {
        return false;
    }
    if let Some(track) = path.strip_prefix("/stream/") {
        return matches!(track.split_once('/'), Some((id, "original")) if !id.is_empty());
    }
    let Some(track) = path.strip_prefix("/download/tracks/") else {
        return false;
    };
    match track.split_once('/') {
        None => !track.is_empty(),
        Some((id, "file")) => !id.is_empty(),
        _ => false,
    }
}

/// Retain one open audio transfer until its response body finishes, errors or
/// is dropped after a disconnect. The original body frames and size hints stay
/// intact, so this does not buffer audio or change range/download behavior.
pub(crate) fn track_http_audio_response(
    response: axum::response::Response,
    network: &crate::network_metrics::NetworkMetrics,
) -> axum::response::Response {
    if !matches!(
        response.status(),
        axum::http::StatusCode::OK | axum::http::StatusCode::PARTIAL_CONTENT
    ) || response.body().is_end_stream()
        || response
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            == Some(0)
    {
        return response;
    }
    let (parts, body) = response.into_parts();
    axum::response::Response::from_parts(
        parts,
        axum::body::Body::new(ActiveAudioBody {
            body,
            guard: Some(network.stream_started()),
        }),
    )
}

struct ActiveAudioBody {
    body: axum::body::Body,
    guard: Option<crate::network_metrics::ActiveStreamGuard>,
}

impl http_body::Body for ActiveAudioBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let result = std::pin::Pin::new(&mut this.body).poll_frame(cx);
        if matches!(&result, std::task::Poll::Ready(None | Some(Err(_))))
            || this.body.is_end_stream()
        {
            this.guard.take();
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

pub fn parse_transcode_mode(value: Option<&str>) -> Result<TranscodeMode, String> {
    let value = value.unwrap_or("auto").trim().to_ascii_lowercase();
    match value.as_str() {
        "auto" => Ok(TranscodeMode::Auto),
        "fixed" | "manual" => Ok(TranscodeMode::Fixed),
        other => Err(format!("invalid mode: {}", other)),
    }
}

pub fn parse_transcode_quality(value: Option<&str>) -> Result<TranscodeQuality, String> {
    let value = value.unwrap_or("high").trim().to_ascii_lowercase();
    match value.as_str() {
        "high" => Ok(TranscodeQuality::High),
        "medium" | "med" => Ok(TranscodeQuality::Medium),
        "low" => Ok(TranscodeQuality::Low),
        other => Err(format!("invalid quality: {}", other)),
    }
}

pub fn parse_frame_ms(value: Option<u32>) -> Result<u32, String> {
    let frame_ms = value.unwrap_or(20);
    match frame_ms {
        2 | 5 | 10 | 20 | 40 | 60 => Ok(frame_ms),
        _ => Err("invalid frame_ms (allowed 2,5,10,20,40,60)".to_string()),
    }
}

pub fn transcode_mode_label(mode: TranscodeMode) -> &'static str {
    match mode {
        TranscodeMode::Auto => "auto",
        TranscodeMode::Fixed => "fixed",
    }
}

pub fn transcode_quality_label(quality: TranscodeQuality) -> &'static str {
    match quality {
        TranscodeQuality::High => "high",
        TranscodeQuality::Medium => "medium",
        TranscodeQuality::Low => "low",
    }
}

pub fn target_bitrate_kbps(
    mode: TranscodeMode,
    quality: TranscodeQuality,
    fixed_kbps: Option<u32>,
) -> Option<u32> {
    let bitrate_bps = match mode {
        TranscodeMode::Fixed => fixed_kbps
            .map(|v| v.saturating_mul(1000))
            .unwrap_or_else(|| quality_bitrate_bps(quality)),
        TranscodeMode::Auto => quality_bitrate_bps(quality),
    };
    Some(bitrate_bps / 1000)
}

fn quality_bitrate_bps(quality: TranscodeQuality) -> u32 {
    match quality {
        TranscodeQuality::High => 160_000,
        TranscodeQuality::Medium => 96_000,
        TranscodeQuality::Low => 48_000,
    }
}

pub fn build_raw_opus_meta(library: &Library, track: &Track) -> RawOpusMeta {
    let artist_name = library
        .get_artist(&track.artist_id)
        .ok()
        .flatten()
        .map(|artist| artist.name)
        .unwrap_or_else(|| "Unknown Artist".to_string());
    let album_title = library
        .get_album(&track.album_id)
        .ok()
        .flatten()
        .map(|album| album.title)
        .unwrap_or_else(|| "Unknown Album".to_string());

    RawOpusMeta {
        track_id: track.id.clone(),
        title: track.title.clone(),
        artist: artist_name,
        album: album_title,
        duration_ms: track.duration_ms,
        codec: "opus".to_string(),
        container: "raw".to_string(),
    }
}

#[cfg(test)]
mod audio_transfer_tests {
    use super::{is_http_audio_transfer, track_http_audio_response};
    use crate::network_metrics::NetworkMetrics;
    use axum::body::{Body, HttpBody};
    use axum::http::{header, Method, Response, StatusCode};
    use bytes::Bytes;
    use std::pin::Pin;

    #[test]
    fn only_audio_file_get_routes_are_transfers() {
        for path in [
            "/download/tracks/track",
            "/download/tracks/track/file",
            "/stream/track/original",
        ] {
            assert!(is_http_audio_transfer(&Method::GET, path));
            assert!(!is_http_audio_transfer(&Method::HEAD, path));
            assert!(!is_http_audio_transfer(&Method::POST, path));
        }
        for path in [
            "/download/tracks/",
            "/download/tracks//file",
            "/download/tracks/track/offline-metadata",
            "/download/tracks/track/file/extra",
            "/stream//original",
            "/stream/track/original/extra",
            "/stream/track/opus",
            "/download/jobs/job/events",
            "/library/artists/artist/cover",
            "/library/metadata-events",
            "/performance/data",
        ] {
            assert!(!is_http_audio_transfer(&Method::GET, path), "{path}");
        }
    }

    #[test]
    fn empty_and_unsuccessful_audio_responses_do_not_count() {
        let network = NetworkMetrics::default();
        let empty = track_http_audio_response(Response::new(Body::empty()), &network);
        assert_eq!(network.active_streams(), 0);
        drop(empty);
        let known_empty = Response::builder()
            .header(header::CONTENT_LENGTH, "0")
            .body(Body::from_stream(futures_util::stream::pending::<
                Result<Bytes, std::io::Error>,
            >()))
            .unwrap();
        let known_empty = track_http_audio_response(known_empty, &network);
        assert_eq!(network.active_streams(), 0);
        drop(known_empty);
        for status in [
            StatusCode::NOT_FOUND,
            StatusCode::RANGE_NOT_SATISFIABLE,
            StatusCode::NO_CONTENT,
        ] {
            let response = Response::builder()
                .status(status)
                .body(Body::from("error"))
                .unwrap();
            let response = track_http_audio_response(response, &network);
            assert_eq!(network.active_streams(), 0);
            drop(response);
        }
    }

    #[tokio::test]
    async fn final_audio_frame_releases_counter_without_replacing_headers_or_size_hint() {
        let network = NetworkMetrics::default();
        let response = Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_RANGE, "bytes 0-4/10")
            .header(header::CONTENT_LENGTH, "5")
            .body(Body::from("audio"))
            .unwrap();
        let mut response = track_http_audio_response(response, &network);
        assert_eq!(network.active_streams(), 1);
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-4/10");
        assert_eq!(response.body().size_hint().exact(), Some(5));
        let frame = std::future::poll_fn(|cx| Pin::new(response.body_mut()).poll_frame(cx))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame.into_data().unwrap(), Bytes::from_static(b"audio"));
        assert_eq!(network.active_streams(), 0);
        drop(response);
        assert_eq!(network.active_streams(), 0);
    }

    #[tokio::test]
    async fn errored_audio_body_releases_counter_and_preserves_the_error() {
        let network = NetworkMetrics::default();
        let response = Response::new(Body::from_stream(futures_util::stream::iter([Err::<
            Bytes,
            _,
        >(
            std::io::Error::other("audio source failed"),
        )])));
        let mut response = track_http_audio_response(response, &network);
        assert_eq!(network.active_streams(), 1);
        let result = std::future::poll_fn(|cx| Pin::new(response.body_mut()).poll_frame(cx))
            .await
            .unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("audio source failed"));
        assert_eq!(network.active_streams(), 0);
    }

    #[tokio::test]
    async fn exhausted_and_disconnected_audio_bodies_release_their_own_counters() {
        let network = NetworkMetrics::default();
        let ended = Response::new(Body::from_stream(futures_util::stream::empty::<
            Result<Bytes, std::io::Error>,
        >()));
        let mut ended = track_http_audio_response(ended, &network);
        let pending = Response::new(Body::from_stream(futures_util::stream::pending::<
            Result<Bytes, std::io::Error>,
        >()));
        let pending = track_http_audio_response(pending, &network);
        assert_eq!(network.active_streams(), 2);
        let result = std::future::poll_fn(|cx| Pin::new(ended.body_mut()).poll_frame(cx)).await;
        assert!(result.is_none());
        assert_eq!(network.active_streams(), 1);
        drop(pending);
        assert_eq!(network.active_streams(), 0);
        drop(ended);
        assert_eq!(network.active_streams(), 0);
    }
}
