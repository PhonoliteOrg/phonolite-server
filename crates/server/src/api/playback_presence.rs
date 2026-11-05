use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use crate::http::{extract_bearer_token, json_error};
use crate::network_metrics::PresenceError;
use crate::state::{AppState, AuthContext, JsonResult};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PresenceRequest {
    client_id: String,
    sequence: u64,
    playing: bool,
}

#[derive(Serialize)]
pub(crate) struct PresenceResponse {
    accepted: bool,
}

/// Live playback state only: no track, listening event or persistent write.
pub(crate) async fn report(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
    Json(request): Json<PresenceRequest>,
) -> JsonResult<PresenceResponse> {
    let token = extract_bearer_token(&headers)
        .ok_or_else(|| json_error(StatusCode::UNAUTHORIZED, "bearer session required"))?;
    let accepted = state
        .process_metrics
        .network()
        .report_playback_presence(
            &ctx.user.id,
            &token,
            &request.client_id,
            request.sequence,
            request.playing,
        )
        .map_err(|error| match error {
            PresenceError::InvalidClientId => {
                json_error(StatusCode::BAD_REQUEST, "invalid client_id")
            }
            PresenceError::Capacity => {
                json_error(StatusCode::TOO_MANY_REQUESTS, "too many playback clients")
            }
        })?;
    Ok(Json(PresenceResponse { accepted }))
}
