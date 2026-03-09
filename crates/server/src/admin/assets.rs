// crates/server/src/admin/assets.rs
use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, HeaderValue, StatusCode},
    response::Response,
};
use std::path::{Component, PathBuf};

use crate::state::AppState;
use crate::utils::{json_error_response, web_root};

pub async fn admin_asset(
    State(state): State<AppState>,
    AxumPath(file): AxumPath<String>,
) -> Response {
    let static_root = web_root(&state).join("static");
    let mut relpath = PathBuf::new();
    for component in PathBuf::from(&file).components() {
        match component {
            Component::Normal(value) => relpath.push(value),
            _ => return json_error_response(StatusCode::FORBIDDEN, "forbidden"),
        }
    }
    let path = static_root.join(&relpath);
    if !path.starts_with(&static_root) {
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
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    response
}
