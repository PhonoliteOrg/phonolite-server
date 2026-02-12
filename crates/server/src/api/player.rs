use axum::{extract::State, http::StatusCode, Extension, Json};
use serde::{Deserialize, Serialize};

use crate::state::{AppState, AuthContext, JsonResult};
use crate::user_data::PlaybackSettings;
use crate::utils::json_error;

const DEFAULT_REPEAT_MODE: &str = "off";

#[derive(Serialize)]
pub struct PlaybackSettingsResponse {
    pub repeat_mode: String,
}

#[derive(Deserialize)]
pub struct UpdatePlaybackSettingsRequest {
    pub repeat_mode: Option<String>,
}

pub async fn get_playback_settings(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
) -> JsonResult<PlaybackSettingsResponse> {
    let settings = state
        .user_data
        .get_playback_settings()
        .map_err(|err: crate::user_data::UserDataError| {
            json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
        })?;
    let repeat_mode = settings
        .map(|s| s.repeat_mode)
        .unwrap_or_else(|| DEFAULT_REPEAT_MODE.to_string());
    Ok(Json(PlaybackSettingsResponse { repeat_mode }))
}

pub async fn update_playback_settings(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    Json(payload): Json<UpdatePlaybackSettingsRequest>,
) -> JsonResult<PlaybackSettingsResponse> {
    let repeat_mode = payload
        .repeat_mode
        .unwrap_or_else(|| DEFAULT_REPEAT_MODE.to_string())
        .to_ascii_lowercase();

    if !is_valid_repeat_mode(&repeat_mode) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "invalid repeat_mode".to_string(),
        ));
    }

    let settings = PlaybackSettings { repeat_mode };
    state
        .user_data
        .set_playback_settings(settings.clone())
        .map_err(|err: crate::user_data::UserDataError| {
            json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
        })?;

    Ok(Json(PlaybackSettingsResponse {
        repeat_mode: settings.repeat_mode,
    }))
}

fn is_valid_repeat_mode(value: &str) -> bool {
    matches!(value, "off" | "one")
}
