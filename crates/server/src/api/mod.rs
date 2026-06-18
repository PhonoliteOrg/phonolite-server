pub mod auth;
pub mod browse;
pub mod download_v2;
pub mod library;
pub mod player;
pub mod server;
pub mod stats;
pub mod user_data;
pub mod views;

use crate::state::LibraryStatus;
use ::library::Library;
use axum::{
    body::Body,
    extract::State,
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};

use crate::admin::extract_token;
use crate::state::{AppState, AuthContext, HealthResponse};
use crate::utils::{json_error, json_error_response};

pub fn api_router(state: AppState) -> Router {
    let auth = Router::new()
        .route("/auth/login", post(auth::auth_login))
        .route("/auth/logout", post(auth::auth_logout));

    let protected = Router::new()
        .route("/library/search", get(library::search))
        .route("/library/v2/events", get(download_v2::metadata_events))
        .route(
            "/library/v2/metadata/snapshots/:kind/:id",
            get(download_v2::metadata_snapshot),
        )
        .route("/library/shuffle", get(library::shuffle_tracks))
        .route("/library/metadata-events", get(library::metadata_events))
        .route("/library/albums/:album_id", get(library::get_album))
        .route(
            "/library/tracks/:track_id/offline-metadata",
            get(library::get_offline_track_metadata),
        )
        .route("/library/match-tracks", post(library::match_tracks))
        .route("/library/playlists", get(user_data::list_playlists))
        .route("/library/playlists", post(user_data::create_playlist))
        .route(
            "/library/playlists/:playlist_id",
            post(user_data::update_playlist),
        )
        .route(
            "/library/playlists/:playlist_id",
            axum::routing::delete(user_data::delete_playlist),
        )
        .route(
            "/library/playlists/:playlist_id/cover",
            get(user_data::get_playlist_cover),
        )
        .route(
            "/library/playlists/:playlist_id/cover",
            put(user_data::upload_playlist_cover),
        )
        .route(
            "/library/playlists/:playlist_id/cover",
            axum::routing::delete(user_data::delete_playlist_cover),
        )
        .route("/library/likes/batch", post(user_data::batch_update_likes))
        .route("/library/likes/:track_id", post(user_data::add_like))
        .route(
            "/library/likes/:track_id",
            axum::routing::delete(user_data::remove_like),
        )
        .route("/download/batches", post(library::create_download_batch))
        .route("/download/tracks/:track_id", get(library::download_track))
        .route("/download/v2/jobs", post(download_v2::create_download_job))
        .route("/download/v2/jobs", get(download_v2::list_download_jobs))
        .route(
            "/download/v2/jobs/:job_id",
            get(download_v2::get_download_job),
        )
        .route(
            "/download/v2/jobs/:job_id/actions",
            post(download_v2::apply_download_job_action),
        )
        .route(
            "/download/v2/jobs/:job_id/events",
            get(download_v2::download_job_events),
        )
        .route(
            "/download/v2/tracks/:track_id/file",
            get(download_v2::download_track_file),
        )
        .route("/browse/artists", get(browse::list_artists))
        .route("/browse/artists/:artist_id", get(browse::get_artist))
        .route(
            "/browse/artists/:artist_id/albums",
            get(browse::list_artist_albums),
        )
        .route(
            "/browse/albums/:album_id/tracks",
            get(browse::list_album_tracks),
        )
        .route("/browse/tracks/:track_id", get(browse::get_track))
        .route(
            "/browse/playlists/:playlist_id/tracks",
            get(browse::list_playlist_tracks),
        )
        .route("/browse/likes", get(browse::list_liked_tracks))
        .route(
            "/library/artists/:artist_id/cover",
            get(library::get_artist_cover),
        )
        .route(
            "/library/albums/:album_id/cover",
            get(library::get_album_cover),
        )
        .route("/stats", get(stats::get_stats))
        .route("/player/settings", get(player::get_playback_settings))
        .route("/player/settings", post(player::update_playback_settings))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .route("/health", get(health))
        .route("/server/capabilities", get(download_v2::get_capabilities))
        .route("/server/ports", get(server::get_ports))
        .merge(auth)
        .merge(protected)
        .with_state(state)
}

async fn require_auth(
    State(state): State<AppState>,
    mut req: axum::http::Request<Body>,
    next: Next,
) -> Response {
    let has_users = match state.auth.has_any_user() {
        Ok(b) => b,
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth db error: {}", err),
            );
        }
    };
    if !has_users {
        return json_error_response(StatusCode::SERVICE_UNAVAILABLE, "server not initialized");
    }

    let token = match extract_token(req.headers()) {
        Some(token) => token,
        None => return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
    };

    match state.auth.user_from_token(&token) {
        Ok(Some(user)) => {
            req.extensions_mut().insert(AuthContext { user });
            next.run(req).await
        }
        Ok(None) => json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
        Err(err) => json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("auth error: {}", err),
        ),
    }
}

async fn health() -> impl IntoResponse {
    Json(HealthResponse { status: "ok" })
}

pub(crate) fn library_or_json_error(
    state: &AppState,
) -> Result<Library, (StatusCode, Json<crate::state::ErrorResponse>)> {
    let guard = state.library_state.read();
    if let Some(library) = guard.library.clone() {
        Ok(library)
    } else {
        Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            library_status_message(&guard.status),
        ))
    }
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

fn library_status_message(status: &LibraryStatus) -> String {
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
