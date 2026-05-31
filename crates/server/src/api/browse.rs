use std::collections::HashSet;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    Extension, Json,
};

use crate::state::{AppState, ArtistQuery, AuthContext, JsonResult, ListResponse, Playlist};
use crate::utils::json_error;

use super::{
    library_or_json_error,
    views::{
        build_album_views, build_artist_view, build_artist_views, build_track_views, AlbumView,
        ArtistView, TrackView,
    },
};

pub async fn list_artists(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    Query(params): Query<ArtistQuery>,
) -> JsonResult<ListResponse<ArtistView>> {
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

    let items = build_artist_views(&library, artists)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;

    Ok(Json(ListResponse { items, total }))
}

pub async fn list_artist_albums(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    AxumPath(artist_id): AxumPath<String>,
) -> JsonResult<Vec<AlbumView>> {
    let library = library_or_json_error(&state)?;
    let _artist = match library.get_artist(&artist_id) {
        Ok(Some(artist)) => artist,
        Ok(None) => {
            return Err(json_error(
                StatusCode::NOT_FOUND,
                "artist not found".to_string(),
            ))
        }
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

    let items = build_album_views(&library, albums)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(Json(items))
}

pub async fn get_artist(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    AxumPath(artist_id): AxumPath<String>,
) -> JsonResult<ArtistView> {
    let library = library_or_json_error(&state)?;
    match library.get_artist(&artist_id) {
        Ok(Some(artist)) => build_artist_view(&library, artist)
            .map(Json)
            .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err)),
        Ok(None) => Err(json_error(
            StatusCode::NOT_FOUND,
            "artist not found".to_string(),
        )),
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
        return Err(json_error(
            StatusCode::NOT_FOUND,
            "album not found".to_string(),
        ));
    }
    tracks.sort_by(|a, b| {
        a.disc_no
            .unwrap_or(0)
            .cmp(&b.disc_no.unwrap_or(0))
            .then_with(|| a.track_no.unwrap_or(0).cmp(&b.track_no.unwrap_or(0)))
            .then_with(|| {
                a.title
                    .to_ascii_lowercase()
                    .cmp(&b.title.to_ascii_lowercase())
            })
    });

    let liked_set = liked_set(&state)?;
    let playlist_set = playlist_set(&state)?;

    let items = build_track_views(&library, &tracks, &liked_set, &playlist_set)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
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
        Ok(None) => {
            return Err(json_error(
                StatusCode::NOT_FOUND,
                "track not found".to_string(),
            ))
        }
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };
    let liked_set = liked_set(&state)?;
    let playlist_set = playlist_set(&state)?;
    let mut items = build_track_views(
        &library,
        std::slice::from_ref(&track),
        &liked_set,
        &playlist_set,
    )
    .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    let view = items
        .pop()
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "track not found".to_string()))?;
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
        return Err(json_error(
            StatusCode::NOT_FOUND,
            "playlist not found".to_string(),
        ));
    };
    let library = library_or_json_error(&state)?;
    let liked_set = liked_set(&state)?;
    let playlist_set = playlist_set(&state)?;
    let mut tracks = Vec::new();
    for track_id in playlist.track_ids {
        if let Ok(Some(track)) = library.get_track(&track_id) {
            tracks.push(track);
        }
    }
    let items = build_track_views(&library, &tracks, &liked_set, &playlist_set)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
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
    let mut tracks = Vec::new();
    for track_id in track_ids {
        if let Ok(Some(track)) = library.get_track(&track_id) {
            tracks.push(track);
        }
    }
    let items = build_track_views(&library, &tracks, &liked_set, &playlist_set)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(Json(items))
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
