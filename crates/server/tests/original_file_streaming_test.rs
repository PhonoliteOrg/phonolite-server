//! Exercise unchanged source delivery and HTTP seeking without opening a listener.
#![allow(dead_code)]

use std::path::PathBuf;

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::routing::get;
use axum::{extract::State, Router};
use common::{Codec, Track};
use tower::ServiceExt;

#[path = "../src/download_files.rs"]
mod download_files;
#[path = "../src/range.rs"]
mod range;
#[path = "../src/shutdown.rs"]
mod shutdown;

mod http {
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::Json;

    pub fn json_error_response(status: StatusCode, message: impl Into<String>) -> Response {
        (status, Json(serde_json::json!({"error": message.into()}))).into_response()
    }
}

struct SourceFile(PathBuf);

impl SourceFile {
    fn new() -> Self {
        Self::with_extension("flac")
    }

    fn with_extension(extension: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "phonolite-original-{}.{extension}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, SOURCE_BYTES).unwrap();

        Self(path)
    }
}

#[tokio::test]
async fn music_formats_have_audio_headers_and_preserve_full_and_ranged_source_bytes() {
    for (extension, content_type) in [
        ("mp3", "audio/mpeg"),
        ("flac", "audio/flac"),
        ("m4a", "audio/mp4"),
        ("mp4", "audio/mp4"),
        ("aac", "audio/aac"),
        ("opus", "audio/ogg"),
        ("ogg", "audio/ogg"),
        ("oga", "audio/ogg"),
        ("wav", "audio/wav"),
        ("aif", "audio/x-aiff"),
        ("aiff", "audio/x-aiff"),
    ] {
        for extension in [extension.to_string(), extension.to_ascii_uppercase()] {
            // Source delivery is independent of decoding: arbitrary file bytes
            // must survive both original playback and download unchanged.
            let source = SourceFile::with_extension(&extension);
            for delivery in [
                download_files::FileDelivery::OriginalStream,
                download_files::FileDelivery::Download,
            ] {
                for ranged in [false, true] {
                    let mut headers = HeaderMap::new();
                    if ranged {
                        headers.insert(header::RANGE, "bytes=4-9".parse().unwrap());
                    }
                    let response = download_files::track_file_response(
                        &track(),
                        &source.0,
                        &headers,
                        delivery,
                        &shutdown::Shutdown::new(),
                    )
                    .await;
                    let expected_bytes = if ranged {
                        &SOURCE_BYTES[4..10]
                    } else {
                        SOURCE_BYTES
                    };
                    assert_eq!(
                        response.status(),
                        if ranged {
                            StatusCode::PARTIAL_CONTENT
                        } else {
                            StatusCode::OK
                        },
                        "{extension}"
                    );
                    assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
                    assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
                    assert_eq!(
                        response.headers()[header::CONTENT_LENGTH],
                        expected_bytes.len().to_string()
                    );
                    if ranged {
                        assert_eq!(
                            response.headers()[header::CONTENT_RANGE],
                            format!("bytes 4-9/{}", SOURCE_BYTES.len())
                        );
                    }
                    match delivery {
                        download_files::FileDelivery::OriginalStream => {
                            assert_eq!(response.headers()[header::CONTENT_DISPOSITION], "inline");
                            assert!(!response.headers().contains_key("x-phonolite-sha256"));
                        }
                        download_files::FileDelivery::Download => {
                            assert_eq!(
                                response.headers()[header::CONTENT_DISPOSITION],
                                format!("attachment; filename=\"original-track.{extension}\"")
                            );
                            assert_eq!(response.headers()["x-phonolite-sha256"].len(), 64);
                        }
                    }
                    assert_eq!(
                        to_bytes(response.into_body(), 1024).await.unwrap(),
                        expected_bytes,
                        "{extension}"
                    );
                }
            }
        }
    }
}

impl Drop for SourceFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const SOURCE_BYTES: &[u8] = b"fLaC\x00\xff\x80\x01\xfe\x7funchanged source";

fn track() -> Track {
    Track {
        id: "original-track".into(),
        album_id: "album".into(),
        artist_id: "artist".into(),
        title: "High resolution source".into(),
        track_no: None,
        disc_no: None,
        duration_ms: 1000,
        codec: Codec::Flac,
        sample_rate: Some(192000),
        channels: Some(2),
        bitrate: None,
        file_relpath: "source.flac".into(),
        file_size: SOURCE_BYTES.len() as u64,
        genres: Vec::new(),
    }
}

