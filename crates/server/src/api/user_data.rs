use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::assets::cover_response;
use crate::state::{AppState, CreatePlaylistRequest, JsonResult, Playlist, UpdatePlaylistRequest};
use crate::user_data::LikeState;
use crate::utils::{json_error, json_error_response};

const MAX_PLAYLIST_IMAGE_BYTES: usize = 5 * 1024 * 1024;

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
        .create_playlist(
            payload.name.trim().to_string(),
            normalize_optional_text(payload.description),
            payload.track_ids,
        )
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
        .update_playlist(
            &playlist_id,
            payload.name,
            payload.description.map(|value| value.trim().to_string()),
            payload.track_ids,
        )
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

pub async fn get_playlist_cover(
    State(state): State<AppState>,
    AxumPath(playlist_id): AxumPath<String>,
) -> Response {
    match state.user_data.get_playlist_image(&playlist_id) {
        Ok(Some(image)) => cover_response(image.bytes, &image.content_type),
        Ok(None) => json_error_response(StatusCode::NOT_FOUND, "cover not found"),
        Err(err) => json_error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)),
    }
}

pub async fn upload_playlist_cover(
    State(state): State<AppState>,
    AxumPath(playlist_id): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> JsonResult<Playlist> {
    if body.is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "image is required".to_string(),
        ));
    }
    if body.len() > MAX_PLAYLIST_IMAGE_BYTES {
        return Err(json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "playlist image is too large".to_string(),
        ));
    }
    let content_type = match normalize_image_content_type(&headers) {
        Some(content_type) => content_type,
        None => {
            return Err(json_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported playlist image type".to_string(),
            ))
        }
    };
    let updated = state
        .user_data
        .set_playlist_image(&playlist_id, content_type, body.to_vec())
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    match updated {
        Some(playlist) => Ok(Json(playlist)),
        None => Err(json_error(
            StatusCode::NOT_FOUND,
            "playlist not found".to_string(),
        )),
    }
}

pub async fn delete_playlist_cover(
    State(state): State<AppState>,
    AxumPath(playlist_id): AxumPath<String>,
) -> JsonResult<Playlist> {
    let updated = state
        .user_data
        .clear_playlist_image(&playlist_id)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    match updated {
        Some(playlist) => Ok(Json(playlist)),
        None => Err(json_error(
            StatusCode::NOT_FOUND,
            "playlist not found".to_string(),
        )),
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

fn normalize_image_content_type(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let content_type = match value.as_str() {
        "image/jpeg" | "image/jpg" => "image/jpeg",
        "image/png" => "image/png",
        "image/webp" => "image/webp",
        "image/gif" => "image/gif",
        _ => return None,
    };
    Some(content_type.to_string())
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
