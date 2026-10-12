//! HTTP-facing bindings for catalog metadata and user personalization.
use crate::http::json_error;
pub use crate::library_views::{
    album_artist_name, build_album_view, build_album_views, build_artist_view, build_artist_views,
    build_track_views, AlbumView, ArtistView, TrackView,
};
use crate::state::{AppState, Playlist};
use axum::{http::StatusCode, Json};
use std::collections::HashSet;

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