async fn response(path: &std::path::Path, headers: &HeaderMap) -> axum::response::Response {
    download_files::track_file_response(
        &track(),
        path,
        headers,
        download_files::FileDelivery::OriginalStream,
        &shutdown::Shutdown::new(),
    )
    .await
}

#[tokio::test]
async fn original_stream_preserves_file_bytes_and_skips_download_checksum() {
    let source = SourceFile::new();
    let original = response(&source.0, &HeaderMap::new()).await;

    assert_eq!(original.status(), StatusCode::OK);
    assert_eq!(original.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(original.headers()[header::CONTENT_DISPOSITION], "inline");
    assert_eq!(original.headers()[header::CONTENT_TYPE], "audio/flac");
    assert_eq!(
        original.headers()[header::CONTENT_LENGTH],
        SOURCE_BYTES.len().to_string()
    );
    assert!(!original.headers().contains_key("x-phonolite-sha256"));
    assert_eq!(
        to_bytes(original.into_body(), 1024).await.unwrap(),
        SOURCE_BYTES
    );

    let download = download_files::track_file_response(
        &track(),
        &source.0,
        &HeaderMap::new(),
        download_files::FileDelivery::Download,
        &shutdown::Shutdown::new(),
    )
    .await;
    assert!(download.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .starts_with("attachment;"));
    assert_eq!(download.headers()["x-phonolite-sha256"].len(), 64);
    assert_eq!(
        to_bytes(download.into_body(), 1024).await.unwrap(),
        SOURCE_BYTES
    );
}

#[tokio::test]
async fn byte_ranges_allow_seeking_without_reencoding_or_reading_the_whole_source() {
    let source = SourceFile::new();
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, "bytes=4-9".parse().unwrap());
    let range = response(&source.0, &headers).await;

    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.headers()[header::CONTENT_LENGTH], "6");
    assert_eq!(
        range.headers()[header::CONTENT_RANGE],
        format!("bytes 4-9/{}", SOURCE_BYTES.len())
    );
    assert_eq!(
        to_bytes(range.into_body(), 6).await.unwrap(),
        &SOURCE_BYTES[4..10]
    );

    headers.insert(header::RANGE, "bytes=-3".parse().unwrap());
    let suffix = response(&source.0, &headers).await;
    assert_eq!(suffix.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        to_bytes(suffix.into_body(), 3).await.unwrap(),
        &SOURCE_BYTES[SOURCE_BYTES.len() - 3..]
    );
}

#[tokio::test]
async fn if_range_uses_source_identity_and_invalid_ranges_have_empty_416_bodies() {
    let source = SourceFile::new();
    let metadata = response(&source.0, &HeaderMap::new()).await;
    let etag = metadata.headers()[header::ETAG].clone();
    drop(metadata);
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, "bytes=1-3".parse().unwrap());
    headers.insert(header::IF_RANGE, etag);
    let matching = response(&source.0, &headers).await;
    assert_eq!(matching.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        to_bytes(matching.into_body(), 3).await.unwrap(),
        &SOURCE_BYTES[1..4]
    );

    headers.insert(header::IF_RANGE, "\"old-source\"".parse().unwrap());
    let changed = response(&source.0, &headers).await;
    assert_eq!(changed.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(changed.into_body(), 1024).await.unwrap(),
        SOURCE_BYTES
    );

    headers.remove(header::IF_RANGE);
    headers.insert(header::RANGE, "bytes=999-1000".parse().unwrap());
    let invalid = response(&source.0, &headers).await;
    assert_eq!(invalid.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        invalid.headers()[header::CONTENT_RANGE],
        format!("bytes */{}", SOURCE_BYTES.len())
    );
    assert!(to_bytes(invalid.into_body(), 0).await.unwrap().is_empty());
}

#[tokio::test]
async fn head_exposes_original_file_metadata_without_sending_a_body() {
    let source = SourceFile::new();
    let router = Router::new()
        .route(
            "/original",
            get(
                |State(path): State<PathBuf>, headers: HeaderMap| async move {
                    response(&path, &headers).await
                },
            ),
        )
        .with_state(source.0.clone());
    let response = router
        .oneshot(
            Request::builder()
                .method(Method::HEAD)
                .uri("/original")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_LENGTH],
        SOURCE_BYTES.len().to_string()
    );
    assert!(response.headers().contains_key(header::ETAG));
    assert!(to_bytes(response.into_body(), 0).await.unwrap().is_empty());
}
