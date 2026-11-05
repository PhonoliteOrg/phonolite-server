//! Disposable disk thumbnails. Decoders and queued fills have separate finite
//! budgets; HTTP transfer admission belongs to the request resource owner.
use std::collections::HashMap;
use std::io::{BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::SystemTime;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use image::{ImageFormat, ImageReader};
use parking_lot::Mutex;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio_util::io::ReaderStream;

use crate::assets::CoverCacheKey;
use crate::http::json_error_response;

/// Only responses with this marker may override the authenticated API's
/// default no-store policy. Authentication is still required on every hit.
#[derive(Clone)]
pub struct AuthenticatedArtwork;

const MAX_DECODER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 20_000;

pub struct ArtworkCache {
    limits: crate::resources::ResourceLimits,
    generation: Arc<Semaphore>,
    slots: Arc<Semaphore>,
    fills: Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>,
    publication: Arc<AsyncMutex<ThumbnailInventory>>,
}

impl Default for ArtworkCache {
    fn default() -> Self {
        Self::new(&crate::resources::ResourceLimits::default())
    }
}

impl ArtworkCache {
    pub fn new(limits: &crate::resources::ResourceLimits) -> Self {
        Self {
            limits: limits.clone(),
            generation: Arc::new(Semaphore::new(limits.thumbnail_workers)),
            slots: Arc::new(Semaphore::new(
                limits.thumbnail_workers + limits.thumbnail_queued,
            )),
            fills: Mutex::new(HashMap::new()),
            publication: Arc::new(AsyncMutex::new(ThumbnailInventory::default())),
        }
    }
    pub fn valid_size(size: u32) -> bool {
        matches!(size, 128 | 256 | 512)
    }

    /// The path is a source-fingerprinted rendition. Callers validate the
    /// requested original reference before entering this operation.
    pub async fn thumbnail(
        &self,
        shutdown: &crate::shutdown::Shutdown,
        root: &Path,
        source: &Path,
        fingerprint: &str,
        size: u32,
        expected_hash: Option<&str>,
    ) -> Result<PathBuf, String> {
        ensure_running(shutdown)?;
        if !Self::valid_size(size) {
            return Err("unsupported artwork size".into());
        }
        let directory = root.join("thumbnails");
        let path = directory.join(format!("{fingerprint}-{size}-v1.png"));
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(path);
        }
        let _slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "artwork generation capacity reached")?;
        let fill = {
            let mut fills = self.fills.lock();
            fills.retain(|_, fill| fill.strong_count() > 0);
            if let Some(fill) = fills.get(&path).and_then(Weak::upgrade) {
                fill
            } else {
                if fills.len() >= 128 {
                    return Err("artwork generation capacity reached".into());
                }
                let fill = Arc::new(AsyncMutex::new(()));
                fills.insert(path.clone(), Arc::downgrade(&fill));
                fill
            }
        };
        let _fill = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Err("server shutting down".into()),
            fill = fill.lock() => fill,
        };
        ensure_running(shutdown)?;
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(path);
        }
        let permit = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Err("server shutting down".into()),
            permit = self.generation.clone().acquire_owned() => permit.map_err(|_| "artwork generator closed")?,
        };
        let bytes =
            crate::workers::thumbnail(shutdown, source, size, expected_hash, &self.limits, permit)
                .await?;
        ensure_running(shutdown)?;
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|error| error.to_string())?;
        let publication = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Err("server shutting down".into()),
            publication = self.publication.clone().lock_owned() => publication,
        };
        let temporary = directory.join(format!("{}.part", uuid::Uuid::new_v4()));
        let published = path.clone();
        let cancellation = shutdown.clone();
        let quota = self.limits.thumbnail_disk_mib as u64 * 1024 * 1024;
        shutdown
            .spawn_blocking_commit("thumbnail publication", move || {
                let mut publication = publication;
                publish_thumbnail(
                    &mut publication,
                    &directory,
                    &published,
                    &temporary,
                    &bytes,
                    quota,
                    &cancellation.flag(),
                )
            })
            .await
            .map_err(|error| error.to_string())??;
        Ok(path)
    }
}

