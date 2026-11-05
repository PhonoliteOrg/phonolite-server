//! Account-owned data. Playback seed writes never control another session.
use crate::{
    http::{extract_token, json_error},
    state::{AppState, AuthContext, JsonResult},
    user_data::{LastTrack, ShufflePreset, ShufflePresets, UserDataError},
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Extension, Json,
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct PresetInput {
    pub name: String,
    #[serde(default)]
    pub artist_ids: Vec<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    pub expected_revision: Option<u64>,
}
#[derive(Deserialize)]
pub struct ExpectedRevision {
    pub expected_revision: u64,
}
#[derive(Deserialize)]
pub struct TrackStart {
    pub track_id: String,
    pub session_id: String,
    pub sequence: u64,
    pub request_id: String,
}
#[derive(Serialize)]
pub struct Revocation {
    revoked: u64,
}

pub async fn presets(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> JsonResult<ShufflePresets> {
    let store = state.user_data.clone();
    read(move || store.shuffle_presets(&ctx.user.id))
        .await
        .map(Json)
}
pub async fn create_preset(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(input): Json<PresetInput>,
) -> JsonResult<ShufflePreset> {
    mutate(&state, move |store| {
        store.save_shuffle_preset(
            &ctx.user.id,
            None,
            input.name,
            input.artist_ids,
            input.genres,
            None,
        )
    })
    .await?
    .map(Json)
    .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "preset not found"))
}
pub async fn update_preset(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(input): Json<PresetInput>,
) -> JsonResult<ShufflePreset> {
    if input.expected_revision.is_none() {
        return Err(json_error(
            StatusCode::PRECONDITION_REQUIRED,
            "expected_revision is required",
        ));
    }
    mutate(&state, move |store| {
        store.save_shuffle_preset(
            &ctx.user.id,
            Some(&id),
            input.name,
            input.artist_ids,
            input.genres,
            input.expected_revision,
        )
    })
    .await?
    .map(Json)
    .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "preset not found"))
}
pub async fn delete_preset(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
    Query(expected): Query<ExpectedRevision>,
) -> Result<StatusCode, (StatusCode, Json<crate::state::ErrorResponse>)> {
    if mutate(&state, move |store| {
        store.delete_shuffle_preset(&ctx.user.id, &id, expected.expected_revision)
    })
    .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(json_error(StatusCode::NOT_FOUND, "preset not found"))
    }
}
pub async fn last_track(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> JsonResult<LastTrack> {
    let store = state.user_data.clone();
    let mut seed = read(move || store.last_track(&ctx.user.id)).await?;
    // Removed library entries are unavailable seeds, never an instruction to play.
    if let Some(id) = &seed.track_id {
        let library = state.library_state.read().library.clone();
        if !library
            .as_ref()
            .is_some_and(|library| matches!(library.get_track(id), Ok(Some(_))))
        {
            seed.track_id = None;
        }
    }
    Ok(Json(seed))
}
pub async fn track_started(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(input): Json<TrackStart>,
) -> JsonResult<LastTrack> {
    let library = super::library_or_json_error(&state)?;
    match library.get_track(&input.track_id) {
        Ok(Some(_)) => {}
        Ok(None) => return Err(json_error(StatusCode::NOT_FOUND, "track not found")),
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                err.to_string(),
            ))
        }
    }
    mutate(&state, move |store| {
        store.record_track_start(
            &ctx.user.id,
            &input.track_id,
            &input.session_id,
            input.sequence,
            &input.request_id,
        )
    })
    .await
    .map(Json)
}
pub async fn revoke_others(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> JsonResult<Revocation> {
    revoke(state, headers, true).await
}
pub async fn revoke_all(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> JsonResult<Revocation> {
    revoke(state, headers, false).await
}
async fn revoke(state: AppState, headers: HeaderMap, keep_current: bool) -> JsonResult<Revocation> {
    let token = extract_token(&headers)
        .ok_or_else(|| json_error(StatusCode::UNAUTHORIZED, "unauthorized"))?;
    if state.shutdown.is_cancelled() {
        return Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "server shutting down",
        ));
    }
    let user = state
        .auth
        .user_from_token(&token)
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .ok_or_else(|| json_error(StatusCode::UNAUTHORIZED, "unauthorized"))?;
    let current_token = keep_current.then(|| token.clone());
    let auth = state.auth.clone();
    let revoked = state
        .shutdown
        .spawn_blocking_commit("revoke_account_sessions", move || {
            auth.revoke_own_sessions(&token, keep_current)
        })
        .await
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .map_err(|err| {
            let status = match err {
                crate::auth::AuthError::DbError(_) => StatusCode::INTERNAL_SERVER_ERROR,
                _ => StatusCode::UNAUTHORIZED,
            };
            json_error(status, err.to_string())
        })?;
    state
        .process_metrics
        .network()
        .clear_playback_user_except(&user.id, current_token.as_deref());
    Ok(Json(Revocation { revoked }))
}
fn error(err: UserDataError) -> (StatusCode, Json<crate::state::ErrorResponse>) {
    let status = match &err {
        UserDataError::Invalid(_) => StatusCode::BAD_REQUEST,
        UserDataError::PresetConflict => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    json_error(status, err.to_string())
}
async fn read<R: Send + 'static>(
    work: impl FnOnce() -> Result<R, UserDataError> + Send + 'static,
) -> Result<R, (StatusCode, Json<crate::state::ErrorResponse>)> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .map_err(error)
}
async fn mutate<R: Send + 'static>(
    state: &AppState,
    work: impl FnOnce(crate::user_data::UserDataStore) -> Result<R, UserDataError> + Send + 'static,
) -> Result<R, (StatusCode, Json<crate::state::ErrorResponse>)> {
    if state.shutdown.is_cancelled() {
        return Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "server shutting down",
        ));
    }
    let store = state.user_data.clone();
    state
        .shutdown
        .spawn_blocking_commit("account_data", move || work(store))
        .await
        .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .map_err(error)
}
