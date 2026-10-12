use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{header, HeaderValue};
use axum::response::Response;
use common::CoverRef;
use library::Library;

use crate::config::resolve_path;
use crate::state::AppState;

#[derive(Debug, Clone)]
pub enum CoverSource {
    Embedded(PathBuf),
    File(PathBuf),
    External(PathBuf),
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum CoverCacheKey {
    Album(String),
    Track(String),
    Artist { id: String, variant: String },
}

#[derive(Debug, Clone, Default)]
pub struct MetadataAssetPruneStats {
    pub removed_files: usize,
    pub removed_dirs: usize,
}

pub fn metadata_root_path(state: &AppState) -> PathBuf {
    let config = state.config.read();
    resolve_path(&state.config_path, &config.metadata_path)
}

pub fn image_ext_from_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

pub fn image_ext_from_url(url: &str) -> Option<&'static str> {
    let path = Path::new(url);
    match path.extension().and_then(|e| e.to_str()) {
        Some("jpg") | Some("jpeg") => Some("jpg"),
        Some("png") => Some("png"),
        Some("gif") => Some("gif"),
        Some("webp") => Some("webp"),
        _ => None,
    }
}

pub fn resolve_cover_source(
    library: &Library,
    cover_ref: &CoverRef,
) -> Result<Option<CoverSource>, String> {
    match cover_ref {
        CoverRef::Embedded { track_id } => {
            let track = library
                .get_track(track_id)
                .map_err(|e| e.to_string())?
                .ok_or("track not found")?;
            let path = library
                .resolve_relpath(&track.file_relpath)
                .ok_or("music root not configured")?;
            Ok(Some(CoverSource::Embedded(path)))
        }
        CoverRef::File { relpath } => {
            let path = library
                .resolve_relpath(relpath)
                .ok_or("music root not configured")?;
            Ok(Some(CoverSource::File(path)))
        }
    }
}

pub fn resolve_artist_logo_source(
    library: &Library,
    metadata_root: &Path,
    artist_id: &str,
) -> Result<Option<CoverSource>, String> {
    if let Some(artist) = library.get_artist(artist_id).map_err(|e| e.to_string())? {
        if let Some(logo_ref) = &artist.logo_ref {
            return Ok(Some(CoverSource::External(metadata_root.join(logo_ref))));
        }
    }
    Ok(None)
}

pub fn resolve_artist_banner_source(
    library: &Library,
    metadata_root: &Path,
    artist_id: &str,
) -> Result<Option<CoverSource>, String> {
    if let Some(artist) = library.get_artist(artist_id).map_err(|e| e.to_string())? {
        if let Some(banner_ref) = &artist.banner_ref {
            return Ok(Some(CoverSource::External(metadata_root.join(banner_ref))));
        }
    }
    Ok(None)
}

pub fn resolve_artist_cover_source(
    library: &Library,
    metadata_root: &Path,
    artist_id: &str,
) -> Result<Option<CoverSource>, String> {
    // Try logo first, then first album cover
    if let Some(source) = resolve_artist_logo_source(library, metadata_root, artist_id)? {
        return Ok(Some(source));
    }

    let albums = library
        .list_artist_albums(artist_id)
        .map_err(|e| e.to_string())?;
    for album in albums {
        if let Some(cover_ref) = &album.cover_ref {
            return resolve_cover_source(library, cover_ref);
        }
    }

    Ok(None)
}

pub async fn warm_cover_cache(
    state: &AppState,
    key: &CoverCacheKey,
    source: CoverSource,
) -> Result<(), String> {
    ensure_cover_cached(state, key, source).await.map(|_| ())
}

async fn ensure_cover_cached(
    state: &AppState,
    key: &CoverCacheKey,
    source: CoverSource,
) -> Result<PathBuf, String> {
    let metadata_root = metadata_root_path(state);
    let cache_dir = metadata_root.join("covers");
    ensure_cover_cached_at(&cache_dir, key, source).await
}

async fn cover_source_fingerprint(source: &CoverSource) -> Result<String, String> {
    let (kind, path) = match source {
        CoverSource::Embedded(path) => ("embedded", path),
        CoverSource::File(path) => ("file", path),
        CoverSource::External(path) => ("external", path),
    };
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|err| format!("failed to read cover source metadata: {err}"))?;
    let modified = metadata
        .modified()
        .map_err(|err| format!("failed to read cover source modification time: {err}"))?;
    let bytes = serde_json::to_vec(&(kind, path, metadata.len(), modified))
        .map_err(|err| format!("failed to fingerprint cover source: {err}"))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

