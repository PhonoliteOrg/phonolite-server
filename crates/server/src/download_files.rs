//! Original-file delivery, identity and checksum behavior shared by streaming and downloads.
use crate::http::json_error_response;
use crate::range::RangeError;
use crate::range::{parse_range_header, ByteRange};
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use common::Track;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::Path as FsPath;
use std::sync::{Arc, OnceLock, Weak};
use std::time::UNIX_EPOCH;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

static DOWNLOAD_CHECKSUM_CACHE: OnceLock<Mutex<ChecksumCache>> = OnceLock::new();
static CHECKSUM_ADMISSION: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
static CHECKSUM_WORKERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
const CHECKSUM_CACHE_BYTES: usize = 1024 * 1024;
const CHECKSUM_CACHE_ENTRIES: usize = 1024;

#[derive(Clone, Copy)]
pub(crate) enum FileDelivery {
    Download,
    OriginalStream,
}

/// Streams unchanged source bytes. Original playback never waits for a full-file
/// checksum; durable downloads retain their checksum and attachment contract.
pub(crate) async fn track_file_response(
    track: &Track,
    path: &FsPath,
    request_headers: &HeaderMap,
    delivery: FileDelivery,
    shutdown: &crate::shutdown::Shutdown,
) -> Response {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(_) => return json_error_response(StatusCode::NOT_FOUND, "track file not found"),
    };
    let metadata = match file.metadata().await {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return json_error_response(StatusCode::NOT_FOUND, "track file not found"),
    };
    let size = metadata.len();
    let etag = download_etag_from_metadata(track, &metadata);
    let sha256 = match delivery {
        FileDelivery::Download => checksum_for_file_cancellable(path, &metadata, shutdown)
            .await
            .ok(),
        FileDelivery::OriginalStream => None,
    };
    let range_header = request_headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let if_range_matches = request_headers
        .get(header::IF_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim() == etag.as_str())
        .unwrap_or(true);
    let range = match range_header.filter(|_| if_range_matches) {
        Some(value) => match parse_range_header(value, size) {
            Ok(range) => Some(range),
            Err(err) => return range_error_response(err, size),
        },
        None => None,
    };
    let (status, start, end, content_len) = match range {
        Some(ByteRange { start, end }) => {
            if let Err(err) = file.seek(std::io::SeekFrom::Start(start)).await {
                return json_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("file seek failed: {}", err),
                );
            }
            let len = end.saturating_sub(start).saturating_add(1);
            (StatusCode::PARTIAL_CONTENT, start, Some(end), len)
        }
        None => (StatusCode::OK, 0, None, size),
    };

    let stream = ReaderStream::new(file.take(content_len));
    let mut response = Response::builder().status(status);
    let headers = response.headers_mut().expect("response headers");
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&download_content_type(path))
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&content_len.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag)
            .unwrap_or_else(|_| HeaderValue::from_static("\"phonolite-download-v2\"")),
    );
    if let Some(sha256) = sha256 {
        if let Ok(value) = HeaderValue::from_str(&sha256) {
            headers.insert("x-phonolite-sha256", value);
        }
    }
    let disposition = match delivery {
        FileDelivery::Download => download_content_disposition(track, path),
        FileDelivery::OriginalStream => "inline".to_string(),
    };
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition)
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    if let Some(end) = end {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end, size))
                .unwrap_or_else(|_| HeaderValue::from_static("bytes */0")),
        );
    }

    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[derive(Default)]
struct ChecksumCache {
    entries: HashMap<String, CachedChecksum>,
    order: VecDeque<String>,
    fills: HashMap<String, Weak<tokio::sync::Mutex<()>>>,
    bytes: usize,
}

impl ChecksumCache {
    fn charge(key: &str, value: &CachedChecksum) -> usize {
        key.len()
            .saturating_mul(2)
            .saturating_add(value.sha256.len())
            .saturating_add(192)
    }

    fn get(&mut self, key: &str, modified_ns: u128, byte_length: u64) -> Option<String> {
        let value = self.entries.get(key)?;
        if value.modified_ns != modified_ns || value.byte_length != byte_length {
            return None;
        }
        self.order.retain(|entry| entry != key);
        self.order.push_back(key.to_owned());
        Some(value.sha256.clone())
    }

    fn insert(&mut self, key: String, value: CachedChecksum) {
        if let Some(old) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(Self::charge(&key, &old));
        }
        self.order.retain(|entry| entry != &key);
        let charge = Self::charge(&key, &value);
        if charge > CHECKSUM_CACHE_BYTES {
            return;
        }
        while self.bytes + charge > CHECKSUM_CACHE_BYTES
            || self.entries.len() >= CHECKSUM_CACHE_ENTRIES
        {
            let Some(victim) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.entries.remove(&victim) {
                self.bytes = self.bytes.saturating_sub(Self::charge(&victim, &old));
            }
        }
        self.bytes += charge;
        self.order.push_back(key.clone());
        self.entries.insert(key, value);
    }
}

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
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    // These paths identify music files, including audio-only MP4 containers.
    // Generic extension guessing otherwise labels .mp4 as video/mp4.
    match extension.as_deref() {
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("m4a" | "mp4") => "audio/mp4",
        Some("aac") => "audio/aac",
        Some("opus" | "ogg" | "oga") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("aif" | "aiff") => "audio/x-aiff",
        _ => {
            return mime_guess::from_path(path)
                .first_or_octet_stream()
                .essence_str()
                .to_string();
        }
    }
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

