use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::path::Path as FsPath;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    Json,
};
use common::{Album, Artist, Track};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use strsim::normalized_levenshtein;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::broadcast;
use tokio_util::io::ReaderStream;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};
use uuid::Uuid;

use crate::assets::{
    cover_response, fetch_cover_cached, metadata_root_path, resolve_artist_banner_source,
    resolve_artist_cover_source, resolve_artist_logo_source, resolve_cover_source, CoverCacheKey,
};
use crate::range::{parse_range_header, ByteRange, RangeError};
use crate::shuffle::{build_shuffle_queue, ShuffleError, ShuffleMode};
use crate::state::{
    AppState, ArtistCoverQuery, JsonResult, Playlist, SearchQuery, SearchResult, ShuffleQuery,
};
use crate::utils::{json_error, json_error_response};

use super::{
    library_or_json_error, library_or_response,
    views::{
        album_artist_name, build_album_view, build_artist_view, build_track_views, AlbumView,
        ArtistView, TrackView,
    },
};

const DEFAULT_SEARCH_LIMIT: usize = 40;
const MAX_SEARCH_LIMIT: usize = 100;
const OFFLINE_METADATA_SCHEMA_VERSION: u32 = 4;

static DOWNLOAD_CHECKSUM_CACHE: OnceLock<Mutex<HashMap<String, CachedChecksum>>> = OnceLock::new();

#[derive(Clone)]
struct CachedChecksum {
    modified_ns: u128,
    byte_length: u64,
    sha256: String,
}

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

    let key = CoverCacheKey::Artist {
        id: artist_id.clone(),
        variant: query.kind.as_deref().unwrap_or("cover").to_string(),
    };
    match fetch_cover_cached(&state, key, source).await {
        Ok((bytes, mime)) => cover_response(bytes, &mime),
        Err(err) => json_error_response(StatusCode::NOT_FOUND, err),
    }
}

pub async fn download_track(
    State(state): State<AppState>,
    AxumPath(track_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let library = match library_or_response(&state) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let track = match library.get_track(&track_id) {
        Ok(Some(track)) => track,
        Ok(None) => return json_error_response(StatusCode::NOT_FOUND, "track not found"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            )
        }
    };
    let path = match library.resolve_relpath(&track.file_relpath) {
        Some(path) => path,
        None => {
            return json_error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "music root not configured",
            )
        }
    };
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return json_error_response(StatusCode::NOT_FOUND, "track file not found"),
        Err(_) => return json_error_response(StatusCode::NOT_FOUND, "track file not found"),
    };
    let size = metadata.len();
    let etag = download_etag(&track, size);
    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let if_range_matches = headers
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

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => return json_error_response(StatusCode::NOT_FOUND, "track file not found"),
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
        HeaderValue::from_str(&download_content_type(&path))
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
            .unwrap_or_else(|_| HeaderValue::from_static("\"phonolite-download\"")),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&download_content_disposition(&track, &path))
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
    let album_artist_ids = albums
        .iter()
        .map(|album| album.artist_id.clone())
        .collect::<HashSet<_>>();
    let album_artist_names = library.artist_name_map(&album_artist_ids).map_err(|err| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("library error: {}", err),
        )
    })?;
    for album in albums {
        let artist_name =
            album_artist_name(&album, album_artist_names.get(&album.artist_id).cloned());
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
    let track_artist_ids = tracks
        .iter()
        .map(|track| track.artist_id.clone())
        .collect::<HashSet<_>>();
    let track_album_ids = tracks
        .iter()
        .map(|track| track.album_id.clone())
        .collect::<HashSet<_>>();
    let track_artist_names = library.artist_name_map(&track_artist_ids).map_err(|err| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("library error: {}", err),
        )
    })?;
    let track_album_titles = library.album_title_map(&track_album_ids).map_err(|err| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("library error: {}", err),
        )
    })?;
    for track in tracks {
        let artist_name = track_artist_names
            .get(&track.artist_id)
            .cloned()
            .unwrap_or_else(|| "Unknown Artist".to_string());
        let album_title = track_album_titles
            .get(&track.album_id)
            .cloned()
            .unwrap_or_else(|| "Unknown Album".to_string());
        let combined = format!("{} {} {}", track.title, artist_name, album_title);
        let score = score_match(&normalized, &combined);
        if score > 0 {
            results.push(SearchResult {
                kind: "track".to_string(),
                id: track.id,
                title: track.title,
                subtitle: Some(format!("{} - {}", artist_name, album_title)),
                score,
            });
        }
    }

    results.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    results.truncate(limit);

    Ok(Json(results))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OfflineTrackMetadata {
    pub schema_version: u32,
    pub track: TrackView,
    pub album: AlbumView,
    pub artist: ArtistView,
}

