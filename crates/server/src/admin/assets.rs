// crates/server/src/admin/assets.rs
use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, HeaderValue, StatusCode},
    response::Response,
};
use std::path::{Component, Path, PathBuf};

use crate::admin::presentation::web_root;
use crate::http::json_error_response;
use crate::state::AppState;

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
    let mime = asset_content_type(&file);
    let mut response = Response::new(Body::from(data));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    response
}

fn asset_content_type(file: &str) -> &'static str {
    let extension = Path::new(file)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "js" => "text/javascript",
        "css" => "text/css",
        "ttf" => "font/ttf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::asset_content_type;

    #[test]
    fn browser_assets_use_their_font_and_image_content_types() {
        for (path, expected) in [
            ("js/app.js", "text/javascript"),
            ("css/app.css", "text/css"),
            ("fonts/Inter.ttf", "font/ttf"),
            ("fonts/Inter.woff", "font/woff"),
            ("fonts/Inter.woff2", "font/woff2"),
            ("img/logo.svg", "image/svg+xml"),
            ("img/logo.PNG", "image/png"),
            ("img/favicon.ico", "image/x-icon"),
            ("img/cover.jpg", "image/jpeg"),
            ("img/cover.webp", "image/webp"),
            ("unknown.bin", "application/octet-stream"),
        ] {
            assert_eq!(asset_content_type(path), expected, "{path}");
        }
    }
}
