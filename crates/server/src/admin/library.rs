//! Authenticated Library administration endpoints and scan/cover actions.
mod view;

use super::auth::{admin_login_page, admin_setup_page};
use super::console::wants_console_view;
use super::{admin_user_from_headers, is_admin, library_for_admin, library_or_response};
use crate::admin::presentation::{html_error, url_escape};
use crate::assets::{
    clear_metadata_assets, cover_response, fetch_cover, fetch_cover_cached, metadata_root_path,
    resolve_artist_banner_source, resolve_artist_cover_source, resolve_artist_logo_source,
    resolve_cover_source, CoverCacheKey,
};
use crate::http::{json_error_response, redirect_to, wants_json};
use crate::scan::{start_cover_sweep, start_enrichment_sweep, start_rescan};
use crate::state::LibraryStatus;
use crate::state::{
    AdminLibraryQuery, AlbumMetadataStatus, AppState, ArtistCoverQuery, LibraryStatusResponse,
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use tracing::warn;
use view::admin_library_page;

pub async fn admin_library_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        return json_error_response(StatusCode::FORBIDDEN, "forbidden");
    }

    Json(library_status_snapshot(&state.library_state).await).into_response()
}

async fn library_status_snapshot(
    library_state: &parking_lot::RwLock<crate::state::LibraryState>,
) -> LibraryStatusResponse {
    let (mut response, library, metadata_work) = {
        let guard = library_state.read();
        (
            library_status_response(&guard.status),
            guard.library.clone(),
            guard.metadata_work.clone(),
        )
    };
    if response.status == "ready" {
        if let Some(library) = library {
            match tokio::task::spawn_blocking(move || library.album_metadata_stats()).await {
                Ok(Ok(stats)) => {
                    let progress = metadata_work.snapshot();
                    response.metadata = Some(AlbumMetadataStatus {
                        phase: progress.as_ref().map(|progress| progress.phase),
                        completed: stats.completed,
                        total: stats.total,
                        progress,
                    })
                }
                Ok(Err(err)) => warn!("Failed to read album metadata status: {}", err),
                Err(err) => warn!("Album metadata status task failed: {}", err),
            }
        }
    }
    response
}

fn library_status_response(library_status: &LibraryStatus) -> LibraryStatusResponse {
    let (status, message, artists, albums, tracks) = match library_status {
        LibraryStatus::Unconfigured => (
            "unconfigured".to_string(),
            Some("music directory must be set".to_string()),
            None,
            None,
            None,
        ),
        LibraryStatus::Missing(path) => (
            "missing".to_string(),
            Some(format!("music directory not found: {}", path.display())),
            None,
            None,
            None,
        ),
        LibraryStatus::Scanning { started, .. } => {
            let since = started
                .elapsed()
                .map(|elapsed: std::time::Duration| {
                    format!("library scan in progress ({}s)", elapsed.as_secs())
                })
                .unwrap_or_else(|_| "library scan in progress".to_string());
            ("scanning".to_string(), Some(since), None, None, None)
        }
        LibraryStatus::Ready(stats) => (
            "ready".to_string(),
            None,
            Some(stats.artists),
            Some(stats.albums),
            Some(stats.tracks),
        ),
        LibraryStatus::Error(message) => {
            ("error".to_string(), Some(message.clone()), None, None, None)
        }
    };

    let progress = match library_status {
        LibraryStatus::Scanning { progress, .. } => Some(*progress.read()),
        _ => None,
    };
    LibraryStatusResponse {
        status,
        message,
        artists,
        albums,
        tracks,
        progress,
        metadata: None,
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;
    use library::{LibraryStats, ScanProgress};
    use std::sync::Arc;

    #[tokio::test]
    async fn ready_library_reports_stored_metadata_and_keeps_indexing_and_enrichment_independent() {
        let db = Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        let library = library::Library::load_or_scan_with_db(Vec::new(), db)
            .unwrap()
            .0;
        let work = Arc::new(crate::metadata_progress::MetadataWork::default());
        let job = work.start(crate::metadata_progress::MetadataPhase::Artists);
        job.set_total(3);
        drop(job.item("Artist", None));
        let item = job.item("Björk", None);
        let state = parking_lot::RwLock::new(crate::state::LibraryState {
            status: LibraryStatus::Ready(library.stats().unwrap()),
            library: Some(library),
            metadata_work: work,
        });
        let response = library_status_snapshot(&state).await;
        assert_eq!(
            response.status, "ready",
            "metadata work does not make the indexed catalog unavailable"
        );
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(
            json["metadata"],
            serde_json::json!({"phase": "artists", "completed": 0, "total": 0,
                "progress": {"phase": "artists", "checked": 1, "total": 3,
                    "current": {"artist": "Björk", "album": null}}})
        );
        assert!(json["progress"].is_null());
        drop(item);
        let json = serde_json::to_value(library_status_snapshot(&state).await).unwrap();
        assert!(json["metadata"]["progress"]["current"].is_null());
        drop(job);
        let json = serde_json::to_value(library_status_snapshot(&state).await).unwrap();
        assert_eq!(
            json["metadata"],
            serde_json::json!({"phase": null, "completed": 0, "total": 0, "progress": null})
        );
        state.write().status = LibraryStatus::Scanning {
            started: std::time::SystemTime::now(),
            progress: Arc::new(parking_lot::RwLock::new(ScanProgress::Discovering)),
        };
        assert!(library_status_snapshot(&state).await.metadata.is_none());
    }

    #[test]
    fn status_response_exposes_live_scan_counts_and_clears_them_when_finished() {
        let progress = Arc::new(parking_lot::RwLock::new(ScanProgress::IndexingAlbums {
            completed: 2,
            total: 5,
        }));
        let status = LibraryStatus::Scanning {
            started: std::time::SystemTime::now(),
            progress: Arc::clone(&progress),
        };
        let response = serde_json::to_value(library_status_response(&status)).unwrap();
        assert_eq!(response["status"], "scanning");
        assert_eq!(
            response["progress"],
            serde_json::json!({"phase": "indexing_albums", "completed": 2, "total": 5})
        );
        *progress.write() = ScanProgress::IndexingAlbums {
            completed: 4,
            total: 5,
        };
        assert_eq!(
            library_status_response(&status).progress,
            Some(ScanProgress::IndexingAlbums {
                completed: 4,
                total: 5
            })
        );

        let ready = library_status_response(&LibraryStatus::Ready(LibraryStats {
            artists: 1,
            albums: 5,
            tracks: 20,
        }));
        assert_eq!(ready.albums, Some(5));
        assert_eq!(ready.progress, None);
        assert_eq!(
            library_status_response(&LibraryStatus::Error("Unreadable folder".into())).progress,
            None
        );
    }
}

pub async fn admin_reindex(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

    let error = match library_for_admin(&state) {
        Ok(library) => {
            if let Err(err) = clear_metadata_assets(&state).await {
                warn!("Failed to clear metadata on reindex: {}", err);
            }
            start_rescan(state.clone(), library, true);
            None
        }
        Err(message) => Some(message),
    };

    if let Some(error) = error {
        if wants_json(&headers) {
            json_error_response(StatusCode::SERVICE_UNAVAILABLE, error)
        } else {
            redirect_to(&format!("/settings?message={}", url_escape(&error)))
        }
    } else if wants_json(&headers) {
        (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "ok": true, "status": "indexing", "message": "Reindex started. Check status below for progress." })),
        )
            .into_response()
    } else {
        redirect_to("/settings")
    }
}

