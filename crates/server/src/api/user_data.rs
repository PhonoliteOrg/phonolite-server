use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::state::{AppState, CreatePlaylistRequest, JsonResult, Playlist, UpdatePlaylistRequest};
use crate::user_data::LikeState;
use crate::utils::json_error;

#[derive(Debug, Deserialize)]
pub struct BatchLikeRequest {
    #[serde(default)]
    pub items: Vec<BatchLikeUpdate>,
}

#[derive(Debug, Deserialize)]
pub struct BatchLikeUpdate {
    pub track_id: String,
    pub liked: bool,
    pub updated_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LikeStateView {
    pub track_id: String,
    pub liked: bool,
    pub updated_at: u64,
}

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
        None => Err(json_error(
            StatusCode::NOT_FOUND,
            "playlist not found".to_string(),
        )),
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
        Err(json_error(
            StatusCode::NOT_FOUND,
            "playlist not found".to_string(),
        ))
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

pub async fn batch_update_likes(
    State(state): State<AppState>,
    Json(payload): Json<BatchLikeRequest>,
) -> JsonResult<Vec<LikeStateView>> {
    if payload.items.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let mut out = Vec::new();
    for item in payload.items {
        let track_id = item.track_id.trim();
        if track_id.is_empty() {
            continue;
        }
        let like_state = state
            .user_data
            .set_like_state_with_updated_at(track_id, item.liked, item.updated_at)
            .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
        out.push(like_state_view(track_id.to_string(), like_state));
    }
    Ok(Json(out))
}

pub(crate) fn like_state_view(track_id: String, state: LikeState) -> LikeStateView {
    LikeStateView {
        track_id,
        liked: state.liked,
        updated_at: state.updated_at,
    }
}
