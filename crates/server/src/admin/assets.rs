// crates/server/src/admin/assets.rs
use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, HeaderValue, StatusCode},
    response::Response,
};

use crate::state::AppState;
use crate::utils::{json_error_response, web_root};

pub async fn admin_asset(State(state): State<AppState>, AxumPath(file): AxumPath<String>) -> Response {
    let path = web_root(&state).join("static").join(&file);
    if !path.starts_with(web_root(&state).join("static")) {
        return json_error_response(StatusCode::FORBIDDEN, "forbidden");
    }
    let data = match tokio::fs::read(&path).await {
        Ok(data) => data,
        Err(_) => return json_error_response(StatusCode::NOT_FOUND, "asset not found"),
    };
    let mime = if file.ends_with(".js") {
        "text/javascript"
    } else if file.ends_with(".css") {
        "text/css"
    } else {
        "application/octet-stream"
    };
    let mut response = Response::new(Body::from(data));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(mime),
    );
    response
}