pub async fn admin_library(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AdminLibraryQuery>,
) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        if wants_console_view(&headers) {
            return redirect_to("/setup");
        }
        return admin_setup_page(&state, None);
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => {
            if wants_console_view(&headers) {
                return redirect_to("/login");
            }
            return admin_login_page(&state, None);
        }
        Err(err) => {
            return html_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        if wants_console_view(&headers) {
            return html_error(
                &state,
                StatusCode::FORBIDDEN,
                "admin access required".to_string(),
            );
        }
        return admin_login_page(&state, Some("admin access required".to_string()));
    }
    admin_library_page(&state, query, None, &headers)
}

pub async fn admin_album_cover(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(album_id): AxumPath<String>,
) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        return json_error_response(StatusCode::FORBIDDEN, "forbidden");
    }

    let library = match library_or_response(&state) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let album = match library.get_album(&album_id) {
        Ok(Some(album)) => album,
        Ok(None) => return json_error_response(StatusCode::NOT_FOUND, "album not found"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            )
        }
    };
    let cover_ref = match album.cover_ref {
        Some(cover) => cover,
        None => return json_error_response(StatusCode::NOT_FOUND, "cover not found"),
    };
    let source = match resolve_cover_source(&library, &cover_ref) {
        Ok(Some(source)) => source,
        Ok(None) => return json_error_response(StatusCode::NOT_FOUND, "cover not found"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            )
        }
    };

    let key = CoverCacheKey::Album(album_id.clone());
    match fetch_cover_cached(&state, key, source).await {
        Ok((bytes, mime)) => cover_response(bytes, &mime),
        Err(err) => json_error_response(StatusCode::NOT_FOUND, err),
    }
}

pub async fn admin_artist_cover(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ArtistCoverQuery>,
    AxumPath(artist_id): AxumPath<String>,
) -> Response {
    if !state.auth.has_admin().unwrap_or(false) {
        return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let user = match admin_user_from_headers(&state, &headers) {
        Ok(Some(user)) => user,
        Ok(None) => return json_error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("auth error: {}", err),
            )
        }
    };
    if !is_admin(&user) {
        return json_error_response(StatusCode::FORBIDDEN, "forbidden");
    }

    let library = match library_or_response(&state) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let metadata_root = metadata_root_path(&state);
    let source = match query.kind.as_deref() {
        Some("logo") => resolve_artist_logo_source(&library, &metadata_root, &artist_id),
        Some("banner") => resolve_artist_banner_source(&library, &metadata_root, &artist_id),
        _ => resolve_artist_cover_source(&library, &metadata_root, &artist_id),
    };
    let source = match source {
        Ok(Some(source)) => source,
        Ok(None) => return json_error_response(StatusCode::NOT_FOUND, "cover not found"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            )
        }
    };

    match fetch_cover(source).await {
        Ok((bytes, mime)) => cover_response(bytes, &mime),
        Err(err) => json_error_response(StatusCode::NOT_FOUND, err),
    }
}

pub async fn admin_scan(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

    let error = match library_for_admin(&state) {
        Ok(library) => {
            start_enrichment_sweep(state.clone(), library.clone(), false);
            start_cover_sweep(state.clone(), library);
            None
        }
        Err(message) => Some(message),
    };

    if let Some(error) = error {
        if wants_json(&headers) {
            json_error_response(StatusCode::SERVICE_UNAVAILABLE, error)
        } else {
            redirect_to(&format!("/settings?message={}", url_escape(&error)))
        }
    } else if wants_json(&headers) {
        (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "ok": true, "status": "refreshing", "message": "Scan started. Missing metadata will be refreshed in the background." })),
        )
            .into_response()
    } else {
        redirect_to("/settings")
    }
}
