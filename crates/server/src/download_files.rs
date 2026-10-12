//! Shared file identity, response metadata, and checksum behavior for both download protocols.
use crate::range::RangeError;
use axum::{
    body::Body,
    http::{header, HeaderValue, StatusCode},
    response::Response,
};
use common::Track;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path as FsPath;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;
use tokio::io::AsyncReadExt;

static DOWNLOAD_CHECKSUM_CACHE: OnceLock<Mutex<HashMap<String, CachedChecksum>>> = OnceLock::new();

#[derive(Clone)]
struct CachedChecksum {
    modified_ns: u128,
    byte_length: u64,
    sha256: String,
}

pub(crate) fn range_error_response(_err: RangeError, size: u64) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let content_range = format!("bytes */{}", size);
    if let Ok(value) = HeaderValue::from_str(&content_range) {
        response.headers_mut().insert(header::CONTENT_RANGE, value);
    }
    response
}

pub(crate) fn download_content_type(path: &std::path::Path) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .essence_str()
        .to_string()
}

pub(crate) fn download_content_disposition(track: &Track, path: &std::path::Path) -> String {
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or("audio");
    let filename = format!(
        "{}.{}",
        sanitize_ascii_filename(&track.id),
        sanitize_ascii_filename(ext)
    );
    format!("attachment; filename=\"{}\"", filename)
}

pub(crate) fn download_etag(track: &Track, size: u64) -> String {
    format!("\"{}-{}\"", sanitize_ascii_filename(&track.id), size)
}

pub(crate) fn download_etag_from_metadata(track: &Track, metadata: &std::fs::Metadata) -> String {
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        "\"{}-{}-{}\"",
        sanitize_ascii_filename(&track.id),
        metadata.len(),
        modified_ns
    )
}

pub(crate) async fn checksum_for_file(
    path: &FsPath,
    metadata: &std::fs::Metadata,
) -> std::io::Result<String> {
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let byte_length = metadata.len();
    let cache_key = path.to_string_lossy().to_string();
    let cache = DOWNLOAD_CHECKSUM_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache.lock().get(&cache_key).cloned() {
        if cached.modified_ns == modified_ns && cached.byte_length == byte_length {
            return Ok(cached.sha256);
        }
    }

    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let sha256 = format!("{:x}", hasher.finalize());
    cache.lock().insert(
        cache_key,
        CachedChecksum {
            modified_ns,
            byte_length,
            sha256: sha256.clone(),
        },
    );
    Ok(sha256)
}

pub(crate) fn percent_encode_path_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        let ch = char::from(*byte);
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

fn sanitize_ascii_filename(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "track".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::Codec;
    use uuid::Uuid;
    fn track() -> Track {
        Track {
            id: "track-1".to_string(),
            album_id: "album-1".to_string(),
            artist_id: "artist-1".to_string(),
            title: "Song".to_string(),
            track_no: Some(1),
            disc_no: None,
            duration_ms: 120000,
            codec: Codec::Mp3,
            sample_rate: None,
            channels: None,
            bitrate: None,
            file_relpath: String::new(),
            file_size: 0,
            genres: Vec::new(),
        }
    }

    #[tokio::test]
    async fn download_range_errors_keep_empty_416_response_and_length() {
        for error in [RangeError::Invalid, RangeError::Unsatisfiable] {
            let response = range_error_response(error, 123);
            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
            assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */123");
            assert!(axum::body::to_bytes(response.into_body(), 0)
                .await
                .unwrap()
                .is_empty());
        }
    }

    #[tokio::test]
    async fn download_file_identity_and_checksums_follow_size_and_modification_time() {
        struct TestFile(std::path::PathBuf);
        impl Drop for TestFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let file = TestFile(
            std::env::temp_dir().join(format!("phonolite-download-{}.mp3", Uuid::new_v4())),
        );
        std::fs::write(&file.0, b"abc").unwrap();
        let modified = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file.0)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let metadata = std::fs::metadata(&file.0).unwrap();
        let mut track = track();
        let legacy_etag = download_etag(&track, metadata.len());
        let versioned_etag = download_etag_from_metadata(&track, &metadata);
        assert_eq!(legacy_etag, "\"track-1-3\"");
        let first = checksum_for_file(&file.0, &metadata).await.unwrap();
        assert_eq!(
            first,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(checksum_for_file(&file.0, &metadata).await.unwrap(), first);

        std::fs::write(&file.0, b"xyz").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file.0)
            .unwrap()
            .set_modified(modified + std::time::Duration::from_secs(1))
            .unwrap();
        let changed = std::fs::metadata(&file.0).unwrap();
        assert_eq!(download_etag(&track, changed.len()), legacy_etag);
        assert_ne!(
            download_etag_from_metadata(&track, &changed),
            versioned_etag
        );
        assert_ne!(checksum_for_file(&file.0, &changed).await.unwrap(), first);

        track.id = "track/\"\r\n名".to_string();
        assert_eq!(
            download_content_disposition(&track, &file.0),
            "attachment; filename=\"track_____.mp3\""
        );
        assert_eq!(
            percent_encode_path_component("a/b &名"),
            "a%2Fb%20%26%E5%90%8D"
        );
    }
}