#[derive(Deserialize)]
pub struct DownloadBatchRequest {
    pub track_ids: Vec<String>,
    pub client_batch_id: Option<String>,
}

#[derive(Serialize)]
pub struct DownloadBatchResponse {
    pub schema_version: u32,
    pub batch_id: String,
    pub created_at: u64,
    pub items: Vec<DownloadBatchItem>,
    pub unavailable: Vec<DownloadBatchUnavailable>,
}

#[derive(Serialize)]
pub struct DownloadBatchItem {
    pub track_id: String,
    pub download_url: String,
    pub offline_metadata: OfflineTrackMetadata,
    pub byte_length: u64,
    pub content_type: String,
    pub etag: String,
    pub sha256: String,
}

#[derive(Serialize)]
pub struct DownloadBatchUnavailable {
    pub track_id: String,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct TrackMatchRequest {
    #[serde(default)]
    pub tracks: Vec<TrackMatchDescriptor>,
}

#[derive(Debug, Deserialize)]
pub struct TrackMatchDescriptor {
    pub local_track_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u32,
    pub track_no: Option<u16>,
    pub disc_no: Option<u16>,
    pub server_track_id: Option<String>,
}

#[derive(Serialize)]
pub struct TrackMatchResponse {
    pub matches: Vec<TrackMatchResult>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TrackMatchResult {
    pub local_track_id: String,
    pub server_track_id: String,
    pub confidence: f64,
    pub match_kind: String,
    pub server_liked: bool,
    pub server_updated_at: u64,
}

pub async fn create_download_batch(
    State(state): State<AppState>,
    Json(request): Json<DownloadBatchRequest>,
) -> JsonResult<DownloadBatchResponse> {
    let library = library_or_json_error(&state)?;
    let liked_set = liked_set(&state)?;
    let playlist_set = playlist_set(&state)?;

    let mut seen = HashSet::new();
    let track_ids = request
        .track_ids
        .into_iter()
        .map(|track_id| track_id.trim().to_string())
        .filter(|track_id| !track_id.is_empty())
        .filter(|track_id| seen.insert(track_id.clone()))
        .collect::<Vec<_>>();

    if track_ids.is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "track_ids must contain at least one track".to_string(),
        ));
    }

    let mut items = Vec::new();
    let mut unavailable = Vec::new();
    for track_id in track_ids {
        let track = match library.get_track(&track_id) {
            Ok(Some(track)) => track,
            Ok(None) => {
                unavailable.push(DownloadBatchUnavailable {
                    track_id,
                    reason: "track not found".to_string(),
                });
                continue;
            }
            Err(err) => {
                unavailable.push(DownloadBatchUnavailable {
                    track_id,
                    reason: format!("library error: {}", err),
                });
                continue;
            }
        };
        let path = match library.resolve_relpath(&track.file_relpath) {
            Some(path) => path,
            None => {
                unavailable.push(DownloadBatchUnavailable {
                    track_id: track.id,
                    reason: "music root not configured".to_string(),
                });
                continue;
            }
        };
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => {
                unavailable.push(DownloadBatchUnavailable {
                    track_id: track.id,
                    reason: "track file not found".to_string(),
                });
                continue;
            }
        };
        let offline_metadata =
            match offline_metadata_for_track(&library, &track, &liked_set, &playlist_set) {
                Ok(metadata) => metadata,
                Err(reason) => {
                    unavailable.push(DownloadBatchUnavailable {
                        track_id: track.id,
                        reason,
                    });
                    continue;
                }
            };
        let sha256 = match checksum_for_file(&path, &metadata).await {
            Ok(sha256) => sha256,
            Err(err) => {
                unavailable.push(DownloadBatchUnavailable {
                    track_id: track.id,
                    reason: format!("checksum failed: {}", err),
                });
                continue;
            }
        };
        let byte_length = metadata.len();
        items.push(DownloadBatchItem {
            track_id: track.id.clone(),
            download_url: format!(
                "/api/v1/download/tracks/{}",
                percent_encode_path_component(&track.id)
            ),
            offline_metadata,
            byte_length,
            content_type: download_content_type(&path),
            etag: download_etag(&track, byte_length),
            sha256,
        });
    }

    let batch_id = request
        .client_batch_id
        .and_then(|value| {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    Ok(Json(DownloadBatchResponse {
        schema_version: OFFLINE_METADATA_SCHEMA_VERSION,
        batch_id,
        created_at: unix_timestamp_ms(),
        items,
        unavailable,
    }))
}

pub async fn match_tracks(
    State(state): State<AppState>,
    Json(request): Json<TrackMatchRequest>,
) -> JsonResult<TrackMatchResponse> {
    if request.tracks.is_empty() {
        return Ok(Json(TrackMatchResponse {
            matches: Vec::new(),
        }));
    }

    let library = library_or_json_error(&state)?;
    let (all_tracks, _) = library
        .list_tracks(None, usize::MAX, 0)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let candidates = build_match_candidates(&library, &all_tracks)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    let by_id = candidates
        .iter()
        .map(|candidate| (candidate.track.id.as_str(), candidate))
        .collect::<HashMap<_, _>>();
    let like_states = state
        .user_data
        .list_like_states()
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?
        .into_iter()
        .collect::<HashMap<_, _>>();

    let mut matches = Vec::new();
    for descriptor in request.tracks {
        let local_track_id = descriptor.local_track_id.trim();
        if local_track_id.is_empty() {
            continue;
        }

        let matched = descriptor
            .server_track_id
            .as_deref()
            .map(str::trim)
            .filter(|track_id| !track_id.is_empty())
            .and_then(|track_id| {
                by_id.get(track_id).map(|candidate| CandidateMatch {
                    track_id: candidate.track.id.clone(),
                    score: 1.0,
                    kind: "exact_server_track_id",
                })
            })
            .or_else(|| best_metadata_match(&descriptor, &candidates));

        let Some(matched) = matched else {
            continue;
        };
        let like_state = like_states
            .get(&matched.track_id)
            .cloned()
            .unwrap_or_else(|| crate::user_data::LikeState {
                liked: false,
                updated_at: 0,
            });
        matches.push(TrackMatchResult {
            local_track_id: local_track_id.to_string(),
            server_track_id: matched.track_id,
            confidence: matched.score,
            match_kind: matched.kind.to_string(),
            server_liked: like_state.liked,
            server_updated_at: like_state.updated_at,
        });
    }

    Ok(Json(TrackMatchResponse { matches }))
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

    let liked_set = liked_set(&state)?;
    let tracks = match build_shuffle_queue(
        &library,
        mode,
        params.artist_id.as_deref(),
        params.album_id.as_deref(),
        &custom_artist_ids,
        &custom_genres,
        &liked_set,
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

    let playlist_set = playlist_set(&state)?;
    let items = build_track_views(&library, &tracks, &liked_set, &playlist_set)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(Json(items))
}

pub async fn metadata_events(
    State(state): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let receiver = state.metadata_events.subscribe();
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let data = match serde_json::to_string(&event) {
                        Ok(data) => data,
                        Err(err) => {
                            tracing::warn!("Failed to serialize metadata event: {}", err);
                            continue;
                        }
                    };
                    let sse = Event::default()
                        .event("metadata")
                        .id(event.revision.to_string())
                        .data(data);
                    return Some((Ok(sse), receiver));
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!("Metadata event stream lagged by {} event(s)", skipped);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn fetch_artists_for_search(
    library: &library::Library,
    query: &str,
    limit: usize,
) -> Result<Vec<Artist>, String> {
    library
        .list_artists(Some(query), limit, 0)
        .map(|(items, _)| items)
        .map_err(|err| err.to_string())
}

fn fetch_albums_for_search(
    library: &library::Library,
    query: &str,
    limit: usize,
) -> Result<Vec<Album>, String> {
    library
        .list_albums(Some(query), limit, 0)
        .map(|(items, _)| items)
        .map_err(|err| err.to_string())
}

fn fetch_tracks_for_search(
    library: &library::Library,
    query: &str,
    limit: usize,
) -> Result<Vec<Track>, String> {
    library
        .list_tracks(Some(query), limit, 0)
        .map(|(items, _)| items)
        .map_err(|err| err.to_string())
}

#[derive(Clone)]
struct TrackMatchCandidate {
    track: Track,
    title_norm: String,
    artist_norm: String,
    album_norm: String,
}

struct CandidateMatch {
    track_id: String,
    score: f64,
    kind: &'static str,
}

fn build_match_candidates(
    library: &library::Library,
    tracks: &[Track],
) -> Result<Vec<TrackMatchCandidate>, String> {
    let artist_ids = tracks
        .iter()
        .map(|track| track.artist_id.clone())
        .collect::<HashSet<_>>();
    let album_ids = tracks
        .iter()
        .map(|track| track.album_id.clone())
        .collect::<HashSet<_>>();
    let artist_names = library
        .artist_name_map(&artist_ids)
        .map_err(|err| err.to_string())?;
    let album_titles = library
        .album_title_map(&album_ids)
        .map_err(|err| err.to_string())?;

    Ok(tracks
        .iter()
        .map(|track| {
            let artist = artist_names
                .get(&track.artist_id)
                .cloned()
                .unwrap_or_else(|| "Unknown Artist".to_string());
            let album = album_titles
                .get(&track.album_id)
                .cloned()
                .unwrap_or_else(|| "Unknown Album".to_string());
            TrackMatchCandidate {
                track: track.clone(),
                title_norm: normalize_match_text(&track.title),
                artist_norm: normalize_match_text(&artist),
                album_norm: normalize_match_text(&album),
            }
        })
        .collect())
}

fn best_metadata_match(
    descriptor: &TrackMatchDescriptor,
    candidates: &[TrackMatchCandidate],
) -> Option<CandidateMatch> {
    let title_norm = normalize_match_text(&descriptor.title);
    let artist_norm = normalize_match_text(&descriptor.artist);
    let album_norm = normalize_match_text(&descriptor.album);
    if title_norm.is_empty() || artist_norm.is_empty() {
        return None;
    }

    let mut best: Option<CandidateMatch> = None;
    let mut second_score: f64 = 0.0;
    for candidate in candidates {
        let duration_delta = duration_delta_ms(descriptor.duration_ms, candidate.track.duration_ms);
        if duration_delta > 3000 {
            continue;
        }
        let title_score = normalized_ratio(&title_norm, &candidate.title_norm);
        let artist_score = normalized_ratio(&artist_norm, &candidate.artist_norm);
        if title_score < 0.86 || artist_score < 0.82 {
            continue;
        }
        let album_score = normalized_ratio(&album_norm, &candidate.album_norm);
        let duration_score = 1.0 - (duration_delta as f64 / 3000.0);
        let number_score = track_number_score(descriptor, &candidate.track);
        let score = title_score * 0.45
            + artist_score * 0.30
            + duration_score * 0.15
            + album_score * 0.07
            + number_score * 0.03;

        if best.as_ref().map_or(true, |current| score > current.score) {
            if let Some(current) = best.replace(CandidateMatch {
                track_id: candidate.track.id.clone(),
                score,
                kind: "metadata_strict",
            }) {
                second_score = second_score.max(current.score);
            }
        } else {
            second_score = second_score.max(score);
        }
    }

    let best = best?;
    if best.score >= 0.94 && best.score - second_score >= 0.05 {
        Some(best)
    } else {
        None
    }
}

fn duration_delta_ms(left: u32, right: u32) -> u32 {
    left.max(right) - left.min(right)
}

fn track_number_score(descriptor: &TrackMatchDescriptor, track: &Track) -> f64 {
    let track_score = optional_number_score(descriptor.track_no, track.track_no);
    let disc_score = optional_number_score(descriptor.disc_no, track.disc_no);
    (track_score + disc_score) / 2.0
}

fn optional_number_score(left: Option<u16>, right: Option<u16>) -> f64 {
    match (left, right) {
        (Some(left), Some(right)) if left == right => 1.0,
        (Some(_), Some(_)) => 0.0,
        _ => 0.5,
    }
}

fn normalized_ratio(left: &str, right: &str) -> f64 {
    if left.is_empty() && right.is_empty() {
        return 1.0;
    }
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    normalized_levenshtein(left, right)
}

fn normalize_match_text(value: &str) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in value.nfkd().flat_map(char::to_lowercase) {
        if is_combining_mark(ch) {
            continue;
        }
        let ch = normalize_match_dash(ch);
        if ch.is_alphanumeric() {
            out.push(ch);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

fn normalize_match_dash(ch: char) -> char {
    match ch {
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
        _ => ch,
    }
}

fn split_list_param(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .collect()
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
    if !query_tokens.is_empty() && query_tokens.iter().all(|token| target.contains(token)) {
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

pub(crate) fn liked_set(
    state: &AppState,
) -> Result<HashSet<String>, (StatusCode, Json<crate::state::ErrorResponse>)> {
    let liked_ids = state
        .user_data
        .list_likes()
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    Ok(liked_ids.into_iter().collect())
}

pub(crate) fn playlist_set(
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

pub async fn get_album(
    State(state): State<AppState>,
    AxumPath(album_id): AxumPath<String>,
) -> JsonResult<AlbumView> {
    let library = library_or_json_error(&state)?;
    match library.get_album(&album_id) {
        Ok(Some(album)) => build_album_view(&library, album)
            .map(Json)
            .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err)),
        Ok(None) => Err(json_error(StatusCode::NOT_FOUND, "album not found")),
        Err(err) => Err(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("library error: {}", err),
        )),
    }
}

pub async fn get_offline_track_metadata(
    State(state): State<AppState>,
    AxumPath(track_id): AxumPath<String>,
) -> JsonResult<OfflineTrackMetadata> {
    let library = library_or_json_error(&state)?;
    let track = match library.get_track(&track_id) {
        Ok(Some(track)) => track,
        Ok(None) => return Err(json_error(StatusCode::NOT_FOUND, "track not found")),
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };
    let liked_set = liked_set(&state)?;
    let playlist_set = playlist_set(&state)?;
    offline_metadata_for_track(&library, &track, &liked_set, &playlist_set)
        .map(Json)
        .map_err(|err| {
            let status = if err.ends_with("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            json_error(status, err)
        })
}

pub(crate) fn offline_metadata_for_track(
    library: &library::Library,
    track: &Track,
    liked_set: &HashSet<String>,
    playlist_set: &HashSet<String>,
) -> Result<OfflineTrackMetadata, String> {
    let album = library
        .get_album(&track.album_id)
        .map_err(|err| format!("library error: {}", err))?
        .ok_or_else(|| "album not found".to_string())?;
    let artist = library
        .get_artist(&track.artist_id)
        .map_err(|err| format!("library error: {}", err))?
        .ok_or_else(|| "artist not found".to_string())?;
    let mut tracks = build_track_views(
        library,
        std::slice::from_ref(track),
        liked_set,
        playlist_set,
    )?;
    let track = tracks.pop().ok_or_else(|| "track not found".to_string())?;
    let album = build_album_view(library, album)?;
    let artist = build_artist_view(library, artist)?;
    Ok(OfflineTrackMetadata {
        schema_version: OFFLINE_METADATA_SCHEMA_VERSION,
        track,
        album,
        artist,
    })
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

pub(crate) fn unix_timestamp_ms() -> u64 {
    UNIX_EPOCH
        .elapsed()
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
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

    fn descriptor() -> TrackMatchDescriptor {
        TrackMatchDescriptor {
            local_track_id: "local-1".to_string(),
            title: "Break My Soul".to_string(),
            artist: "Beyonce".to_string(),
            album: "Renaissance".to_string(),
            duration_ms: 278_000,
            track_no: Some(6),
            disc_no: Some(1),
            server_track_id: None,
        }
    }

    fn candidate(
        id: &str,
        title: &str,
        artist: &str,
        album: &str,
        duration_ms: u32,
    ) -> TrackMatchCandidate {
        TrackMatchCandidate {
            track: Track {
                id: id.to_string(),
                album_id: "album-id".to_string(),
                artist_id: "artist-id".to_string(),
                title: title.to_string(),
                track_no: Some(6),
                disc_no: Some(1),
                duration_ms,
                codec: Codec::Mp3,
                sample_rate: None,
                channels: None,
                bitrate: None,
                file_relpath: String::new(),
                file_size: 0,
                genres: Vec::new(),
            },
            title_norm: normalize_match_text(title),
            artist_norm: normalize_match_text(artist),
            album_norm: normalize_match_text(album),
        }
    }

    #[test]
    fn metadata_match_accepts_strict_fuzzy_accents() {
        let matched = best_metadata_match(
            &descriptor(),
            &[candidate(
                "server-1",
                "Break My Soul",
                "Beyoncé",
                "Renaissance",
                278_000,
            )],
        )
        .expect("strict fuzzy metadata should match");

        assert_eq!(matched.track_id, "server-1");
        assert!(matched.score >= 0.94);
    }

    #[test]
    fn metadata_match_rejects_duration_mismatch() {
        let matched = best_metadata_match(
            &descriptor(),
            &[candidate(
                "server-1",
                "Break My Soul",
                "Beyoncé",
                "Renaissance",
                285_000,
            )],
        );

        assert!(matched.is_none());
    }

    #[test]
    fn metadata_match_rejects_ambiguous_second_best() {
        let matched = best_metadata_match(
            &descriptor(),
            &[
                candidate(
                    "server-1",
                    "Break My Soul",
                    "Beyoncé",
                    "Renaissance",
                    278_000,
                ),
                candidate(
                    "server-2",
                    "Break My Soul",
                    "Beyoncé",
                    "Renaissance",
                    278_000,
                ),
            ],
        );

        assert!(matched.is_none());
    }
}
