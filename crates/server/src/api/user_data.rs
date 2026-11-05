use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use crate::assets::cover_response;
use crate::http::{json_error, json_error_response};
use crate::state::{AppState, AuthContext, CreatePlaylistRequest, JsonResult, Playlist};
use crate::user_data::{LikeState, UserDataError, UserDataStore, VersionedPlaylist};

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

#[derive(Debug, Deserialize)]
pub struct VersionedUpdatePlaylistRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub track_ids: Option<Vec<String>>,
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct PlaylistTracksRequest {
    pub track_ids: Vec<String>,
}

pub async fn list_playlists(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> JsonResult<Vec<VersionedPlaylist>> {
    let playlists = state
        .user_data
        .collection_snapshot(&ctx.user.id)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)))?;
    Ok(Json(playlists.playlists))
}

pub async fn create_playlist(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(payload): Json<CreatePlaylistRequest>,
) -> JsonResult<VersionedPlaylist> {
    ensure_mutation_running(&state)?;
    let user_id = ctx.user.id.clone();
    if payload.name.trim().is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "name is required".to_string(),
        ));
    }
    let playlist = commit_user_data(&state, "playlist_create", move |store| {
        store.create_playlist(
            &ctx.user.id,
            payload.name.trim().to_string(),
            normalize_optional_text(payload.description),
            payload.track_ids,
        )
    })
    .await?;
    playlist_response(&state, &user_id, playlist)
}

pub async fn update_playlist(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(playlist_id): AxumPath<String>,
    Json(payload): Json<VersionedUpdatePlaylistRequest>,
) -> JsonResult<VersionedPlaylist> {
    ensure_mutation_running(&state)?;
    let user_id = ctx.user.id.clone();
    let updated = commit_user_data(&state, "playlist_update", move |store| {
        store.update_playlist_checked(
            &ctx.user.id,
            &playlist_id,
            payload.name,
            payload.description.map(|value| value.trim().to_string()),
            payload.track_ids,
            payload.expected_revision,
        )
    })
    .await?;
    match updated {
        Some(playlist) => playlist_response(&state, &user_id, playlist),
        None => Err(json_error(
            StatusCode::NOT_FOUND,
            "playlist not found".to_string(),
        )),
    }
}

pub async fn add_playlist_tracks(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(playlist_id): AxumPath<String>,
    Json(payload): Json<PlaylistTracksRequest>,
) -> JsonResult<VersionedPlaylist> {
    change_playlist_tracks(state, ctx, playlist_id, payload.track_ids, true).await
}

pub async fn remove_playlist_tracks(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(playlist_id): AxumPath<String>,
    Json(payload): Json<PlaylistTracksRequest>,
) -> JsonResult<VersionedPlaylist> {
    change_playlist_tracks(state, ctx, playlist_id, payload.track_ids, false).await
}

async fn change_playlist_tracks(
    state: AppState,
    ctx: AuthContext,
    playlist_id: String,
    track_ids: Vec<String>,
    add: bool,
) -> JsonResult<VersionedPlaylist> {
    let user_id = ctx.user.id.clone();
    let updated = commit_user_data(&state, "playlist_tracks", move |store| {
        store.change_playlist_tracks(&ctx.user.id, &playlist_id, track_ids, add)
    })
    .await?;
    match updated {
        Some(playlist) => playlist_response(&state, &user_id, playlist),
        None => Err(json_error(StatusCode::NOT_FOUND, "playlist not found")),
    }
}

pub async fn delete_playlist(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(playlist_id): AxumPath<String>,
) -> JsonResult<()> {
    ensure_mutation_running(&state)?;
    let deleted = commit_user_data(&state, "playlist_delete", move |store| {
        store.delete_playlist(&ctx.user.id, &playlist_id)
    })
    .await?;
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
    Extension(ctx): Extension<AuthContext>,
    AxumPath(playlist_id): AxumPath<String>,
) -> Response {
    match state
        .user_data
        .get_playlist_image(&ctx.user.id, &playlist_id)
    {
        Ok(Some(image)) => {
            let mut response = cover_response(image.bytes, &image.content_type);
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-store"),
            );
            response
        }
        Ok(None) => json_error_response(StatusCode::NOT_FOUND, "cover not found"),
        Err(err) => json_error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err)),
    }
}

