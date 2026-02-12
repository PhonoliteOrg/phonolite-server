use std::collections::HashSet;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::Response,
    Json,
};
use common::{Album, Artist, Track};
use serde::Serialize;

use crate::assets::{
    cover_response, fetch_cover, fetch_cover_cached, metadata_root_path,
    resolve_artist_banner_source, resolve_artist_cover_source, resolve_artist_logo_source,
    resolve_cover_source, CoverCacheKey,
};
use crate::shuffle::{build_shuffle_queue, ShuffleError, ShuffleMode};
use crate::state::{
    AppState, ArtistCoverQuery, JsonResult, Playlist, SearchQuery, SearchResult, ShuffleQuery,
};
use crate::utils::{json_error, json_error_response};

use super::{library_or_json_error, library_or_response};

const DEFAULT_SEARCH_LIMIT: usize = 40;
const MAX_SEARCH_LIMIT: usize = 100;

pub async fn get_artist_cover(
    State(state): State<AppState>,
    Query(query): Query<ArtistCoverQuery>,
    AxumPath(artist_id): AxumPath<String>,
) -> Response {
    let library = match library_or_response(&state) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let metadata_root = metadata_root_path(&state);
    let source = match query.kind.as_deref() {
        Some("logo") => resolve_artist_logo_source(&library, &metadata_root, &artist_id),
        Some("banner") => resolve_artist_banner_source(&library, &metadata_root, &artist_id),
        _ => resolve_artist_cover_source(&library, &metadata_root, &artist_id),
    };
    let source = match source {
        Ok(Some(source)) => source,
        Ok(None) => return json_error_response(StatusCode::NOT_FOUND, "cover not found"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            )
        }
    };

    match fetch_cover(source).await {
        Ok((bytes, mime)) => cover_response(bytes, &mime),
        Err(err) => json_error_response(StatusCode::NOT_FOUND, err),
    }
}

pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> JsonResult<Vec<SearchResult>> {
    let library = library_or_json_error(&state)?;
    let query = params.query.trim();
    if query.is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "query is required".to_string(),
        ));
    }
    let limit = params
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);

    let mut results: Vec<SearchResult> = Vec::new();
    let normalized = normalize_search(query);

    let artists = match fetch_artists_for_search(&library, query, limit) {
        Ok(items) => items,
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };
    for artist in artists {
        let score = score_match(&normalized, &artist.name);
        if score > 0 {
            results.push(SearchResult {
                kind: "artist".to_string(),
                id: artist.id,
                title: artist.name,
                subtitle: None,
                score,
            });
        }
    }

    let albums = match fetch_albums_for_search(&library, query, limit) {
        Ok(items) => items,
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };
    for album in albums {
        let artist_name = library
            .get_artist(&album.artist_id)
            .ok()
            .flatten()
            .map(|artist| artist.name)
            .unwrap_or_else(|| "Unknown Artist".to_string());
        let combined = format!("{} {}", album.title, artist_name);
        let score = score_match(&normalized, &combined);
        if score > 0 {
            results.push(SearchResult {
                kind: "album".to_string(),
                id: album.id,
                title: album.title,
                subtitle: Some(artist_name),
                score,
            });
        }
    }

    let tracks = match fetch_tracks_for_search(&library, query, limit) {
        Ok(items) => items,
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };
    for track in tracks {
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
        let combined = format!("{} {} {}", track.title, artist_name, album_title);
        let score = score_match(&normalized, &combined);
        if score > 0 {
            results.push(SearchResult {
                kind: "track".to_string(),
                id: track.id,
                title: track.title,
                subtitle: Some(format!("{} — {}", artist_name, album_title)),
                score,
            });
        }
    }

    results.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    results.truncate(limit);

    Ok(Json(results))
}

#[derive(Serialize, Clone)]
pub struct TrackView {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub artist_id: String,
    pub album_id: String,
    pub duration_ms: u32,
    pub track_no: Option<u16>,
    pub disc_no: Option<u16>,
    pub liked: bool,
    pub in_playlists: bool,
}