pub(crate) async fn checksum_for_file_cancellable(
    path: &FsPath,
    metadata: &std::fs::Metadata,
    shutdown: &crate::shutdown::Shutdown,
) -> std::io::Result<String> {
    let _job = shutdown
        .track_operation("download_checksum")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "server shutting down",
            )
        })?;
    let operation = async {
        let modified_ns = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let byte_length = metadata.len();
        let cache_key = path.to_string_lossy().to_string();
        let cache = DOWNLOAD_CHECKSUM_CACHE.get_or_init(|| Mutex::new(ChecksumCache::default()));
        if let Some(cached) = cache.lock().get(&cache_key, modified_ns, byte_length) {
            return Ok(cached);
        }

        // Bound both queued callers and their per-key coalescing state.
        let _admitted = CHECKSUM_ADMISSION
            .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(32)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::WouldBlock, "checksum capacity reached")
            })?;
        let fill = {
            let mut state = cache.lock();
            state.fills.retain(|_, fill| fill.strong_count() > 0);
            if let Some(fill) = state.fills.get(&cache_key).and_then(Weak::upgrade) {
                fill
            } else {
                let fill = Arc::new(tokio::sync::Mutex::new(()));
                state.fills.insert(cache_key.clone(), Arc::downgrade(&fill));
                fill
            }
        };
        let _fill = fill.lock().await;
        if let Some(cached) = cache.lock().get(&cache_key, modified_ns, byte_length) {
            return Ok(cached);
        }
        let _worker = CHECKSUM_WORKERS
            .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| std::io::Error::other("checksum workers stopped"))?;

        let mut file = tokio::fs::File::open(path).await?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            if shutdown.is_cancelled() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "server shutting down",
                ));
            }
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let sha256 = format!("{:x}", hasher.finalize());
        let current = file.metadata().await?;
        let current_modified = current
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        if current.len() != byte_length || current_modified != modified_ns {
            return Err(std::io::Error::other(
                "download source changed while checksumming",
            ));
        }
        if shutdown.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "server shutting down",
            ));
        }
        cache.lock().insert(
            cache_key,
            CachedChecksum {
                modified_ns,
                byte_length,
                sha256: sha256.clone(),
            },
        );
        Ok(sha256)
    };
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "server shutting down")),
        result = operation => result,
    }
}

#[cfg(test)]
async fn checksum_for_file(path: &FsPath, metadata: &std::fs::Metadata) -> std::io::Result<String> {
    checksum_for_file_cancellable(path, metadata, &crate::shutdown::Shutdown::new()).await
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

    #[test]
    fn non_music_content_types_keep_the_generic_fallback() {
        assert_eq!(download_content_type(FsPath::new("cover.png")), "image/png");
        assert_eq!(
            download_content_type(FsPath::new("unknown.phonolite-unknown")),
            "application/octet-stream"
        );
        assert_eq!(
            download_content_type(FsPath::new("no-extension")),
            "application/octet-stream"
        );
    }

    #[tokio::test]
    async fn cancelled_checksum_does_not_wait_for_a_fill_or_open_the_source() {
        let path =
            std::env::temp_dir().join(format!("phonolite-checksum-cancel-{}", Uuid::new_v4()));
        std::fs::write(&path, b"unpublished").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let cache = DOWNLOAD_CHECKSUM_CACHE.get_or_init(|| Mutex::new(ChecksumCache::default()));
        let fill = Arc::new(tokio::sync::Mutex::new(()));
        cache
            .lock()
            .fills
            .insert(path.to_string_lossy().to_string(), Arc::downgrade(&fill));
        let held = fill.lock().await;
        let shutdown = crate::shutdown::Shutdown::new();
        let flag = shutdown.clone();
        let request_path = path.clone();
        let job = tokio::spawn(async move {
            checksum_for_file_cancellable(&request_path, &metadata, &flag).await
        });
        tokio::task::yield_now().await;
        shutdown.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), job).await;
        drop(held);
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            result.unwrap().unwrap().unwrap_err().kind(),
            std::io::ErrorKind::ConnectionAborted
        );
        assert!(shutdown.drain(std::time::Duration::from_secs(1)).await);
    }

    #[test]
    fn checksum_history_is_bounded_and_recent_files_remain_reusable() {
        let mut cache = ChecksumCache::default();
        for index in 0..5000 {
            cache.insert(
                format!("file-{index}"),
                CachedChecksum {
                    modified_ns: 10,
                    byte_length: 20,
                    sha256: "a".repeat(64),
                },
            );
        }
        assert!(cache.entries.len() <= CHECKSUM_CACHE_ENTRIES);
        assert!(cache.bytes <= CHECKSUM_CACHE_BYTES);
        assert!(cache.get("file-0", 10, 20).is_none());
        assert!(cache.get("file-4999", 10, 20).is_some());
        assert!(cache.get("file-4999", 11, 20).is_none());
    }
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
