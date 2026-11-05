pub mod account;
pub mod account_events;
pub mod account_sync;
pub mod auth;
pub mod browse;
pub mod download_v2;
mod downloads;
pub mod library;
pub mod playback;
mod playback_presence;
pub mod player;
pub mod server;
pub mod shuffle_sessions;
pub mod stats;
mod track_files;
pub mod user_data;
pub mod views;

#[cfg(test)]
mod catalogue_tests;

use crate::state::LibraryStatus;
use ::library::Library;
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};

use crate::http::{extract_bearer_token, extract_token};
use crate::http::{json_error, json_error_response};
use crate::state::{AppState, AuthContext, HealthResponse};

pub fn api_router(state: AppState) -> Router {
    let auth = Router::new()
        .route("/auth/opaque/start", post(crate::security::start))
        .route("/auth/opaque/finish", post(crate::security::finish))
        .layer(axum::extract::DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn(crate::security::deadline))
        .route("/auth/logout", post(auth::auth_logout));

    let protected = Router::new()
        .route("/account/state", get(account_events::snapshot))
        .route("/account/events", get(account_events::events))
        .route(
            "/account/shuffle-presets",
            get(account::presets).post(account::create_preset),
        )
        .route(
            "/account/shuffle-presets/:id",
            post(account::update_preset).delete(account::delete_preset),
        )
        .route(
            "/account/last-track",
            get(account::last_track).post(account::track_started),
        )
        .route(
            "/account/playback",
            get(playback::get)
                .post(playback::save)
                .delete(playback::clear),
        )
        .route("/account/playback/decks/:id", get(playback::deck_page))
        .route(
            "/account/playback/presence",
            post(playback_presence::report).layer(axum::extract::DefaultBodyLimit::max(1024)),
        )
        .route(
            "/account/sessions/revoke-others",
            post(account::revoke_others),
        )
        .route("/account/sessions/revoke-all", post(account::revoke_all))
        .route(
            "/account/collections/events",
            get(account_sync::collection_events),
        )
        .route(
            "/account/collections/snapshot",
            get(account_sync::collection_snapshot),
        )
        .route("/library/search", get(library::search))
        .route("/library/events", get(download_v2::metadata_events))
        .route(
            "/library/metadata/snapshots/:kind/:id",
            get(download_v2::metadata_snapshot),
        )
        .route("/library/shuffle", get(library::shuffle_tracks))
        .route(
            "/library/shuffle/sessions",
            post(shuffle_sessions::create_session),
        )
        .route(
            "/library/shuffle/sessions/:session_id",
            get(shuffle_sessions::get_page).delete(shuffle_sessions::delete_session),
        )
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
            "/library/playlists/:playlist_id/tracks",
            post(user_data::add_playlist_tracks).delete(user_data::remove_playlist_tracks),
        )
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
        .route("/download/jobs", post(download_v2::create_download_job))
        .route("/download/jobs", get(download_v2::list_download_jobs))
        .route("/download/jobs/:job_id", get(download_v2::get_download_job))
        .route(
            "/download/jobs/:job_id/tracks/:track_id/file",
            get(download_v2::download_prepared_file),
        )
        .route(
            "/download/jobs/:job_id/actions",
            post(download_v2::apply_download_job_action),
        )
        .route(
            "/download/jobs/:job_id/events",
            get(download_v2::download_job_events),
        )
        .route(
            "/download/tracks/:track_id/file",
            get(download_v2::download_track_file),
        )
        .route(
            "/stream/:track_id/original",
            get(track_files::stream_original_track),
        )
        .route("/browse/artists", get(browse::list_artists))
        .route(
            "/browse/artist-summaries",
            get(browse::list_artist_summaries),
        )
        .route("/browse/artists/:artist_id", get(browse::get_artist))
        .route(
            "/browse/artists/:artist_id/releases",
            get(browse::list_artist_releases),
        )
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
        .route("/stats/state", get(stats::counter_state))
        .route("/stats/snapshot", get(stats::counter_snapshot))
        .route("/stats/batch", post(stats::ingest_counters))
        .route("/stats/activate", post(stats::activate_counters))
        .route("/stats/sync", get(stats::sync_stats))
        .route("/stats/events", post(stats::ingest_stats))
        .route("/stats/preferences", post(stats::update_stats_preference))
        .route("/stats/reset", post(stats::reset_stats))
        .route("/player/settings", get(player::get_playback_settings))
        .route("/player/settings", post(player::update_playback_settings))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .route("/health", get(health))
        .route("/server/capabilities", get(download_v2::get_capabilities))
        .route("/server/ports", get(server::get_ports))
        .merge(auth::password_router())
        .merge(auth)
        .merge(protected)
        .with_state(state)
}

async fn require_auth(
    State(state): State<AppState>,
    mut req: axum::http::Request<Body>,
    next: Next,
) -> Response {
    if state.shutdown.is_cancelled() {
        return json_error_response(StatusCode::SERVICE_UNAVAILABLE, "server is shutting down");
    }
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
            // Cookie-authenticated console requests are not client presence.
            if let (Some(client_token), Some(connection)) = (
                extract_bearer_token(req.headers()),
                req.extensions().get::<crate::presence::HttpConnection>(),
            ) {
                state.auth.observe_connection(&client_token, connection);
            }
            let path = req
                .uri()
                .path()
                .strip_prefix("/api")
                .unwrap_or(req.uri().path());
            let artwork = path.starts_with("/library/artists/") && path.ends_with("/cover")
                || path.starts_with("/library/albums/") && path.ends_with("/cover");
            let catalogue = path.starts_with("/browse/artists")
                || path == "/browse/artist-summaries"
                || path == "/library/search";
            let audio_transfer = crate::streaming::is_http_audio_transfer(req.method(), path);
            let permit = if artwork || catalogue {
                let gate = if artwork {
                    &state.resources.artwork
                } else {
                    &state.resources.catalogue
                };
                match gate.acquire(&user.id, &state.shutdown).await {
                    Ok(permit) => Some(std::sync::Arc::new(permit)),
                    Err(error) => return error.response(),
                }
            } else {
                None
            };
            if let Some(permit) = &permit {
                req.extensions_mut().insert(std::sync::Arc::clone(permit));
            }
            req.extensions_mut().insert(AuthContext { user });
            let mut response = next.run(req).await;
            if audio_transfer {
                response = crate::streaming::track_http_audio_response(
                    response,
                    &state.process_metrics.network(),
                );
            }
            // The same endpoint URL can return a different account's personal data.
            if response
                .extensions()
                .get::<crate::artwork_cache::AuthenticatedArtwork>()
                .is_none()
            {
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("private, no-store"),
                );
            }
            match permit {
                Some(permit) => permit.retain_response(response),
                None => response,
            }
        }
        Ok(None) => json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
        Err(err) => json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("auth error: {}", err),
        ),
    }
}

async fn health(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    connection: Option<axum::Extension<crate::presence::HttpConnection>>,
) -> impl IntoResponse {
    // Keep anonymous reachability checks public. An authenticated health probe
    // also proves that an otherwise idle app is still connected.
    if let (Some(token), Some(axum::Extension(connection))) =
        (extract_bearer_token(&headers), connection)
    {
        if state
            .auth
            .user_from_token(&token)
            .is_ok_and(|user| user.is_some())
        {
            state.auth.observe_connection(&token, &connection);
        }
    }
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
