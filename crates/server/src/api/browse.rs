use std::collections::HashSet;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Serialize;
use common::Artist;

use crate::state::{AppState, ArtistQuery, AuthContext, JsonResult, ListResponse, Playlist};
use crate::utils::json_error;

use super::library_or_json_error;

#[derive(Serialize)]
pub struct BrowseArtist {
    pub id: String,
    pub name: String,
    pub genres: Vec<String>,
    pub album_count: usize,
    pub summary: Option<String>,
    pub logo_ref: Option<String>,
    pub banner_ref: Option<String>,
}

#[derive(Serialize)]
pub struct BrowseAlbum {
    pub id: String,
    pub artist_id: String,
    pub artist_name: String,
    pub title: String,
    pub year: Option<i32>,
    pub genres: Vec<String>,
    pub track_count: usize,
    pub summary: Option<String>,
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

pub async fn list_artists(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    Query(params): Query<ArtistQuery>,
) -> JsonResult<ListResponse<BrowseArtist>> {
    let library = library_or_json_error(&state)?;
    let limit = params.limit.unwrap_or(200).max(1);
    let offset = params.offset.unwrap_or(0);
    let search = params.search.as_deref();

    let (artists, total) = match library.list_artists(search, limit, offset) {
        Ok(value) => value,
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };

    let mut items = Vec::with_capacity(artists.len());
    for artist in artists {
        let album_count = match library.list_artist_albums(&artist.id) {
            Ok(albums) => albums.len(),
            Err(_) => 0,
        };
        items.push(BrowseArtist {
            id: artist.id,
            name: artist.name,
            genres: artist.genres,
            album_count,
            summary: artist.summary,
            logo_ref: artist.logo_ref,
            banner_ref: artist.banner_ref,
        });
    }

    Ok(Json(ListResponse { items, total }))
}

pub async fn list_artist_albums(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    AxumPath(artist_id): AxumPath<String>,
) -> JsonResult<Vec<BrowseAlbum>> {
    let library = library_or_json_error(&state)?;
    let artist = match library.get_artist(&artist_id) {
        Ok(Some(artist)) => artist,
        Ok(None) => return Err(json_error(StatusCode::NOT_FOUND, "artist not found".to_string())),
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };
    let albums = match library.list_artist_albums(&artist_id) {
        Ok(albums) => albums,
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };

    let mut items = Vec::with_capacity(albums.len());
    for album in albums {
        let track_count = match library.get_album_tracks(&album.id) {
            Ok(tracks) => tracks.len(),
            Err(_) => 0,
        };
        items.push(BrowseAlbum {
            id: album.id,
            artist_id: album.artist_id,
            artist_name: artist.name.clone(),
            title: album.title,
            year: album.year,
            genres: album.genres,
            track_count,
            summary: album.summary,
        });
    }
    Ok(Json(items))
}

pub async fn get_artist(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    AxumPath(artist_id): AxumPath<String>,
) -> JsonResult<Artist> {
    let library = library_or_json_error(&state)?;
    match library.get_artist(&artist_id) {
        Ok(Some(artist)) => Ok(Json(artist)),
        Ok(None) => Err(json_error(StatusCode::NOT_FOUND, "artist not found".to_string())),
        Err(err) => Err(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("library error: {}", err),
        )),
    }
}

pub async fn list_album_tracks(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    AxumPath(album_id): AxumPath<String>,
) -> JsonResult<Vec<TrackView>> {
    let library = library_or_json_error(&state)?;
    let mut tracks = match library.get_album_tracks(&album_id) {
        Ok(tracks) => tracks,
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };
    if tracks.is_empty() {
        return Err(json_error(StatusCode::NOT_FOUND, "album not found".to_string()));
    }
    tracks.sort_by(|a, b| {
        a.disc_no
            .unwrap_or(0)
            .cmp(&b.disc_no.unwrap_or(0))
            .then_with(|| a.track_no.unwrap_or(0).cmp(&b.track_no.unwrap_or(0)))
            .then_with(|| a.title.to_ascii_lowercase().cmp(&b.title.to_ascii_lowercase()))
    });

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

pub async fn get_track(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    AxumPath(track_id): AxumPath<String>,
) -> JsonResult<TrackView> {
    let library = library_or_json_error(&state)?;
    let track = match library.get_track(&track_id) {
        Ok(Some(track)) => track,
        Ok(None) => return Err(json_error(StatusCode::NOT_FOUND, "track not found".to_string())),
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };
    let liked_set = liked_set(&state)?;
    let playlist_set = playlist_set(&state)?;
    let view = build_track_view(&library, &track, &liked_set, &playlist_set)?;
    Ok(Json(view))
}

pub async fn list_playlist_tracks(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    AxumPath(playlist_id): AxumPath<String>,
) -> JsonResult<Vec<TrackView>> {
    let playlist = state
        .user_data
        .get_playlist(&playlist_id)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    let Some(playlist) = playlist else {
        return Err(json_error(StatusCode::NOT_FOUND, "playlist not found".to_string()));
    };
    let library = library_or_json_error(&state)?;
    let liked_set = liked_set(&state)?;
    let playlist_set = playlist_set(&state)?;
    let mut items = Vec::new();
    for track_id in playlist.track_ids {
        if let Ok(Some(track)) = library.get_track(&track_id) {
            if let Ok(view) = build_track_view(&library, &track, &liked_set, &playlist_set) {
                items.push(view);
            }
        }
    }
    Ok(Json(items))
}

pub async fn list_liked_tracks(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
) -> JsonResult<Vec<TrackView>> {
    let track_ids = state
        .user_data
        .list_likes()
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    let library = library_or_json_error(&state)?;
    let liked_set = liked_set(&state)?;
    let playlist_set = playlist_set(&state)?;
    let mut items = Vec::new();
    for track_id in track_ids {
        if let Ok(Some(track)) = library.get_track(&track_id) {
            if let Ok(view) = build_track_view(&library, &track, &liked_set, &playlist_set) {
                items.push(view);
            }
        }
    }
    Ok(Json(items))
}

fn liked_set(state: &AppState) -> Result<HashSet<String>, (StatusCode, Json<crate::state::ErrorResponse>)> {
    let liked_ids = state
        .user_data
        .list_likes()
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    Ok(liked_ids.into_iter().collect())
}

fn playlist_set(state: &AppState) -> Result<HashSet<String>, (StatusCode, Json<crate::state::ErrorResponse>)> {
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
