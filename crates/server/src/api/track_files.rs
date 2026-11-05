use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};

use crate::download_files::{self, FileDelivery};
use crate::http::json_error_response;
use crate::state::AppState;

use super::library_or_response;

/// Original playback uses the authenticated library source without transcoding.
/// Axum's GET route also handles HEAD, retaining metadata with an empty body.
pub async fn stream_original_track(
    State(state): State<AppState>,
    Path(track_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    track_file_response(&state, &track_id, &headers, FileDelivery::OriginalStream).await
}

pub(crate) async fn track_file_response(
    state: &AppState,
    track_id: &str,
    headers: &HeaderMap,
    delivery: FileDelivery,
) -> Response {
    let library = match library_or_response(state) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let track = match library.get_track(track_id) {
        Ok(Some(track)) => track,
        Ok(None) => return json_error_response(StatusCode::NOT_FOUND, "track not found"),
        Err(err) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            )
        }
    };
    let path = match library.resolve_relpath(&track.file_relpath) {
        Some(path) => path,
        None => {
            return json_error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "music root not configured",
            )
        }
    };

    download_files::track_file_response(&track, &path, headers, delivery, &state.shutdown).await
}
