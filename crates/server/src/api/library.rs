pub use super::downloads::{create_download_batch, download_track, get_offline_track_metadata};

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    response::Response,
    Json,
};
use common::{Album, Artist, Track};
use serde::{Deserialize, Serialize};
use strsim::normalized_levenshtein;
use tokio::sync::broadcast;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

use crate::assets::{
    cover_response, fetch_cover_cached, metadata_root_path, resolve_artist_banner_source,
    resolve_artist_cover_source, resolve_artist_logo_source, resolve_cover_source, CoverCacheKey,
};
use crate::http::{json_error, json_error_response};
use crate::shuffle::{build_shuffle_queue, ShuffleError, ShuffleMode};
use crate::state::{
    AppState, ArtistCoverQuery, JsonResult, SearchQuery, SearchResult, ShuffleQuery,
};

use super::{
    library_or_json_error, library_or_response,
    views::{
        album_artist_name, build_album_view, build_track_views, liked_set, playlist_set, AlbumView,
        TrackView,
    },
};

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

    let key = CoverCacheKey::Artist {
        id: artist_id.clone(),
        variant: query.kind.as_deref().unwrap_or("cover").to_string(),
    };
    match fetch_cover_cached(&state, key, source).await {
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
                album_id: None,
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
        let credits = library
            .album_credits(&album.id)
            .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        let artist_name = if credits.display_name.trim().is_empty() {
            album_artist_name(&album, album_artist_names.get(&album.artist_id).cloned())
        } else {
            credits.display_name
        };
        let combined = format!("{} {}", album.title, artist_name);
        let score = score_match(&normalized, &combined);
        if score > 0 {
            results.push(SearchResult {
                kind: "album".to_string(),
                album_id: Some(album.id.clone()),
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
        let credits = library
            .track_credits(&track.id)
            .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        let artist_name = if credits.display_name.trim().is_empty() {
            track_artist_names
                .get(&track.artist_id)
                .cloned()
                .unwrap_or_else(|| "Unknown Artist".to_string())
        } else {
            credits.display_name
        };
        let album_title = track_album_titles
            .get(&track.album_id)
            .cloned()
            .unwrap_or_else(|| "Unknown Album".to_string());
        let combined = format!("{} {} {}", track.title, artist_name, album_title);
        let score = score_match(&normalized, &combined);
        if score > 0 {
            results.push(SearchResult {
                kind: "track".to_string(),
                album_id: Some(track.album_id.clone()),
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
        .list_artists_for_browse(Some(query), limit, 0, false)
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
            let credits = library
                .track_credits(&track.id)
                .map_err(|err| err.to_string())?;
            let artist = if credits.display_name.trim().is_empty() {
                artist_names
                    .get(&track.artist_id)
                    .cloned()
                    .unwrap_or_else(|| "Unknown Artist".to_string())
            } else {
                credits.display_name
            };
            let album = album_titles
                .get(&track.album_id)
                .cloned()
                .unwrap_or_else(|| "Unknown Album".to_string());
            Ok(TrackMatchCandidate {
                track: track.clone(),
                title_norm: normalize_match_text(&track.title),
                artist_norm: normalize_match_text(&artist),
                album_norm: normalize_match_text(&album),
            })
        })
        .collect::<Result<Vec<_>, String>>()?)
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
    for ch in value.chars().flat_map(char::to_lowercase) {
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
    fn search_scoring_preserves_non_latin_names() {
        assert_eq!(normalize_search("東京"), "東京");
        assert_eq!(score_match(&normalize_search("東京"), "東京"), 100);
        assert_eq!(score_match(&normalize_search("東京"), "大阪"), 0);
        assert_eq!(score_match(&normalize_search("Björk"), "BJÖRK"), 100);
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
