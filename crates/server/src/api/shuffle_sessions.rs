//! Windowed playback over an immutable, account-owned shuffle deck.
//! Catalog selection and metadata projection run on bounded blocking workers;
//! request handlers never decode a whole music library on a Tokio worker.
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use crate::http::json_error;
use crate::shuffle_sessions::{ShufflePageIds, ShuffleSessionError, ShuffleSessionRequest};
use crate::state::{AppState, AuthContext, JsonResult};

use super::{
    library_or_json_error,
    views::{build_track_views, TrackView},
};

#[derive(Debug, Deserialize)]
pub struct ShufflePageQuery {
    pub client_id: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteShuffleQuery {
    pub client_id: String,
}

#[derive(Debug, Serialize)]
pub struct ShufflePageResponse {
    pub session_id: String,
    pub offset: usize,
    pub next_offset: usize,
    pub total: usize,
    pub has_more: bool,
    pub items: Vec<TrackView>,
    /// Deleted catalog entries leave holes in an existing deck. Offsets remain
    /// stable so retries, history navigation, and refill cursors cannot repeat.
    pub item_offsets: Vec<usize>,
}

pub async fn create_session(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(request): Json<ShuffleSessionRequest>,
) -> JsonResult<ShufflePageResponse> {
    ensure_running(&state)?;
    request.validate().map_err(api_error)?;
    if let Some(page) = state
        .shuffle_sessions
        .existing(&ctx.user.id, &request.client_id, &request)
        .map_err(api_error)?
    {
        return hydrate_page(state, ctx.user.id, page).await;
    }
    let library = library_or_json_error(&state)?;
    // Resolve personal candidates using authenticated ownership, never a user
    // identifier supplied by the caller. No personal candidate cache is shared.
    let mode = request.mode.trim().to_ascii_lowercase();
    let personal_ids = if matches!(mode.as_str(), "liked" | "likes" | "playlist") {
        let permit = state
            .shuffle_sessions
            .acquire_work_permit()
            .await
            .map_err(api_error)?;
        let store = state.user_data.clone();
        let user_id = ctx.user.id.clone();
        let playlist_id = request.playlist_id.clone();
        state
            .shutdown
            .spawn_blocking(move || {
                let _permit = permit;
                match mode.as_str() {
                    "liked" | "likes" => store.list_likes(&user_id).map(Some).map_err(|err| {
                        ShuffleSessionError::Library(format!("personal data: {err:?}"))
                    }),
                    "playlist" => {
                        let id =
                            playlist_id
                                .filter(|id| !id.trim().is_empty())
                                .ok_or_else(|| {
                                    ShuffleSessionError::BadRequest(
                                        "playlist_id required for shuffle=playlist".into(),
                                    )
                                })?;
                        store
                            .get_playlist(&user_id, &id)
                            .map_err(|err| {
                                ShuffleSessionError::Library(format!("personal data: {err:?}"))
                            })?
                            .map(|playlist| Some(playlist.track_ids))
                            .ok_or(ShuffleSessionError::NotFound)
                    }
                    _ => Ok(None),
                }
            })
            .await
            .map_err(join_error)?
            .map_err(api_error)?
    } else {
        None
    };
    let page = state
        .shuffle_sessions
        .create(
            library,
            state.metadata_events.revision(),
            ctx.user.id.clone(),
            request.client_id.clone(),
            request,
            personal_ids,
        )
        .await
        .map_err(api_error)?;
    hydrate_page(state, ctx.user.id, page).await
}

pub async fn get_page(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Query(query): Query<ShufflePageQuery>,
) -> JsonResult<ShufflePageResponse> {
    ensure_running(&state)?;
    let page = state
        .shuffle_sessions
        .page(
            ctx.user.id.clone(),
            query.client_id,
            session_id,
            query.offset.unwrap_or(0),
            query.limit.unwrap_or(10),
        )
        .await
        .map_err(api_error)?;
    hydrate_page(state, ctx.user.id, page).await
}

pub async fn delete_session(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Query(query): Query<DeleteShuffleQuery>,
) -> Result<StatusCode, (StatusCode, Json<crate::state::ErrorResponse>)> {
    state
        .shuffle_sessions
        .remove(ctx.user.id, query.client_id, session_id)
        .await
        .map_err(api_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn hydrate_page(
    state: AppState,
    user_id: String,
    page: ShufflePageIds,
) -> JsonResult<ShufflePageResponse> {
    ensure_running(&state)?;
    let library = library_or_json_error(&state)?;
    let permit = state
        .shuffle_sessions
        .acquire_work_permit()
        .await
        .map_err(api_error)?;
    state
        .shutdown
        .clone()
        .spawn_blocking(move || {
            let _permit = permit;
            ensure_running(&state).map_err(|_| ShuffleSessionError::Busy)?;
            let tracks = library
                .tracks_by_ids(&page.track_ids)
                .map_err(|err| ShuffleSessionError::Library(err.to_string()))?;
            let positions = page
                .track_ids
                .iter()
                .enumerate()
                .map(|(index, id)| (id.as_str(), page.offset + index))
                .collect::<std::collections::HashMap<_, _>>();
            let item_offsets = tracks
                .iter()
                .map(|track| positions[track.id.as_str()])
                .collect();
            let (liked, in_playlists) = state
                .user_data
                .track_membership(&user_id, &page.track_ids)
                .map_err(|err| ShuffleSessionError::Library(format!("personal data: {err:?}")))?;
            let items = build_track_views(&library, &tracks, &liked, &in_playlists)
                .map_err(ShuffleSessionError::Library)?;
            Ok(ShufflePageResponse {
                session_id: page.session_id,
                offset: page.offset,
                next_offset: page.next_offset,
                total: page.total,
                has_more: page.has_more,
                items,
                item_offsets,
            })
        })
        .await
        .map_err(join_error)?
        .map(Json)
        .map_err(api_error)
}

fn ensure_running(state: &AppState) -> Result<(), (StatusCode, Json<crate::state::ErrorResponse>)> {
    if state.shutdown.is_cancelled() {
        Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "server is shutting down",
        ))
    } else {
        Ok(())
    }
}

fn join_error(err: tokio::task::JoinError) -> (StatusCode, Json<crate::state::ErrorResponse>) {
    tracing::error!(error = %err, "shuffle worker failed");
    json_error(StatusCode::INTERNAL_SERVER_ERROR, "shuffle worker failed")
}

fn api_error(err: ShuffleSessionError) -> (StatusCode, Json<crate::state::ErrorResponse>) {
    match err {
        ShuffleSessionError::BadRequest(message) => json_error(StatusCode::BAD_REQUEST, message),
        ShuffleSessionError::NotFound => {
            json_error(StatusCode::NOT_FOUND, "shuffle session or scope not found")
        }
        ShuffleSessionError::Conflict(message) => json_error(StatusCode::CONFLICT, message),
        ShuffleSessionError::Busy => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "shuffle capacity reached; retry shortly",
        ),
        ShuffleSessionError::Library(message) => {
            tracing::error!(error = %message, "shuffle library operation failed");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unable to read shuffle library",
            )
        }
    }
}