async fn ensure_cover_cached_at(
    cache_dir: &Path,
    key: &CoverCacheKey,
    source: CoverSource,
) -> Result<PathBuf, String> {
    tokio::fs::create_dir_all(cache_dir)
        .await
        .map_err(|err| format!("failed to create cover cache: {err}"))?;
    // The endpoint ID stays stable while the artwork source can change.
    // Include source kind, path, size and modification time in the disk key.
    // Separate generations also avoid concurrent requests overwriting one
    // another's cached source while files are being rescanned.
    let fingerprint = cover_source_fingerprint(&source).await?;

    let (id, prefix) = match key {
        CoverCacheKey::Album(id) => (id.clone(), "album".to_string()),
        CoverCacheKey::Track(id) => (id.clone(), "track".to_string()),
        CoverCacheKey::Artist { id, variant } => (id.clone(), format!("artist-{}", variant)),
    };

    for ext in ["jpg", "png", "webp", "gif"] {
        let filename = format!("{}-{}.{}.{}", prefix, id, fingerprint, ext);
        let path = cache_dir.join(filename);
        if path.exists() {
            return Ok(path);
        }
    }

    let (data, mime) = fetch_cover(source.clone()).await?;
    if cover_source_fingerprint(&source).await? != fingerprint {
        return Err("cover source changed while reading".to_owned());
    }

    let ext = image_ext_from_mime(&mime).unwrap_or("jpg");
    let filename = format!("{}-{}.{}.{}", prefix, id, fingerprint, ext);
    let path = cache_dir.join(filename);
    tokio::fs::write(&path, &data)
        .await
        .map_err(|err| format!("failed to store cached cover: {err}"))?;

    Ok(path)
}

pub async fn fetch_cover_cached(
    state: &AppState,
    key: CoverCacheKey,
    source: CoverSource,
) -> Result<(Vec<u8>, String), String> {
    let path = ensure_cover_cached(state, &key, source).await?;
    let data = tokio::fs::read(&path)
        .await
        .map_err(|_| "failed to read cached cover".to_string())?;
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    Ok((data, mime))
}

pub async fn fetch_cover(source: CoverSource) -> Result<(Vec<u8>, String), String> {
    match source {
        CoverSource::File(path) | CoverSource::External(path) => {
            let data = tokio::fs::read(&path)
                .await
                .map_err(|_| "failed to read cover file".to_string())?;
            let mime = mime_guess::from_path(&path)
                .first_or_octet_stream()
                .to_string();
            Ok((data, mime))
        }
        CoverSource::Embedded(path) => {
            let art = tokio::task::spawn_blocking(move || metadata::read_cover(&path))
                .await
                .map_err(|e| format!("embedded cover task failed: {}", e))?
                .map_err(|e| format!("failed to read embedded cover: {:?}", e))?;
            match art {
                Some(art) => Ok((
                    art.data,
                    art.mime
                        .unwrap_or_else(|| "application/octet-stream".to_string()),
                )),
                None => Err("no embedded cover found".to_string()),
            }
        }
    }
}

pub fn cover_response(data: Vec<u8>, mime: &str) -> Response {
    let mut response = Response::new(Body::from(data));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        // Cover URLs are stable, but their artwork and attribution can change.
        HeaderValue::from_static("private, no-cache"),
    );
    response
}

