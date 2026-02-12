use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    Json,
};

use crate::state::{
    AppState, CreatePlaylistRequest, JsonResult, Playlist, UpdatePlaylistRequest,
};
use crate::utils::json_error;

pub async fn list_playlists(State(state): State<AppState>) -> JsonResult<Vec<Playlist>> {
    let playlists = state
        .user_data
        .list_playlists()
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    Ok(Json(playlists))
}

pub async fn create_playlist(
    State(state): State<AppState>,
    Json(payload): Json<CreatePlaylistRequest>,
) -> JsonResult<Playlist> {
    if payload.name.trim().is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "name is required".to_string(),
        ));
    }
    let playlist = state
        .user_data
        .create_playlist(payload.name.trim().to_string(), payload.track_ids)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    Ok(Json(playlist))
}

pub async fn update_playlist(
    State(state): State<AppState>,
    AxumPath(playlist_id): AxumPath<String>,
    Json(payload): Json<UpdatePlaylistRequest>,
) -> JsonResult<Playlist> {
    let updated = state
        .user_data
        .update_playlist(&playlist_id, payload.name, payload.track_ids)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    match updated {
        Some(playlist) => Ok(Json(playlist)),
        None => Err(json_error(StatusCode::NOT_FOUND, "playlist not found".to_string())),
    }
}

pub async fn delete_playlist(
    State(state): State<AppState>,
    AxumPath(playlist_id): AxumPath<String>,
) -> JsonResult<()> {
    let deleted = state
        .user_data
        .delete_playlist(&playlist_id)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    if deleted {
        Ok(Json(()))
    } else {
        Err(json_error(StatusCode::NOT_FOUND, "playlist not found".to_string()))
    }
}

pub async fn add_like(
    State(state): State<AppState>,
    AxumPath(track_id): AxumPath<String>,
) -> JsonResult<()> {
    state
        .user_data
        .add_like(&track_id)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    Ok(Json(()))
}

pub async fn remove_like(
    State(state): State<AppState>,
    AxumPath(track_id): AxumPath<String>,
) -> JsonResult<()> {
    state
        .user_data
        .remove_like(&track_id)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    Ok(Json(()))
}
