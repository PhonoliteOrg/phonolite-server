use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::admin::extract_token;
use crate::state::{AppState, HealthResponse, JsonResult, LoginRequest, LoginResponse};
use crate::utils::{json_error, json_error_response};

pub async fn auth_login(
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> JsonResult<LoginResponse> {
    if !state.auth.has_any_user().unwrap_or(false) {
        return Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "server not initialized",
        ));
    }

    let user = match state
        .auth
        .authenticate(&payload.username, &payload.password)
    {
        Ok(Some(user)) => user,
        Ok(None) => return Err(json_error(StatusCode::UNAUTHORIZED, "invalid credentials")),
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            ))
        }
    };

    let session = match state.auth.create_session(&user.id) {
        Ok(session) => session,
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            ))
        }
    };

    Ok(Json(LoginResponse {
        token: session.token,
        expires_at: session.expires_at,
        token_type: "Bearer",
    }))
}

pub async fn auth_logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = match extract_token(&headers) {
        Some(token) => token,
        None => return json_error_response(StatusCode::BAD_REQUEST, "missing token"),
    };

    if let Err(err) = state.auth.revoke_session(&token) {
        return json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("auth error: {}", err),
        );
    }

    Json(HealthResponse { status: "ok" }).into_response()
}

