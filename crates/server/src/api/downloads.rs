//! Legacy download and offline-metadata HTTP endpoints.
use super::{
    library_or_json_error, library_or_response,
    views::{liked_set, playlist_set},
};
use crate::download_files::{
    checksum_for_file, download_content_disposition, download_content_type, download_etag,
    percent_encode_path_component, range_error_response,
};
use crate::http::{json_error, json_error_response};
use crate::offline_metadata::{
    offline_metadata_for_track, OfflineTrackMetadata, OFFLINE_METADATA_SCHEMA_VERSION,
};
use crate::range::{parse_range_header, ByteRange};
use crate::state::{AppState, JsonResult};
use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::UNIX_EPOCH;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

#[derive(Deserialize)]
pub struct DownloadBatchRequest {
    pub track_ids: Vec<String>,
    pub client_batch_id: Option<String>,
}

#[derive(Serialize)]
pub struct DownloadBatchResponse {
    pub schema_version: u32,
    pub batch_id: String,
    pub created_at: u64,
    pub items: Vec<DownloadBatchItem>,
    pub unavailable: Vec<DownloadBatchUnavailable>,
}

#[derive(Serialize)]
pub struct DownloadBatchItem {
    pub track_id: String,
    pub download_url: String,
    pub offline_metadata: OfflineTrackMetadata,
    pub byte_length: u64,
    pub content_type: String,
    pub etag: String,
    pub sha256: String,
}

#[derive(Serialize)]
pub struct DownloadBatchUnavailable {
    pub track_id: String,
    pub reason: String,
}

pub async fn download_track(
    State(state): State<AppState>,
    AxumPath(track_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let library = match library_or_response(&state) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let track = match library.get_track(&track_id) {
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
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return json_error_response(StatusCode::NOT_FOUND, "track file not found"),
        Err(_) => return json_error_response(StatusCode::NOT_FOUND, "track file not found"),
    };
    let size = metadata.len();
    let etag = download_etag(&track, size);
    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let if_range_matches = headers
        .get(header::IF_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim() == etag.as_str())
        .unwrap_or(true);
    let range = match range_header.filter(|_| if_range_matches) {
        Some(value) => match parse_range_header(value, size) {
            Ok(range) => Some(range),
            Err(err) => return range_error_response(err, size),
        },
        None => None,
    };

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => return json_error_response(StatusCode::NOT_FOUND, "track file not found"),
    };

    let (status, start, end, content_len) = match range {
        Some(ByteRange { start, end }) => {
            if let Err(err) = file.seek(std::io::SeekFrom::Start(start)).await {
                return json_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("file seek failed: {}", err),
                );
            }
            let len = end.saturating_sub(start).saturating_add(1);
            (StatusCode::PARTIAL_CONTENT, start, Some(end), len)
        }
        None => (StatusCode::OK, 0, None, size),
    };

    let stream = ReaderStream::new(file.take(content_len));
    let mut response = Response::builder().status(status);
    let headers = response.headers_mut().expect("response headers");
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&download_content_type(&path))
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&content_len.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag)
            .unwrap_or_else(|_| HeaderValue::from_static("\"phonolite-download\"")),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&download_content_disposition(&track, &path))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    if let Some(end) = end {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end, size))
                .unwrap_or_else(|_| HeaderValue::from_static("bytes */0")),
        );
    }

    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub async fn create_download_batch(
    State(state): State<AppState>,
    Json(request): Json<DownloadBatchRequest>,
) -> JsonResult<DownloadBatchResponse> {
    let library = library_or_json_error(&state)?;
    let liked_set = liked_set(&state)?;
    let playlist_set = playlist_set(&state)?;

    let mut seen = HashSet::new();
    let track_ids = request
        .track_ids
        .into_iter()
        .map(|track_id| track_id.trim().to_string())
        .filter(|track_id| !track_id.is_empty())
        .filter(|track_id| seen.insert(track_id.clone()))
        .collect::<Vec<_>>();

    if track_ids.is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "track_ids must contain at least one track".to_string(),
        ));
    }

    let mut items = Vec::new();
    let mut unavailable = Vec::new();
    for track_id in track_ids {
        let track = match library.get_track(&track_id) {
            Ok(Some(track)) => track,
            Ok(None) => {
                unavailable.push(DownloadBatchUnavailable {
                    track_id,
                    reason: "track not found".to_string(),
                });
                continue;
            }
            Err(err) => {
                unavailable.push(DownloadBatchUnavailable {
                    track_id,
                    reason: format!("library error: {}", err),
                });
                continue;
            }
        };
        let path = match library.resolve_relpath(&track.file_relpath) {
            Some(path) => path,
            None => {
                unavailable.push(DownloadBatchUnavailable {
                    track_id: track.id,
                    reason: "music root not configured".to_string(),
                });
                continue;
            }
        };
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => {
                unavailable.push(DownloadBatchUnavailable {
                    track_id: track.id,
                    reason: "track file not found".to_string(),
                });
                continue;
            }
        };
        let offline_metadata =
            match offline_metadata_for_track(&library, &track, &liked_set, &playlist_set) {
                Ok(metadata) => metadata,
                Err(reason) => {
                    unavailable.push(DownloadBatchUnavailable {
                        track_id: track.id,
                        reason,
                    });
                    continue;
                }
            };
        let sha256 = match checksum_for_file(&path, &metadata).await {
            Ok(sha256) => sha256,
            Err(err) => {
                unavailable.push(DownloadBatchUnavailable {
                    track_id: track.id,
                    reason: format!("checksum failed: {}", err),
                });
                continue;
            }
        };
        let byte_length = metadata.len();
        items.push(DownloadBatchItem {
            track_id: track.id.clone(),
            download_url: format!(
                "/api/download/tracks/{}",
                percent_encode_path_component(&track.id)
            ),
            offline_metadata,
            byte_length,
            content_type: download_content_type(&path),
            etag: download_etag(&track, byte_length),
            sha256,
        });
    }

    let batch_id = request
        .client_batch_id
        .and_then(|value| {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    Ok(Json(DownloadBatchResponse {
        schema_version: OFFLINE_METADATA_SCHEMA_VERSION,
        batch_id,
        created_at: unix_timestamp_ms(),
        items,
        unavailable,
    }))
}

pub async fn get_offline_track_metadata(
    State(state): State<AppState>,
    AxumPath(track_id): AxumPath<String>,
) -> JsonResult<OfflineTrackMetadata> {
    let library = library_or_json_error(&state)?;
    let track = match library.get_track(&track_id) {
        Ok(Some(track)) => track,
        Ok(None) => return Err(json_error(StatusCode::NOT_FOUND, "track not found")),
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("library error: {}", err),
            ))
        }
    };
    let liked_set = liked_set(&state)?;
    let playlist_set = playlist_set(&state)?;
    offline_metadata_for_track(&library, &track, &liked_set, &playlist_set)
        .map(Json)
        .map_err(|err| {
            let status = if err.ends_with("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            json_error(status, err)
        })
}

fn unix_timestamp_ms() -> u64 {
    UNIX_EPOCH
        .elapsed()
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}