pub async fn clear_metadata_assets(state: &AppState) -> Result<(), String> {
    let root = metadata_root_path(state);
    if root.exists() {
        tokio::fs::remove_dir_all(&root)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub async fn prune_stale_metadata_assets(
    state: &AppState,
    library: &Library,
) -> Result<MetadataAssetPruneStats, String> {
    let root = metadata_root_path(state);
    if !root.exists() {
        return Ok(MetadataAssetPruneStats::default());
    }

    let artist_ids = load_artist_ids(library)?;
    let album_ids = load_album_ids(library)?;
    let track_ids = load_track_ids(library)?;

    let mut stats = MetadataAssetPruneStats::default();
    let metadata_root = &root;
    stats.removed_files += prune_cover_cache(
        &root.join("covers"),
        &artist_ids,
        &album_ids,
        &track_ids,
        |key| async move {
            let source = match key {
                CoverCacheKey::Album(id) => library
                    .get_album(&id)
                    .ok()
                    .flatten()
                    .and_then(|album| album.cover_ref)
                    .and_then(|cover| resolve_cover_source(library, &cover).ok().flatten()),
                CoverCacheKey::Track(id) => {
                    resolve_cover_source(library, &CoverRef::Embedded { track_id: id })
                        .ok()
                        .flatten()
                }
                CoverCacheKey::Artist { id, variant } => match variant.as_str() {
                    "logo" => resolve_artist_logo_source(library, metadata_root, &id),
                    "banner" => resolve_artist_banner_source(library, metadata_root, &id),
                    _ => resolve_artist_cover_source(library, metadata_root, &id),
                }
                .ok()
                .flatten(),
            }?;
            cover_source_fingerprint(&source).await.ok()
        },
    )
    .await?;
    stats.removed_files += prune_flat_artist_assets(&root.join("logos"), &artist_ids).await?;
    stats.removed_files += prune_flat_artist_assets(&root.join("banners"), &artist_ids).await?;
    stats.removed_dirs += prune_legacy_artist_assets(&root.join("artists"), &artist_ids).await?;
    Ok(stats)
}

fn load_artist_ids(library: &Library) -> Result<HashSet<String>, String> {
    let mut ids = HashSet::new();
    let mut cursor = None;
    let page_size = 500usize;
    loop {
        let (items, total) = library
            .page_artists(&mut cursor, page_size)
            .map_err(|e| e.to_string())?;
        for item in items {
            ids.insert(item.id);
        }
        if ids.len() >= total {
            break;
        }
    }
    Ok(ids)
}

fn load_album_ids(library: &Library) -> Result<HashSet<String>, String> {
    let mut ids = HashSet::new();
    let mut cursor = None;
    let page_size = 500usize;
    loop {
        let (items, total) = library
            .page_albums(&mut cursor, page_size)
            .map_err(|e| e.to_string())?;
        for item in items {
            ids.insert(item.id);
        }
        if ids.len() >= total {
            break;
        }
    }
    Ok(ids)
}

fn load_track_ids(library: &Library) -> Result<HashSet<String>, String> {
    let mut ids = HashSet::new();
    let mut cursor = None;
    let page_size = 1000usize;
    loop {
        let (items, total) = library
            .page_tracks(&mut cursor, page_size)
            .map_err(|e| e.to_string())?;
        for item in items {
            ids.insert(item.id);
        }
        if ids.len() >= total {
            break;
        }
    }
    Ok(ids)
}

async fn prune_cover_cache<F, Fut>(
    dir: &Path,
    artist_ids: &HashSet<String>,
    album_ids: &HashSet<String>,
    track_ids: &HashSet<String>,
    mut fingerprint: F,
) -> Result<usize, String>
where
    F: FnMut(CoverCacheKey) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    let mut removed = 0usize;
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err.to_string()),
    };
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        // New cover cache names carry a source fingerprint after the ID.
        let (identity, generation) = stem
            .split_once('.')
            .map_or((stem, None), |(id, generation)| (id, Some(generation)));
        let (key, live) = if let Some(id) = identity.strip_prefix("album-") {
            (CoverCacheKey::Album(id.to_owned()), album_ids.contains(id))
        } else if let Some(id) = identity.strip_prefix("track-") {
            (CoverCacheKey::Track(id.to_owned()), track_ids.contains(id))
        } else if let Some(id) = identity.strip_prefix("artist-logo-") {
            (
                CoverCacheKey::Artist {
                    id: id.to_owned(),
                    variant: "logo".to_owned(),
                },
                artist_ids.contains(id),
            )
        } else if let Some(id) = identity.strip_prefix("artist-banner-") {
            (
                CoverCacheKey::Artist {
                    id: id.to_owned(),
                    variant: "banner".to_owned(),
                },
                artist_ids.contains(id),
            )
        } else if let Some(id) = identity.strip_prefix("artist-cover-") {
            (
                CoverCacheKey::Artist {
                    id: id.to_owned(),
                    variant: "cover".to_owned(),
                },
                artist_ids.contains(id),
            )
        } else {
            continue;
        };
        let stale = !live
            || match generation {
                Some(generation) => fingerprint(key).await.as_deref() != Some(generation),
                None => true, // Legacy cache bytes do not identify their source.
            };
        if stale {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| e.to_string())?;
            removed += 1;
        }
    }
    Ok(removed)
}

async fn prune_flat_artist_assets(
    dir: &Path,
    artist_ids: &HashSet<String>,
) -> Result<usize, String> {
    let mut removed = 0usize;
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err.to_string()),
    };
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        // Versioned image names and URL source records start with the artist ID.
        let artist_id = stem.split('.').next().unwrap_or(stem);
        if !artist_ids.contains(artist_id) {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| e.to_string())?;
            removed += 1;
        }
    }
    Ok(removed)
}

