// crates/server/src/admin/mod.rs
pub mod assets;
pub mod auth;
pub mod activity;
pub mod library;
pub mod settings;
pub mod users;

use std::time::Duration;

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
    routing::{get, post},
    Router,
};
use library::Library;
use crate::state::LibraryStatus;

use crate::auth::{AuthError, AuthUser, SessionToken, UserRole};
use crate::state::AppState;
use crate::utils::{escape_html, html_error, json_error_response, redirect_to};

pub fn admin_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(admin_home))
        .route("/assets/*file", get(assets::admin_asset))
        .route("/actions/restart", post(settings::admin_restart))
        .route("/actions/shutdown", post(settings::admin_shutdown))
        .route(
            "/setup",
            get(auth::admin_setup_form).post(auth::admin_setup_submit),
        )
        .route(
            "/login",
            get(auth::admin_login_form).post(auth::admin_login_submit),
        )
        .route("/logout", post(auth::admin_logout))
        .route(
            "/settings",
            get(settings::admin_settings).post(settings::admin_update_settings),
        )
        .route("/activity", get(activity::admin_activity))
        .route("/activity/clear", post(activity::admin_activity_clear))
        .route("/status/activity", get(activity::admin_activity_status))
        .route("/status/library", get(library::admin_library_status))
        .route(
            "/settings/metadata/add",
            post(settings::admin_add_metadata_source),
        )
        .route(
            "/settings/metadata/:source_id/update",
            post(settings::admin_update_metadata_source),
        )
        .route(
            "/settings/metadata/:source_id/delete",
            post(settings::admin_delete_metadata_source),
        )
        .route(
            "/settings/metadata/:source_id/toggle",
            post(settings::admin_toggle_metadata_source),
        )
        .route(
            "/settings/metadata/test",
            post(settings::admin_test_metadata_source),
        )
        .route("/settings/reindex", post(library::admin_reindex))
        .route("/settings/scan", post(library::admin_scan))
        .route("/library", get(library::admin_library))
        .route(
            "/covers/albums/:album_id",
            get(library::admin_album_cover),
        )
        .route(
            "/covers/artists/:artist_id",
            get(library::admin_artist_cover),
        )
        .route(
            "/users",
            get(users::admin_users).post(users::admin_create_user),
        )
        .route("/users/bulk-delete", post(users::admin_bulk_delete))
        .route("/users/:user_id/update", post(users::admin_update_user))
        .route("/users/:user_id/password", post(users::admin_set_password))
        .route("/users/:user_id/delete", post(users::admin_delete_user))
        .with_state(state)
}

async fn admin_home(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let has_admin = match state.auth.has_admin() {
        Ok(b) => b,
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth db error: {}", err),
            );
        }
    };
    if !has_admin {
        return redirect_to("/setup");
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return redirect_to("/login"),
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        return html_error(&state, StatusCode::FORBIDDEN, "forbidden".to_string());
    }
    users::admin_users_page(&state, &user, None)
}

pub fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        if let Ok(value) = value.to_str() {
            if let Some(token) = value.strip_prefix("Bearer ") {
                return Some(token.trim().to_string());
            }
        }
    }
    extract_session_cookie(headers)
}

pub fn extract_session_cookie(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    parse_cookie_value(cookie, "phonolite_session")
}

fn parse_cookie_value(cookie: &str, name: &str) -> Option<String> {
    for part in cookie.split(';') {
        let part = part.trim();
        let mut iter = part.splitn(2, '=');
        let key = iter.next()?.trim();
        let value = iter.next()?.trim();
        if key == name && !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

pub fn admin_user_from_headers(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<AuthUser>, AuthError> {
    let token = match extract_session_cookie(headers) {
        Some(token) => token,
        None => return Ok(None),
    };
    state.auth.user_from_token(&token)
}

pub fn session_cookie_header(session: &SessionToken, ttl: Duration) -> HeaderValue {
    let value = format!(
        "phonolite_session={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        session.token,
        ttl.as_secs()
    );
    HeaderValue::from_str(&value).unwrap_or_else(|_| {
        HeaderValue::from_static("phonolite_session=invalid; Path=/; HttpOnly; SameSite=Strict")
    })
}

pub fn clear_session_cookie() -> HeaderValue {
    HeaderValue::from_static("phonolite_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

pub fn is_admin(user: &AuthUser) -> bool {
    matches!(user.role, UserRole::Admin | UserRole::SuperAdmin)
}

pub(crate) fn library_or_response(state: &AppState) -> Result<Library, Response> {
    let guard = state.library_state.read();
    if let Some(library) = guard.library.clone() {
        Ok(library)
    } else {
        Err(json_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            library_status_message(&guard.status),
        ))
    }
}

pub(crate) fn library_for_admin(state: &AppState) -> Result<Library, String> {
    let guard = state.library_state.read();
    if let Some(library) = guard.library.clone() {
        Ok(library)
    } else {
        Err(library_status_message(&guard.status))
    }
}

pub(crate) fn library_status_message(status: &LibraryStatus) -> String {
    match status {
        LibraryStatus::Unconfigured => "music directory must be set".to_string(),
        LibraryStatus::Missing(path) => {
            format!("music directory not found: {}", path.display())
        }
        LibraryStatus::Scanning { .. } => "library indexing in progress".to_string(),
        LibraryStatus::Ready(_) => "library ready".to_string(),
        LibraryStatus::Error(message) => format!("library error: {}", message),
    }
}

pub(crate) fn render_message(message: Option<String>) -> String {
    match message {
        Some(message) => {
            let trimmed = message.trim_start();
            let (class, text) = if let Some(rest) = trimmed.strip_prefix("info:") {
                ("msg", rest.trim())
            } else if let Some(rest) = trimmed.strip_prefix("ok:") {
                ("msg", rest.trim())
            } else if let Some(rest) = trimmed.strip_prefix("success:") {
                ("msg", rest.trim())
            } else {
                ("msg error", trimmed)
            };
            format!(
                r#"<div class="{}" data-autohide><span>{}</span><button type="button" class="msg-close" data-close-message>x</button></div>"#,
                class,
                escape_html(text)
            )
        }
        None => String::new(),
    }
}