pub async fn upload_playlist_cover(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(playlist_id): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> JsonResult<VersionedPlaylist> {
    ensure_mutation_running(&state)?;
    let user_id = ctx.user.id.clone();
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
    let updated = commit_user_data(&state, "playlist_cover_store", move |store| {
        store.set_playlist_image(&ctx.user.id, &playlist_id, content_type, body.to_vec())
    })
    .await?;
    match updated {
        Some(playlist) => playlist_response(&state, &user_id, playlist),
        None => Err(json_error(
            StatusCode::NOT_FOUND,
            "playlist not found".to_string(),
        )),
    }
}

pub async fn delete_playlist_cover(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(playlist_id): AxumPath<String>,
) -> JsonResult<VersionedPlaylist> {
    ensure_mutation_running(&state)?;
    let user_id = ctx.user.id.clone();
    let updated = commit_user_data(&state, "playlist_cover_delete", move |store| {
        store.clear_playlist_image(&ctx.user.id, &playlist_id)
    })
    .await?;
    match updated {
        Some(playlist) => playlist_response(&state, &user_id, playlist),
        None => Err(json_error(
            StatusCode::NOT_FOUND,
            "playlist not found".to_string(),
        )),
    }
}

pub async fn add_like(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(track_id): AxumPath<String>,
) -> JsonResult<()> {
    ensure_mutation_running(&state)?;
    commit_user_data(&state, "add_like", move |store| {
        store.add_like(&ctx.user.id, &track_id)
    })
    .await?;
    Ok(Json(()))
}

pub async fn remove_like(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(track_id): AxumPath<String>,
) -> JsonResult<()> {
    ensure_mutation_running(&state)?;
    commit_user_data(&state, "remove_like", move |store| {
        store.remove_like(&ctx.user.id, &track_id)
    })
    .await?;
    Ok(Json(()))
}

pub async fn batch_update_likes(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(payload): Json<BatchLikeRequest>,
) -> JsonResult<Vec<LikeStateView>> {
    ensure_mutation_running(&state)?;
    if payload.items.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let out = commit_user_data(&state, "likes_batch_update", move |store| {
        let items = payload
            .items
            .into_iter()
            .filter_map(|item| {
                let track_id = item.track_id.trim().to_owned();
                (!track_id.is_empty()).then_some((track_id, item.liked, item.updated_at))
            })
            .collect();
        Ok(store
            .set_like_states(&ctx.user.id, items)?
            .into_iter()
            .map(|(id, state)| like_state_view(id, state))
            .collect())
    })
    .await?;
    Ok(Json(out))
}

fn playlist_response(
    state: &AppState,
    user_id: &str,
    playlist: Playlist,
) -> JsonResult<VersionedPlaylist> {
    // A later concurrent commit may already exist. Return a consistent current
    // playlist/revision pair rather than attaching a newer version to old data.
    match state
        .user_data
        .versioned_playlist(user_id, &playlist.id)
        .map_err(user_data_error)?
    {
        Some(playlist) => Ok(Json(playlist)),
        None => Err(json_error(StatusCode::NOT_FOUND, "playlist not found")),
    }
}

fn user_data_error(err: UserDataError) -> (StatusCode, Json<crate::state::ErrorResponse>) {
    let status = match err {
        UserDataError::PreconditionRequired => StatusCode::PRECONDITION_REQUIRED,
        UserDataError::PlaylistConflict => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    json_error(status, err.to_string())
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

fn ensure_mutation_running(
    state: &AppState,
) -> Result<(), (StatusCode, Json<crate::state::ErrorResponse>)> {
    if state.shutdown.is_cancelled() {
        Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "server shutting down",
        ))
    } else {
        Ok(())
    }
}

async fn commit_user_data<R, F>(
    state: &AppState,
    name: &str,
    work: F,
) -> Result<R, (StatusCode, Json<crate::state::ErrorResponse>)>
where
    R: Send + 'static,
    F: FnOnce(UserDataStore) -> Result<R, UserDataError> + Send + 'static,
{
    ensure_mutation_running(state)?;
    let store = state.user_data.clone();
    state
        .shutdown
        .spawn_blocking_commit(name, move || work(store))
        .await
        .map_err(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("user data task failed: {err}"),
            )
        })?
        .map_err(user_data_error)
}