fn ensure_running(shutdown: &crate::shutdown::Shutdown) -> Result<(), String> {
    if shutdown.is_cancelled() {
        Err("server shutting down".into())
    } else {
        Ok(())
    }
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(crate) fn render_thumbnail(
    source: &Path,
    size: u32,
    expected_hash: Option<&str>,
    config: &crate::resources::ResourceLimits,
) -> Result<Vec<u8>, String> {
    let max_input_bytes = config.thumbnail_source_mib as u64 * 1024 * 1024;
    let metadata = std::fs::metadata(source).map_err(|error| error.to_string())?;
    if metadata.len() == 0 || metadata.len() > max_input_bytes {
        return Err("artwork exceeds encoded byte limit".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(source)
        .map_err(|error| error.to_string())?
        .take(max_input_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > max_input_bytes {
        return Err("artwork exceeds encoded byte limit".into());
    }
    if expected_hash.is_some_and(|expected| blake3::hash(&bytes).to_hex().as_str() != expected) {
        return Err("artwork content version changed".into());
    }
    let reader = ImageReader::new(BufReader::new(Cursor::new(&bytes)))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| error.to_string())?;
    if width == 0
        || height == 0
        || width > config.thumbnail_max_dimension
        || height > config.thumbnail_max_dimension
        || u64::from(width) * u64::from(height) > config.thumbnail_max_pixels
    {
        return Err("artwork exceeds decoded pixel limit".into());
    }
    let mut reader = ImageReader::new(BufReader::new(Cursor::new(bytes)))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(config.thumbnail_max_dimension);
    limits.max_image_height = Some(config.thumbnail_max_dimension);
    limits.max_alloc = Some(
        config
            .thumbnail_max_pixels
            .saturating_mul(8)
            .min(MAX_DECODER_BYTES),
    );
    reader.limits(limits);
    let image = reader.decode().map_err(|error| error.to_string())?;
    let thumbnail = image.thumbnail(size, size);
    let mut encoded = Cursor::new(Vec::new());
    thumbnail
        .write_to(&mut encoded, ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(encoded.into_inner())
}

const EVICTION_CANDIDATES: usize = 128;

#[derive(Default)]
struct ThumbnailInventory {
    directory: Option<PathBuf>,
    modified: Option<SystemTime>,
    bytes: u64,
    count: usize,
    oldest: Vec<ThumbnailFile>,
    complete_window: bool,
    #[cfg(test)]
    scans: usize,
}

struct ThumbnailFile {
    path: PathBuf,
    modified: Option<SystemTime>,
    bytes: u64,
}

impl ThumbnailInventory {
    fn invalidate(&mut self) {
        self.directory = None;
        self.modified = None;
        self.oldest.clear();
        self.complete_window = false;
    }

    fn prepare(
        &mut self,
        directory: &Path,
        cancellation: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<()> {
        check_artwork_cancelled(cancellation)?;
        // Fingerprinted renditions are immutable outside this publication owner.
        // Namespace changes invalidate accounting; directory timestamps cannot
        // detect an external in-place edit of an existing rendition.
        let modified = std::fs::metadata(directory)?.modified().ok();
        if self.directory.as_deref() == Some(directory)
            && modified.is_some()
            && self.modified == modified
        {
            return Ok(());
        }
        self.scan(directory, cancellation)
    }

    fn scan(
        &mut self,
        directory: &Path,
        cancellation: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<()> {
        check_artwork_cancelled(cancellation)?;
        self.invalidate();
        self.bytes = 0;
        self.count = 0;
        #[cfg(test)]
        {
            self.scans += 1;
        }
        for file in std::fs::read_dir(directory)? {
            check_artwork_cancelled(cancellation)?;
            let file = file?;
            let path = file.path();
            if path.extension().and_then(|value| value.to_str()) != Some("png") {
                continue;
            }
            let metadata = file.metadata()?;
            self.bytes = self.bytes.saturating_add(metadata.len());
            self.count = self.count.saturating_add(1);
            self.oldest.push(ThumbnailFile {
                path,
                modified: metadata.modified().ok(),
                bytes: metadata.len(),
            });
            if self.oldest.len() > EVICTION_CANDIDATES * 2 {
                self.trim_candidates();
            }
        }
        self.trim_candidates();
        self.complete_window = self.count == self.oldest.len();
        self.directory = Some(directory.to_owned());
        self.modified = std::fs::metadata(directory)?.modified().ok();
        Ok(())
    }

    fn trim_candidates(&mut self) {
        self.oldest.sort_by_key(|file| file.modified);
        if self.oldest.len() > EVICTION_CANDIDATES {
            self.complete_window = false;
        }
        self.oldest.truncate(EVICTION_CANDIDATES);
    }

    fn record_publication(&mut self, current: &Path, previous: Option<u64>) -> std::io::Result<()> {
        let metadata = std::fs::symlink_metadata(current)?;
        if let Some(previous) = previous {
            self.bytes = self.bytes.saturating_sub(previous);
        } else {
            self.count = self.count.saturating_add(1);
        }
        self.bytes = self.bytes.saturating_add(metadata.len());
        self.oldest.retain(|file| file.path != current);
        let modified = metadata.modified().ok();
        // When older files were omitted from the bounded window, do not fill
        // its freed slots with newer publications ahead of those older files.
        // Refill from disk when the oldest window has actually been consumed.
        if self.complete_window
            || self
                .oldest
                .last()
                .is_some_and(|file| modified <= file.modified)
        {
            self.oldest.push(ThumbnailFile {
                path: current.to_owned(),
                modified,
                bytes: metadata.len(),
            });
        }
        self.trim_candidates();
        Ok(())
    }

    fn prune(
        &mut self,
        directory: &Path,
        current: &Path,
        disk_bytes: u64,
        cancellation: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<bool> {
        check_artwork_cancelled(cancellation)?;
        let mut blocked = Vec::new();
        let mut refreshed = false;
        let mut attempts = 0;
        while self.bytes > disk_bytes || self.count > MAX_FILES {
            check_artwork_cancelled(cancellation)?;
            if attempts == EVICTION_CANDIDATES {
                break;
            }
            let Some(index) = self.oldest.iter().position(|file| file.path != current) else {
                // Only a bounded oldest window is retained. Refill it after
                // those files have been evicted, instead of scanning on every
                // normal publication once the disk cache reaches its quota.
                if !refreshed && self.count > 1 {
                    self.scan(directory, cancellation)?;
                    self.oldest.retain(|file| {
                        !blocked
                            .iter()
                            .any(|blocked: &ThumbnailFile| blocked.path == file.path)
                    });
                    refreshed = true;
                    continue;
                }
                break;
            };
            let mut candidate = self.oldest.remove(index);
            attempts += 1;
            match std::fs::symlink_metadata(&candidate.path) {
                Ok(metadata) => {
                    self.bytes = self.bytes.saturating_sub(candidate.bytes);
                    candidate.bytes = metadata.len();
                    self.bytes = self.bytes.saturating_add(candidate.bytes);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.bytes = self.bytes.saturating_sub(candidate.bytes);
                    self.count = self.count.saturating_sub(1);
                    continue;
                }
                Err(error) => return Err(error),
            }
            if self.bytes <= disk_bytes && self.count <= MAX_FILES {
                blocked.push(candidate);
                break;
            }
            match std::fs::remove_file(&candidate.path) {
                Ok(()) => {
                    self.bytes = self.bytes.saturating_sub(candidate.bytes);
                    self.count = self.count.saturating_sub(1);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.bytes = self.bytes.saturating_sub(candidate.bytes);
                    self.count = self.count.saturating_sub(1);
                }
                // Protected files remain accounted and are retried on a later
                // publication, matching the existing Windows cleanup behavior.
                Err(_) => blocked.push(candidate),
            }
        }
        self.oldest.extend(blocked);
        self.trim_candidates();
        Ok(self.bytes <= disk_bytes && self.count <= MAX_FILES)
    }
}

fn check_artwork_cancelled(cancellation: &std::sync::atomic::AtomicBool) -> std::io::Result<()> {
    if cancellation.load(std::sync::atomic::Ordering::Acquire) {
        Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "server shutting down",
        ))
    } else {
        Ok(())
    }
}

fn publish_thumbnail(
    inventory: &mut ThumbnailInventory,
    directory: &Path,
    current: &Path,
    temporary: &Path,
    bytes: &[u8],
    disk_bytes: u64,
    cancellation: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let _cleanup = TemporaryFile(temporary.to_owned());
    let result = (|| {
        inventory
            .prepare(directory, cancellation)
            .map_err(|error| error.to_string())?;
        let previous = match std::fs::symlink_metadata(current) {
            Ok(metadata) => Some(metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        };
        if previous.is_some() {
            // A concurrent publisher may have created the target after the
            // fast-path existence check. Reconcile before accounting its
            // replacement, including platforms that retain that existing file.
            inventory
                .scan(directory, cancellation)
                .map_err(|error| error.to_string())?;
        }
        std::fs::write(temporary, bytes).map_err(|error| error.to_string())?;
        check_artwork_cancelled(cancellation).map_err(|error| error.to_string())?;
        match std::fs::rename(temporary, current) {
            Ok(()) => {}
            Err(_) if current.exists() => {}
            Err(error) => return Err(error.to_string()),
        }
        inventory
            .record_publication(current, previous)
            .map_err(|error| error.to_string())?;
        if !inventory
            .prune(directory, current, disk_bytes, cancellation)
            .map_err(|error| error.to_string())?
        {
            let _ = std::fs::remove_file(current);
            return Err("artwork disk capacity reached".into());
        }
        inventory.modified = std::fs::metadata(directory)
            .map_err(|error| error.to_string())?
            .modified()
            .ok();
        Ok(())
    })();
    if result.is_err() {
        inventory.invalidate();
    }
    result
}

/// Shared API/admin artwork rendering after their own authentication checks.
pub async fn artist_response(
    state: &crate::state::AppState,
    artist_id: &str,
    query: &crate::state::ArtistCoverQuery,
    source: crate::assets::CoverSource,
    headers: &HeaderMap,
) -> Response {
    if query
        .size
        .is_some_and(|size| !ArtworkCache::valid_size(size))
        || query.rendition.is_some_and(|version| version != 1)
        || query.v.as_ref().is_some_and(|value| value.len() > 4096)
        || query
            .kind
            .as_deref()
            .is_some_and(|kind| !matches!(kind, "logo" | "banner" | "cover"))
    {
        return json_error_response(StatusCode::BAD_REQUEST, "invalid artwork request");
    }
    let key = CoverCacheKey::Artist {
        id: artist_id.to_owned(),
        variant: query.kind.as_deref().unwrap_or("cover").to_string(),
    };
    let source_path = match &source {
        crate::assets::CoverSource::External(path) | crate::assets::CoverSource::File(path) => {
            Some(path.clone())
        }
        crate::assets::CoverSource::Embedded(_) => None,
    };
    let metadata_root = crate::assets::metadata_root_path(state);
    if let Some(version) = query.v.as_deref() {
        // Version parameters identify a specific source reference. Reject a
        // stale reference instead of caching replacement bytes under its URL.
        let current = source_path
            .as_ref()
            .and_then(|path| path.strip_prefix(&metadata_root).ok())
            .map(|path| path.to_string_lossy().replace('\\', "/"));
        if current.as_deref() != Some(version) {
            return json_error_response(
                StatusCode::CONFLICT,
                "artwork version changed; refresh artist metadata",
            );
        }
    }
    let fingerprint = match crate::assets::cover_source_fingerprint(&source).await {
        Ok(value) => value,
        Err(error) => return json_error_response(StatusCode::NOT_FOUND, error),
    };
    let mut immutable = false;
    let path = if let Some(size) = query.size {
        let Some(original) = source_path else {
            return json_error_response(StatusCode::BAD_REQUEST, "thumbnail source unavailable");
        };
        let hash = query.v.as_deref().and_then(|version| {
            let filename = version.rsplit('/').next()?;
            let mut parts = filename.split('.');
            parts.next()?;
            let hash = parts.next()?;
            (hash.len() == 64 && hash.bytes().all(|value| value.is_ascii_hexdigit()))
                .then_some(hash)
        });
        match state
            .artwork_cache
            .thumbnail(
                &state.shutdown,
                &metadata_root,
                &original,
                &fingerprint,
                size,
                hash,
            )
            .await
        {
            Ok(path) => {
                immutable = hash.is_some() && query.rendition == Some(1);
                path
            }
            Err(error) if error.contains("capacity reached") => {
                let mut response = json_error_response(StatusCode::SERVICE_UNAVAILABLE, error);
                response.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    axum::http::HeaderValue::from_static("2"),
                );
                return response;
            }
            Err(error) => return json_error_response(StatusCode::UNPROCESSABLE_ENTITY, error),
        }
    } else if let Some(path) = source_path {
        path
    } else {
        match crate::assets::ensure_cover_cached(state, &key, source.clone()).await {
            Ok(path) => path,
            Err(error) => return json_error_response(StatusCode::NOT_FOUND, error),
        }
    };
    if crate::assets::cover_source_fingerprint(&source)
        .await
        .ok()
        .as_deref()
        != Some(&fingerprint)
    {
        return json_error_response(StatusCode::CONFLICT, "artwork source changed while reading");
    }
    let etag = format!("\"{}-{}-v1\"", fingerprint, query.size.unwrap_or(0));
    crate::artwork_cache::file_response(&path, &etag, &headers, immutable).await
}

pub async fn file_response(
    path: &Path,
    etag: &str,
    headers: &HeaderMap,
    immutable: bool,
) -> Response {
    let cache_control = if immutable {
        "private, max-age=31536000, immutable"
    } else {
        "private, no-cache"
    };
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(_) => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::NOT_FOUND;
            return response;
        }
    };
    let mut response = if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|candidate| candidate.trim() == etag || candidate.trim() == "*")
        }) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        response
    } else {
        let length = file.metadata().await.ok().map(|metadata| metadata.len());
        let mut response = Response::new(Body::from_stream(ReaderStream::with_capacity(
            file,
            32 * 1024,
        )));
        if let Some(length) = length {
            if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
                response.headers_mut().insert(header::CONTENT_LENGTH, value);
            }
        }
        response
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    if let Ok(value) = HeaderValue::from_str(mime.as_ref()) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    if let Ok(value) = HeaderValue::from_str(etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response.extensions_mut().insert(AuthenticatedArtwork);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct InventoryDirectory(PathBuf);

    impl InventoryDirectory {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap()
                .join("target")
                .join(format!("thumbnail-inventory-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn publish(
            &self,
            inventory: &mut ThumbnailInventory,
            name: &str,
            bytes: usize,
            quota: u64,
        ) -> Result<PathBuf, String> {
            let current = self.0.join(format!("{name}.png"));
            let temporary = self.0.join(format!("{name}.part"));
            publish_thumbnail(
                inventory,
                &self.0,
                &current,
                &temporary,
                &vec![0; bytes],
                quota,
                &AtomicBool::new(false),
            )?;
            Ok(current)
        }

        fn actual_usage(&self) -> (usize, u64) {
            let mut count = 0;
            let mut bytes = 0;
            for entry in std::fs::read_dir(&self.0).unwrap() {
                let entry = entry.unwrap();
                if entry.path().extension().and_then(|value| value.to_str()) == Some("png") {
                    count += 1;
                    bytes += entry.metadata().unwrap().len();
                }
            }
            (count, bytes)
        }
    }

    impl Drop for InventoryDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn inventory_publications_below_quota_share_one_directory_scan() {
        let directory = InventoryDirectory::new();
        let mut inventory = ThumbnailInventory::default();
        for index in 0..40 {
            directory
                .publish(&mut inventory, &format!("image-{index}"), 4, 1024)
                .unwrap();
        }
        assert_eq!(inventory.scans, 1);
        assert_eq!(directory.actual_usage(), (inventory.count, inventory.bytes));
        assert_eq!(directory.actual_usage(), (40, 160));
        assert!(inventory.oldest.len() <= EVICTION_CANDIDATES);
    }

    #[test]
    fn inventory_at_quota_evicts_without_rescanning_every_publication() {
        let directory = InventoryDirectory::new();
        let mut inventory = ThumbnailInventory::default();
        for index in 0..100 {
            let published = directory
                .publish(&mut inventory, &format!("image-{index}"), 4, 32)
                .unwrap();
            assert!(published.is_file());
            let actual = directory.actual_usage();
            assert!(actual.1 <= 32);
            assert_eq!(actual, (inventory.count, inventory.bytes));
        }
        assert_eq!(inventory.scans, 1);
        assert_eq!(directory.actual_usage(), (8, 32));
    }

    #[test]
    fn inventory_refills_bounded_oldest_window_before_evicting_newer_files() {
        let directory = InventoryDirectory::new();
        for index in 0..200 {
            let path = directory.0.join(format!("old-{index}.png"));
            std::fs::write(&path, [0]).unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(index))
                .unwrap();
        }
        let mut inventory = ThumbnailInventory::default();
        for index in 0..200 {
            directory
                .publish(&mut inventory, &format!("new-{index}"), 1, 200)
                .unwrap();
            assert!(inventory.oldest.len() <= EVICTION_CANDIDATES);
            assert_eq!(directory.actual_usage(), (200, 200));
        }
        assert_eq!(inventory.scans, 2);
        for index in 0..200 {
            assert!(!directory.0.join(format!("old-{index}.png")).exists());
            assert!(directory.0.join(format!("new-{index}.png")).exists());
        }
    }

    #[test]
    fn inventory_reconciles_external_additions_deletions_and_directory_changes() {
        let directory = InventoryDirectory::new();
        let mut inventory = ThumbnailInventory::default();
        let first = directory.publish(&mut inventory, "first", 4, 32).unwrap();
        std::fs::write(directory.0.join("external.png"), [0; 8]).unwrap();
        directory.publish(&mut inventory, "next", 4, 32).unwrap();
        assert_eq!(inventory.scans, 2);
        assert_eq!(directory.actual_usage(), (inventory.count, inventory.bytes));
        std::fs::remove_file(first).unwrap();
        directory.publish(&mut inventory, "last", 4, 32).unwrap();
        assert_eq!(inventory.scans, 3);
        assert_eq!(directory.actual_usage(), (inventory.count, inventory.bytes));

        let replacement = InventoryDirectory::new();
        replacement.publish(&mut inventory, "other", 3, 32).unwrap();
        assert_eq!(inventory.scans, 4);
        assert_eq!(replacement.actual_usage(), (1, 3));
        assert_eq!((inventory.count, inventory.bytes), (1, 3));
    }

    #[test]
    fn inventory_accounts_replaced_files_and_recovers_after_capacity_failure() {
        let directory = InventoryDirectory::new();
        let mut inventory = ThumbnailInventory::default();
        let current = directory.publish(&mut inventory, "same", 4, 16).unwrap();
        directory.publish(&mut inventory, "same", 8, 16).unwrap();
        assert_eq!(std::fs::metadata(current).unwrap().len(), 8);
        assert_eq!(directory.actual_usage(), (1, 8));
        assert_eq!((inventory.count, inventory.bytes), (1, 8));
        let error = directory
            .publish(&mut inventory, "oversized", 17, 16)
            .unwrap_err();
        assert!(error.contains("capacity reached"));
        assert!(!directory.0.join("oversized.png").exists());
        assert!(inventory.directory.is_none());
        directory
            .publish(&mut inventory, "recovered", 2, 16)
            .unwrap();
        assert_eq!(directory.actual_usage(), (inventory.count, inventory.bytes));
        assert!(inventory.bytes <= 16);
    }

    #[cfg(windows)]
    #[test]
    fn inventory_rename_fallback_accounts_the_existing_protected_file() {
        use std::os::windows::fs::OpenOptionsExt;

        let directory = InventoryDirectory::new();
        let mut inventory = ThumbnailInventory::default();
        let current = directory
            .publish(&mut inventory, "protected", 4, 16)
            .unwrap();
        // Permit attribute/read access while preventing rename/delete, as a
        // simultaneous Windows reader may do during thumbnail publication.
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&current)
            .unwrap();
        directory
            .publish(&mut inventory, "protected", 32, 16)
            .unwrap();
        assert_eq!(std::fs::metadata(&current).unwrap().len(), 4);
        assert_eq!((inventory.count, inventory.bytes), (1, 4));
        assert!(!directory.0.join("protected.part").exists());
        drop(reader);
    }

    #[test]
    fn inventory_cancellation_does_not_publish_or_retain_partial_files() {
        let directory = InventoryDirectory::new();
        let mut inventory = ThumbnailInventory::default();
        let cancellation = AtomicBool::new(true);
        let current = directory.0.join("cancelled.png");
        let temporary = directory.0.join("cancelled.part");
        let error = publish_thumbnail(
            &mut inventory,
            &directory.0,
            &current,
            &temporary,
            &[1, 2, 3],
            16,
            &cancellation,
        )
        .unwrap_err();
        assert!(error.contains("shutting down"));
        assert!(!current.exists());
        assert!(!temporary.exists());
        assert!(inventory.directory.is_none());
        cancellation.store(false, Ordering::Release);
        directory.publish(&mut inventory, "retry", 3, 16).unwrap();
        assert_eq!(directory.actual_usage(), (1, 3));
    }

    #[test]
    fn only_fixed_variants_are_accepted() {
        for size in [128, 256, 512] {
            assert!(ArtworkCache::valid_size(size));
        }
        for size in [0, 127, 1024, u32::MAX] {
            assert!(!ArtworkCache::valid_size(size));
        }
    }

    #[tokio::test]
    async fn generation_failure_and_saturation_release_owned_budgets() {
        let root = std::env::temp_dir().join(format!(
            "phonolite-thumbnail-limits-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 4]))
            .save(&source)
            .unwrap();
        let limits = crate::resources::ResourceLimits {
            thumbnail_max_pixels: 1,
            ..Default::default()
        };
        let cache = ArtworkCache::new(&limits);
        let error = cache
            .thumbnail(
                &crate::shutdown::Shutdown::new(),
                &root,
                &source,
                "pixel-limit",
                128,
                None,
            )
            .await
            .unwrap_err();
        assert!(error.contains("pixel limit"));
        assert_eq!(cache.generation.available_permits(), 1);
        assert_eq!(cache.slots.available_permits(), 17);
        let slots = cache.slots.acquire_many(17).await.unwrap();
        let error = cache
            .thumbnail(
                &crate::shutdown::Shutdown::new(),
                &root,
                &source,
                "saturated",
                128,
                None,
            )
            .await
            .unwrap_err();
        assert!(error.contains("capacity reached"));
        drop(slots);
        assert!(!root.join("thumbnails").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn missing_file_never_returns_a_successful_conditional_hit() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"deleted\""),
        );
        let missing =
            std::env::temp_dir().join(format!("phonolite-missing-{}", uuid::Uuid::new_v4()));
        let response = file_response(&missing, "\"deleted\"", &headers, true).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response
            .extensions()
            .get::<AuthenticatedArtwork>()
            .is_none());
    }

    #[tokio::test]
    async fn rendition_preserves_shape_and_versioned_conditional_response() {
        let root =
            std::env::temp_dir().join(format!("phonolite-thumbnail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.png");
        image::RgbaImage::from_pixel(400, 200, image::Rgba([255, 0, 0, 0]))
            .save(&source)
            .unwrap();
        let hash = blake3::hash(&std::fs::read(&source).unwrap())
            .to_hex()
            .to_string();
        let cache = ArtworkCache::default();
        let path = cache
            .thumbnail(
                &crate::shutdown::Shutdown::new(),
                &root,
                &source,
                "fingerprint",
                128,
                Some(&hash),
            )
            .await
            .unwrap();
        assert_eq!(
            ImageReader::open(&path).unwrap().into_dimensions().unwrap(),
            (128, 64)
        );
        assert!(cache
            .thumbnail(
                &crate::shutdown::Shutdown::new(),
                &root,
                &source,
                "other",
                128,
                Some("wrong")
            )
            .await
            .is_err());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"version\""),
        );
        let response = file_response(&path, "\"version\"", &headers, true).await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, max-age=31536000, immutable"
        );
        assert!(response
            .extensions()
            .get::<AuthenticatedArtwork>()
            .is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}