pub async fn shuffle_tracks(
    State(state): State<AppState>,
    Query(params): Query<ShuffleQuery>,
) -> JsonResult<Vec<TrackView>> {
    let library = library_or_json_error(&state)?;
    let mode = match ShuffleMode::parse(&params.mode) {
        Some(mode) => mode,
        None => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "invalid shuffle mode".to_string(),
            ))
        }
    };

    let custom_artist_ids = split_list_param(params.artist_ids.as_deref());
    let custom_genres = split_list_param(params.genres.as_deref());

    let tracks = match build_shuffle_queue(
        &library,
        mode,
        params.artist_id.as_deref(),
        params.album_id.as_deref(),
        &custom_artist_ids,
        &custom_genres,
    ) {
        Ok(tracks) => tracks,
        Err(ShuffleError::MissingArtistId) => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "artist_id required for shuffle=artist".to_string(),
            ))
        }
        Err(ShuffleError::MissingAlbumId) => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "album_id required for shuffle=album".to_string(),
            ))
        }
        Err(ShuffleError::Library(err)) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };

    if tracks.is_empty() {
        return Err(json_error(StatusCode::NOT_FOUND, "no tracks found"));
    }

    let liked_set = liked_set(&state)?;
    let playlist_set = playlist_set(&state)?;
    let mut items = Vec::with_capacity(tracks.len());
    for track in tracks {
        if let Ok(view) = build_track_view(&library, &track, &liked_set, &playlist_set) {
            items.push(view);
        }
    }

    Ok(Json(items))
}

fn split_list_param(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("")
        .split(',')
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .collect()
}

fn fetch_artists_for_search(
    library: &library::Library,
    query: &str,
    limit: usize,
) -> Result<Vec<Artist>, String> {
    let (items, _) = library
        .list_artists(Some(query), limit, 0)
        .map_err(|err| err.to_string())?;
    if !items.is_empty() {
        return Ok(items);
    }
    fetch_all_artists_for_fuzzy(library, limit)
}

fn fetch_all_artists_for_fuzzy(
    library: &library::Library,
    max_items: usize,
) -> Result<Vec<Artist>, String> {
    let mut items = Vec::new();
    let mut offset = 0usize;
    let limit = 200usize;
    while items.len() < max_items {
        let (mut batch, total) = library
            .list_artists(None, limit, offset)
            .map_err(|err| err.to_string())?;
        items.append(&mut batch);
        if items.len() >= total {
            break;
        }
        offset = items.len();
    }
    Ok(items)
}

fn fetch_albums_for_search(
    library: &library::Library,
    query: &str,
    limit: usize,
) -> Result<Vec<Album>, String> {
    let (items, _) = library
        .list_albums(Some(query), limit, 0)
        .map_err(|err| err.to_string())?;
    if !items.is_empty() {
        return Ok(items);
    }
    fetch_all_albums_for_fuzzy(library, limit)
}

fn fetch_all_albums_for_fuzzy(
    library: &library::Library,
    max_items: usize,
) -> Result<Vec<Album>, String> {
    let mut items = Vec::new();
    let mut offset = 0usize;
    let limit = 200usize;
    while items.len() < max_items {
        let (mut batch, total) = library
            .list_albums(None, limit, offset)
            .map_err(|err| err.to_string())?;
        items.append(&mut batch);
        if items.len() >= total {
            break;
        }
        offset = items.len();
    }
    Ok(items)
}

fn fetch_tracks_for_search(
    library: &library::Library,
    query: &str,
    limit: usize,
) -> Result<Vec<Track>, String> {
    let (items, _) = library
        .list_tracks(Some(query), limit, 0)
        .map_err(|err| err.to_string())?;
    if !items.is_empty() {
        return Ok(items);
    }
    fetch_all_tracks_for_fuzzy(library, limit)
}

fn fetch_all_tracks_for_fuzzy(
    library: &library::Library,
    max_items: usize,
) -> Result<Vec<Track>, String> {
    let mut items = Vec::new();
    let mut offset = 0usize;
    let limit = 200usize;
    while items.len() < max_items {
        let (mut batch, total) = library
            .list_tracks(None, limit, offset)
            .map_err(|err| err.to_string())?;
        items.append(&mut batch);
        if items.len() >= total {
            break;
        }
        offset = items.len();
    }
    Ok(items)
}

