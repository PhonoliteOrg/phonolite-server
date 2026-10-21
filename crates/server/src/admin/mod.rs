// crates/server/src/admin/mod.rs
pub(crate) mod access;
pub mod assets;
pub mod auth;
pub(crate) mod backup;
mod console;
pub mod library;
pub mod logs;
pub(crate) mod presentation;
pub mod settings;
pub mod users;
mod web_assets;

use crate::state::LibraryStatus;
use ::library::Library;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    middleware,
    response::Response,
    routing::{get, post},
    Router,
};

use crate::admin::presentation::html_error;
use crate::auth::{AuthError, AuthUser, UserRole};
use crate::http::{json_error_response, redirect_to, request_authority};
use crate::state::AppState;
use access::AdminAccess;

pub fn admin_router(state: AppState, address: std::net::SocketAddr) -> Router {
    // Address changes are pending until restart. Check the actual listener,
    // rather than the editable configuration.
    let access = AdminAccess::new(address);
    Router::new()
        .route("/", get(admin_home))
        .route("/assets/*file", get(assets::admin_asset))
        .route("/actions/restart", post(settings::admin_restart))
        .route("/actions/shutdown", post(settings::admin_shutdown))
        .merge(settings::security_router())
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
        .route("/logs", get(logs::admin_logs))
        .route("/logs/clear", post(logs::admin_logs_clear))
        .route("/logs/tail", get(logs::admin_logs_tail))
        .route("/logs/issues", get(logs::admin_logs_issues))
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
        .route("/settings/folders", get(settings::admin_browse_folders))
        .route(
            "/settings/music-roots/add",
            post(settings::admin_add_music_root),
        )
        .route(
            "/settings/music-roots/:root_id/update",
            post(settings::admin_update_music_root),
        )
        .route(
            "/settings/music-roots/:root_id/delete",
            post(settings::admin_delete_music_root),
        )
        .route(
            "/settings/logging/debug",
            post(settings::admin_toggle_debug_logs),
        )
        .route("/settings/reindex", post(library::admin_reindex))
        .route("/settings/scan", post(library::admin_scan))
        .route("/settings/backup/export", post(backup::export))
        .route("/settings/backup/jobs", get(backup::list_jobs))
        .route("/settings/backup/jobs/:id", get(backup::job_status))
        .route("/settings/backup/jobs/:id/remove", post(backup::remove_job))
        .route(
            "/settings/backup/jobs/:id/restore",
            post(backup::restore_job),
        )
        .route(
            "/settings/backup/jobs/:id/parts/:index",
            get(backup::download_part),
        )
        .route("/settings/backup/limits", get(backup::limits))
        .route("/settings/backup/preview", post(backup::preview))
        .route("/settings/backup/restore", post(backup::restore))
        .route("/library", get(library::admin_library))
        .route("/covers/albums/:album_id", get(library::admin_album_cover))
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
        .layer(middleware::from_fn(console::console_response))
        .layer(middleware::from_fn_with_state(access, admin_origin))
        .with_state(state)
}

// Binding an interface alone does not stop DNS rebinding or cross-site forms.
async fn admin_origin(
    State(access): State<AdminAccess>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let Some((host, port)) = request_authority(&request, 443) else {
        return json_error_response(StatusCode::FORBIDDEN, "valid administration host required");
    };
    if !access.allows(&host, port) {
        return json_error_response(
            StatusCode::FORBIDDEN,
            "configured administration host required",
        );
    }
    if request.method() != axum::http::Method::GET && request.method() != axum::http::Method::HEAD {
        let origin = request
            .headers()
            .get(axum::http::header::ORIGIN)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        let with_port = format!("https://{host}:{port}");
        let default_port_origin = format!("https://{host}");
        if origin != with_port && !(port == 443 && origin == default_port_origin) {
            return json_error_response(
                StatusCode::FORBIDDEN,
                "same-origin administration required",
            );
        }
    }
    next.run(request).await
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
    redirect_to("/library")
}

use crate::http::extract_session_cookie;

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

