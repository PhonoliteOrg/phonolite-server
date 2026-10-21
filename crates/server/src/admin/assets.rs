// crates/server/src/admin/assets.rs
use axum::{
    body::Body,
    extract::Path as AxumPath,
    http::{header, HeaderValue, StatusCode},
    response::Response,
};
use std::path::Path;

use super::web_assets::WebAssets;
use crate::http::json_error_response;

pub async fn admin_asset(AxumPath(file): AxumPath<String>) -> Response {
    asset_response(&WebAssets::configured(), &file).await
}

async fn asset_response(assets: &WebAssets, file: &str) -> Response {
    if file.contains('\\') || file.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return json_error_response(StatusCode::FORBIDDEN, "forbidden");
    }
    let Some(asset) = assets.get(&format!("static/{file}")) else {
        return json_error_response(StatusCode::NOT_FOUND, "asset not found");
    };
    let data = match asset.bytes().await {
        Ok(data) => data,
        Err(_) => return json_error_response(StatusCode::NOT_FOUND, "asset not found"),
    };
    let mime = asset_content_type(file);
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
    use super::{asset_content_type, asset_response, WebAssets};
    use axum::{
        body::to_bytes,
        http::{header, StatusCode},
    };

    #[tokio::test]
    async fn serves_bundled_console_files_without_a_web_directory() {
        let assets = WebAssets::default();
        for (file, mime) in [
            ("css/admin.css", "text/css"),
            ("js/app.js", "text/javascript"),
            ("fonts/Poppins-Regular.ttf", "font/ttf"),
            ("img/phonolite-logo-nobackground.png", "image/png"),
            ("icons/lucide/x.svg", "image/svg+xml"),
        ] {
            let response = asset_response(&assets, file).await;
            assert_eq!(response.status(), StatusCode::OK, "{file}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], mime, "{file}");
            let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap();
            assert!(!body.is_empty(), "{file}");
            if file.ends_with(".png") {
                assert!(body.starts_with(b"\x89PNG\r\n\x1a\n"));
            }
        }
    }

    #[tokio::test]
    async fn missing_assets_and_template_traversal_are_rejected() {
        let assets = WebAssets::default();
        for file in ["js/missing.js", "templates/login.html", "config.yaml"] {
            assert_eq!(
                asset_response(&assets, file).await.status(),
                StatusCode::NOT_FOUND,
                "{file}"
            );
        }
        for file in [
            "../templates/login.html",
            "/js/app.js",
            "js/../../config.yaml",
            "js\\app.js",
            "js/./app.js",
        ] {
            assert_eq!(
                asset_response(&assets, file).await.status(),
                StatusCode::FORBIDDEN,
                "{file}"
            );
        }
    }

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