async fn prune_legacy_artist_assets(
    dir: &Path,
    artist_ids: &HashSet<String>,
) -> Result<usize, String> {
    let mut removed = 0usize;
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err.to_string()),
    };
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !artist_ids.contains(name) {
            tokio::fs::remove_dir_all(&path)
                .await
                .map_err(|e| e.to_string())?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("phonolite-artwork-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn cover_cache_reuses_unchanged_sources_and_refreshes_changed_files_and_refs() {
        let dir = TestDirectory::new();
        let source = dir.0.join("cover.jpg");
        let cache = dir.0.join("covers");
        tokio::fs::write(&source, b"original artwork")
            .await
            .unwrap();
        let key = CoverCacheKey::Album("album".to_owned());
        let first = ensure_cover_cached_at(&cache, &key, CoverSource::File(source.clone()))
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&first).await.unwrap(), b"original artwork");

        // Alter only the cached bytes to prove an unchanged source takes the
        // cache path, rather than performing another extraction or copy.
        tokio::fs::write(&first, b"cached generation")
            .await
            .unwrap();
        let reused = ensure_cover_cached_at(&cache, &key, CoverSource::File(source.clone()))
            .await
            .unwrap();
        assert_eq!(reused, first);
        assert_eq!(
            tokio::fs::read(&reused).await.unwrap(),
            b"cached generation"
        );

        let old_modified = std::fs::metadata(&source).unwrap().modified().unwrap();
        tokio::fs::write(&source, b"updated artwork!")
            .await
            .unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(old_modified + std::time::Duration::from_secs(1))
            .unwrap();
        let refreshed = ensure_cover_cached_at(&cache, &key, CoverSource::File(source.clone()))
            .await
            .unwrap();
        assert_ne!(refreshed, first);
        assert_eq!(
            tokio::fs::read(&refreshed).await.unwrap(),
            b"updated artwork!"
        );

        let replacement = dir.0.join("other-cover.jpg");
        tokio::fs::write(&replacement, b"different referenced artwork")
            .await
            .unwrap();
        let changed_ref = ensure_cover_cached_at(&cache, &key, CoverSource::File(replacement))
            .await
            .unwrap();
        assert_ne!(changed_ref, refreshed);
        assert_eq!(
            tokio::fs::read(&changed_ref).await.unwrap(),
            b"different referenced artwork"
        );

        tokio::fs::remove_file(&source).await.unwrap();
        assert!(
            ensure_cover_cached_at(&cache, &key, CoverSource::File(source))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unverified_legacy_cover_cache_is_rebuilt_from_current_source() {
        let dir = TestDirectory::new();
        let source = dir.0.join("correct.jpg");
        let cache = dir.0.join("covers");
        tokio::fs::create_dir_all(&cache).await.unwrap();
        tokio::fs::write(cache.join("artist-logo-artist.jpg"), b"wrong old artist")
            .await
            .unwrap();
        tokio::fs::write(&source, b"verified artist artwork")
            .await
            .unwrap();
        let path = ensure_cover_cached_at(
            &cache,
            &CoverCacheKey::Artist {
                id: "artist".to_owned(),
                variant: "logo".to_owned(),
            },
            CoverSource::External(source),
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read(path).await.unwrap(),
            b"verified artist artwork"
        );
    }

    #[tokio::test]
    async fn pruning_recognizes_versioned_artwork_and_source_records() {
        let dir = TestDirectory::new();
        for name in [
            "album-live.fingerprint.jpg",
            "album-live.oldfingerprint.jpg",
            "album-live.jpg",
            "album-removed.fingerprint.jpg",
            "artist-cover-removed.fingerprint.png",
        ] {
            tokio::fs::write(dir.0.join(name), b"image").await.unwrap();
        }
        let albums = HashSet::from(["live".to_owned()]);
        assert_eq!(
            prune_cover_cache(
                &dir.0,
                &HashSet::new(),
                &albums,
                &HashSet::new(),
                |_| async { Some("fingerprint".to_owned()) }
            )
            .await
            .unwrap(),
            4
        );
        assert!(dir.0.join("album-live.fingerprint.jpg").exists());

        let flat = dir.0.join("logos");
        tokio::fs::create_dir_all(&flat).await.unwrap();
        for name in [
            "live.content.jpg",
            "live.source",
            "removed.content.jpg",
            "removed.source",
        ] {
            tokio::fs::write(flat.join(name), b"asset").await.unwrap();
        }
        assert_eq!(
            prune_flat_artist_assets(&flat, &HashSet::from(["live".to_owned()]))
                .await
                .unwrap(),
            2
        );
        assert!(flat.join("live.content.jpg").exists());
        assert!(flat.join("live.source").exists());
    }

    #[test]
    fn stable_cover_urls_require_http_cache_revalidation() {
        let response = cover_response(vec![1, 2, 3], "image/png");
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "private, no-cache"
        );
    }
}