fn normalize_search(value: &str) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in value.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

fn score_match(query: &str, candidate: &str) -> u32 {
    if query.is_empty() {
        return 0;
    }
    let target = normalize_search(candidate);
    if target.is_empty() {
        return 0;
    }

    if target == query {
        return 100;
    }
    if target.starts_with(query) {
        return 90;
    }
    if target.contains(query) {
        return 80;
    }

    let query_tokens: Vec<&str> = query.split_whitespace().collect();
    if !query_tokens.is_empty()
        && query_tokens
            .iter()
            .all(|token| target.contains(token))
    {
        return 70;
    }

    if is_subsequence(query, &target) {
        return 60;
    }

    0
}

fn is_subsequence(query: &str, target: &str) -> bool {
    let mut q = query.chars().filter(|ch| !ch.is_whitespace());
    let mut current = q.next();
    for ch in target.chars().filter(|ch| !ch.is_whitespace()) {
        if let Some(needle) = current {
            if ch == needle {
                current = q.next();
                if current.is_none() {
                    return true;
                }
            }
        } else {
            return true;
        }
    }
    current.is_none()
}

fn liked_set(
    state: &AppState,
) -> Result<HashSet<String>, (StatusCode, Json<crate::state::ErrorResponse>)> {
    let liked_ids = state
        .user_data
        .list_likes()
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    Ok(liked_ids.into_iter().collect())
}

fn playlist_set(
    state: &AppState,
) -> Result<HashSet<String>, (StatusCode, Json<crate::state::ErrorResponse>)> {
    let playlists = state
        .user_data
        .list_playlists()
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    Ok(playlist_track_ids(&playlists))
}

fn playlist_track_ids(playlists: &[Playlist]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for playlist in playlists {
        for track_id in &playlist.track_ids {
            ids.insert(track_id.clone());
        }
    }
    ids
}

fn build_track_view(
    library: &library::Library,
    track: &common::Track,
    liked_set: &HashSet<String>,
    playlist_set: &HashSet<String>,
) -> Result<TrackView, (StatusCode, Json<crate::state::ErrorResponse>)> {
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
    Ok(TrackView {
        id: track.id.clone(),
        title: track.title.clone(),
        artist: artist_name,
        album: album_title,
        artist_id: track.artist_id.clone(),
        album_id: track.album_id.clone(),
        duration_ms: track.duration_ms,
        track_no: track.track_no,
        disc_no: track.disc_no,
        liked: liked_set.contains(&track.id),
        in_playlists: playlist_set.contains(&track.id),
    })
}

pub async fn get_album(
    State(state): State<AppState>,
    AxumPath(album_id): AxumPath<String>,
) -> JsonResult<Album> {
    let library = library_or_json_error(&state)?;
    match library.get_album(&album_id) {
        Ok(Some(album)) => Ok(Json(album)),
        Ok(None) => Err(json_error(StatusCode::NOT_FOUND, "album not found")),
        Err(err) => Err(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("library error: {}", err),
        )),
    }
}

pub async fn get_album_cover(
    State(state): State<AppState>,
    AxumPath(album_id): AxumPath<String>,
) -> Response {
    let library = match library_or_response(&state) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let album = match library.get_album(&album_id) {
        Ok(Some(album)) => album,
        Ok(None) => return json_error_response(StatusCode::NOT_FOUND, "album not found"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            )
        }
    };
    let cover_ref = match album.cover_ref {
        Some(cover) => cover,
        None => return json_error_response(StatusCode::NOT_FOUND, "cover not found"),
    };
    let source = match resolve_cover_source(&library, &cover_ref) {
        Ok(Some(source)) => source,
        Ok(None) => return json_error_response(StatusCode::NOT_FOUND, "cover not found"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            )
        }
    };

    let key = CoverCacheKey::Album(album_id.clone());
    match fetch_cover_cached(&state, key, source).await {
        Ok((bytes, mime)) => cover_response(bytes, &mime),
        Err(err) => json_error_response(StatusCode::NOT_FOUND, err),
    }
}

