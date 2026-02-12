use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{header, HeaderValue};
use axum::response::Response;
use common::{join_relpath, CoverRef};
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
            let path = join_relpath(library.root(), &track.file_relpath);
            Ok(Some(CoverSource::Embedded(path)))
        }
        CoverRef::File { relpath } => {
            let path = join_relpath(library.root(), relpath);
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
    if !cache_dir.exists() {
        let _ = tokio::fs::create_dir_all(&cache_dir).await;
    }

    let (id, prefix) = match key {
        CoverCacheKey::Album(id) => (id, "album"),
        CoverCacheKey::Track(id) => (id, "track"),
    };

    for ext in ["jpg", "png", "webp", "gif"] {
        let filename = format!("{}-{}.{}", prefix, id, ext);
        let path = cache_dir.join(filename);
        if path.exists() {
            return Ok(path);
        }
    }

    let (data, mime) = fetch_cover(source).await?;

    let ext = image_ext_from_mime(&mime).unwrap_or("jpg");
    let filename = format!("{}-{}.{}", prefix, id, ext);
    let path = cache_dir.join(filename);
    let _ = tokio::fs::write(&path, &data).await;

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
            match metadata::read_cover(&path)
                .map_err(|e| format!("failed to read embedded cover: {:?}", e))?
            {
                Some(art) => Ok((art.data, art.mime.unwrap_or_else(|| "application/octet-stream".to_string()))),
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
        HeaderValue::from_static("public, max-age=31536000"),
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