#[cfg(test)]
mod admin_origin_tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header, Method, Request},
        routing::any,
    };
    use tower::ServiceExt;

    fn protected(address: &str) -> Router {
        Router::new()
            .route("/", any(|| async { StatusCode::NO_CONTENT }))
            .layer(middleware::from_fn_with_state(
                AdminAccess::new(address.parse().unwrap()),
                admin_origin,
            ))
    }

    async fn response_status(
        app: Router,
        method: Method,
        host: &str,
        origin: Option<&str>,
    ) -> StatusCode {
        let mut request = Request::builder()
            .method(method)
            .uri("/")
            .header(header::HOST, host);
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        app.oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn origin_checks_use_the_active_listener_port_for_reads_and_writes() {
        // Exercise only the middleware with in-process requests; no listeners
        // or real server actions are involved.
        let app = protected("127.0.0.1:4002");
        for (method, host, origin, expected) in [
            (Method::GET, "localhost:4002", None, StatusCode::NO_CONTENT),
            (Method::GET, "127.0.0.1:4002", None, StatusCode::NO_CONTENT),
            (
                Method::POST,
                "localhost:4002",
                Some("https://localhost:4002"),
                StatusCode::NO_CONTENT,
            ),
            (
                Method::POST,
                "127.0.0.1:4002",
                Some("https://127.0.0.1:4002"),
                StatusCode::NO_CONTENT,
            ),
            (Method::GET, "localhost:3002", None, StatusCode::FORBIDDEN),
            (Method::GET, "localhost:4003", None, StatusCode::FORBIDDEN),
            (
                Method::GET,
                "evil.example:4002",
                None,
                StatusCode::FORBIDDEN,
            ),
            (
                Method::POST,
                "localhost:4002",
                Some("https://localhost:4003"),
                StatusCode::FORBIDDEN,
            ),
            (
                Method::POST,
                "localhost:4002",
                Some("http://localhost:4002"),
                StatusCode::FORBIDDEN,
            ),
            (Method::POST, "localhost:4002", None, StatusCode::FORBIDDEN),
        ] {
            assert_eq!(
                response_status(app.clone(), method, host, origin).await,
                expected,
                "host={host} origin={origin:?}"
            );
        }
    }

    #[tokio::test]
    async fn admin_access_accepts_selected_ipv4_and_ipv6_bindings_with_same_origin_writes() {
        for (address, method, host, origin, allowed) in [
            (
                "192.168.1.10:4002",
                Method::GET,
                "192.168.1.10:4002",
                None,
                true,
            ),
            (
                "192.168.1.10:4002",
                Method::POST,
                "192.168.1.10:4002",
                Some("https://192.168.1.10:4002"),
                true,
            ),
            (
                "192.168.1.10:4002",
                Method::GET,
                "192.168.1.11:4002",
                None,
                false,
            ),
            (
                "192.168.1.10:4002",
                Method::GET,
                "localhost:4002",
                None,
                false,
            ),
            (
                "0.0.0.0:4002",
                Method::POST,
                "192.168.1.10:4002",
                Some("https://192.168.1.10:4002"),
                true,
            ),
            (
                "0.0.0.0:4002",
                Method::POST,
                "192.168.1.10:4002",
                Some("https://192.168.1.11:4002"),
                false,
            ),
            (
                "0.0.0.0:4002",
                Method::POST,
                "192.168.1.10:4002",
                Some("http://192.168.1.10:4002"),
                false,
            ),
            (
                "0.0.0.0:4002",
                Method::GET,
                "evil.example:4002",
                None,
                false,
            ),
            ("0.0.0.0:4002", Method::GET, "[::1]:4002", None, false),
            (
                "[fd00::10]:4002",
                Method::POST,
                "[fd00::10]:4002",
                Some("https://[fd00::10]:4002"),
                true,
            ),
            (
                "[fd00::10]:4002",
                Method::GET,
                "[fd00::11]:4002",
                None,
                false,
            ),
            ("[::1]:4002", Method::GET, "localhost:4002", None, true),
            (
                "[::]:4002",
                Method::POST,
                "[fd00::10]:4002",
                Some("https://[fd00::10]:4002"),
                true,
            ),
            ("[::]:4002", Method::GET, "192.168.1.10:4002", None, true),
            (
                "0.0.0.0:443",
                Method::POST,
                "192.168.1.10",
                Some("https://192.168.1.10"),
                true,
            ),
            (
                "0.0.0.0:443",
                Method::POST,
                "192.168.1.10:443",
                Some("https://192.168.1.10"),
                true,
            ),
        ] {
            let expected = if allowed {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::FORBIDDEN
            };
            assert_eq!(
                response_status(protected(address), method, host, origin).await,
                expected,
                "bind={address} host={host} origin={origin:?}"
            );
        }
    }

    #[tokio::test]
    async fn admin_access_rejects_ambiguous_host_headers() {
        let request = Request::builder()
            .uri("/")
            .header(header::HOST, "192.168.1.10:4002")
            .header(header::HOST, "192.168.1.11:4002")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            protected("0.0.0.0:4002")
                .oneshot(request)
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
}
