use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::http::extract_token;
use crate::http::json_error_response;
use crate::state::{AppState, HealthResponse};

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
