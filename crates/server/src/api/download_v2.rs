use std::collections::HashSet;
use std::convert::Infallible;

use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::download_jobs::{
    DownloadJob, DownloadJobItem, DownloadJobItemStatus, DownloadJobScope, DownloadJobStatus,
};
use crate::range::{parse_range_header, ByteRange};
use crate::state::{AppState, AuthContext, JsonResult};
use crate::utils::{json_error, json_error_response};

use super::{
    library::{
        checksum_for_file, download_content_disposition, download_content_type,
        download_etag_from_metadata, liked_set, offline_metadata_for_track,
        percent_encode_path_component, playlist_set, range_error_response,
    },
    library_or_json_error, library_or_response,
    views::{build_album_view, build_artist_view},
};

#[derive(Serialize)]
pub struct ServerCapabilitiesResponse {
    pub download_jobs_v2: &'static str,
    pub metadata_snapshots_v2: &'static str,
    pub event_replay_v2: &'static str,
    pub track_file_contract_v2: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct CreateDownloadJobRequest {
    pub client_id: String,
    pub client_request_id: String,
    pub scope: DownloadJobScope,
}

#[derive(Debug, Deserialize)]
pub struct DownloadJobQuery {
    pub client_id: Option<String>,
    pub scope_kind: Option<String>,
    pub scope_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DownloadJobActionRequest {
    pub action: String,
}

#[derive(Debug, Deserialize)]
pub struct EventQuery {
    pub cursor: Option<u64>,
}

#[derive(Serialize)]
pub struct MetadataSnapshotResponse {
    pub schema_version: u32,
    pub kind: String,
    pub id: String,
    pub snapshot: serde_json::Value,
}

pub async fn get_capabilities() -> Json<ServerCapabilitiesResponse> {
    Json(ServerCapabilitiesResponse {
        download_jobs_v2: "1",
        metadata_snapshots_v2: "1",
        event_replay_v2: "1",
        track_file_contract_v2: "1",
    })
}

pub async fn create_download_job(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(request): Json<CreateDownloadJobRequest>,
) -> JsonResult<DownloadJob> {
    let client_id = required_text(&request.client_id, "client_id")?;
    let client_request_id = required_text(&request.client_request_id, "client_request_id")?;
    let scope = normalize_scope(request.scope)?;

    if let Some(job) = state
        .download_jobs
        .get_by_request(&ctx.user.id, &client_id, &client_request_id)
        .map_err(store_error)?
    {
        return Ok(Json(job));
    }
    if let Some(job) = state
        .download_jobs
        .get_by_scope(&ctx.user.id, &client_id, &scope)
        .map_err(store_error)?
    {
        return Ok(Json(job));
    }

    let library = library_or_json_error(&state)?;
    let liked = liked_set(&state)?;
    let playlists = playlist_set(&state)?;
    let track_ids = expand_scope(&state, &library, &scope)?;
    let items = resolve_job_items(&library, &liked, &playlists, track_ids).await;
    let job = state.download_jobs.create_record(
        ctx.user.id.clone(),
        client_id,
        client_request_id,
        scope,
        items,
    );
    let job = state.download_jobs.upsert_job(job).map_err(store_error)?;
    let _ = state
        .download_jobs
        .append_event(job.clone(), "job_created", "download job created");
    Ok(Json(
        state
            .download_jobs
            .get_job(&job.job_id)
            .map_err(store_error)?
            .unwrap_or(job),
    ))
}

pub async fn list_download_jobs(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(query): Query<DownloadJobQuery>,
) -> JsonResult<Vec<DownloadJob>> {
    let jobs = state
        .download_jobs
        .list_jobs(
            &ctx.user.id,
            query.client_id.as_deref(),
            query.scope_kind.as_deref(),
            query.scope_id.as_deref(),
        )
        .map_err(store_error)?;
    Ok(Json(jobs))
}

pub async fn get_download_job(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(job_id): AxumPath<String>,
) -> JsonResult<DownloadJob> {
    let Some(job) = state.download_jobs.get_job(&job_id).map_err(store_error)? else {
        return Err(json_error(StatusCode::NOT_FOUND, "download job not found"));
    };
    if job.user_id != ctx.user.id {
        return Err(json_error(StatusCode::NOT_FOUND, "download job not found"));
    }
    Ok(Json(job))
}

pub async fn apply_download_job_action(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(job_id): AxumPath<String>,
    Json(request): Json<DownloadJobActionRequest>,
) -> JsonResult<DownloadJob> {
    let Some(mut job) = state.download_jobs.get_job(&job_id).map_err(store_error)? else {
        return Err(json_error(StatusCode::NOT_FOUND, "download job not found"));
    };
    if job.user_id != ctx.user.id {
        return Err(json_error(StatusCode::NOT_FOUND, "download job not found"));
    }

    match request.action.trim() {
        "pause" => job.status = DownloadJobStatus::Paused,
        "resume" => {
            job.status = if job.ready_count > 0 {
                DownloadJobStatus::ReadyToDownload
            } else {
                DownloadJobStatus::Queued
            };
        }
        "cancel" => {
            job.status = DownloadJobStatus::Canceled;
            for item in &mut job.items {
                if !matches!(
                    item.status,
                    DownloadJobItemStatus::Complete | DownloadJobItemStatus::MetadataFailed
                ) {
                    item.status = DownloadJobItemStatus::Canceled;
                }
            }
        }
        "retry_failed" => {
            for item in &mut job.items {
                if matches!(
                    item.status,
                    DownloadJobItemStatus::Failed | DownloadJobItemStatus::MetadataFailed
                ) {
                    item.status = DownloadJobItemStatus::Queued;
                    item.error = None;
                }
            }
            job.status = DownloadJobStatus::Queued;
        }
        "delete" => {
            state.download_jobs.delete_job(&job).map_err(store_error)?;
            return Ok(Json(job));
        }
        _ => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "unsupported download job action",
            ))
        }
    }
    let job = state.download_jobs.upsert_job(job).map_err(store_error)?;
    let _ = state.download_jobs.append_event(
        job.clone(),
        "job_updated",
        format!("download job action applied: {}", request.action.trim()),
    );
    Ok(Json(
        state
            .download_jobs
            .get_job(&job.job_id)
            .map_err(store_error)?
            .unwrap_or(job),
    ))
}

pub async fn download_job_events(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(job_id): AxumPath<String>,
    Query(query): Query<EventQuery>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, Response> {
    let Some(job) = state
        .download_jobs
        .get_job(&job_id)
        .map_err(|err| json_error_response(StatusCode::INTERNAL_SERVER_ERROR, err))?
    else {
        return Err(json_error_response(
            StatusCode::NOT_FOUND,
            "download job not found",
        ));
    };
    if job.user_id != ctx.user.id {
        return Err(json_error_response(
            StatusCode::NOT_FOUND,
            "download job not found",
        ));
    }
    let events = state
        .download_jobs
        .events_after(&job_id, query.cursor.unwrap_or(0))
        .map_err(|err| json_error_response(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    let stream = tokio_stream::iter(events.into_iter().filter_map(|event| {
        let data = serde_json::to_string(&event).ok()?;
        Some(Ok(Event::default()
            .id(event.cursor.to_string())
            .event(event.kind)
            .data(data)))
    }));
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

pub async fn metadata_events(
    Query(_query): Query<EventQuery>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let stream = tokio_stream::empty::<Result<Event, Infallible>>();
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub async fn metadata_snapshot(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
    AxumPath((kind, id)): AxumPath<(String, String)>,
) -> JsonResult<MetadataSnapshotResponse> {
    let library = library_or_json_error(&state)?;
    let kind = kind.trim().to_ascii_lowercase();
    let snapshot = match kind.as_str() {
        "track" => {
            let track = library
                .get_track(&id)
                .map_err(|err| {
                    json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("library error: {}", err),
                    )
                })?
                .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "track not found"))?;
            let liked = liked_set(&state)?;
            let playlists = playlist_set(&state)?;
            serde_json::to_value(
                offline_metadata_for_track(&library, &track, &liked, &playlists)
                    .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?,
            )
            .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        }
        "album" => {
            let album = library
                .get_album(&id)
                .map_err(|err| {
                    json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("library error: {}", err),
                    )
                })?
                .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "album not found"))?;
            serde_json::to_value(
                build_album_view(&library, album)
                    .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?,
            )
            .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        }
        "artist" => {
            let artist = library
                .get_artist(&id)
                .map_err(|err| {
                    json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("library error: {}", err),
                    )
                })?
                .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "artist not found"))?;
            serde_json::to_value(
                build_artist_view(&library, artist)
                    .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err))?,
            )
            .map_err(|err| json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        }
        _ => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "metadata snapshot kind must be track, album, or artist",
            ))
        }
    };
    Ok(Json(MetadataSnapshotResponse {
        schema_version: 4,
        kind,
        id,
        snapshot,
    }))
}

pub async fn download_track_file(
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
    let etag = download_etag_from_metadata(&track, &metadata);
    let sha256 = checksum_for_file(&path, &metadata).await.ok();
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
            .unwrap_or_else(|_| HeaderValue::from_static("\"phonolite-download-v2\"")),
    );
    if let Some(sha256) = sha256 {
        if let Ok(value) = HeaderValue::from_str(&sha256) {
            headers.insert("x-phonolite-sha256", value);
        }
    }
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

async fn resolve_job_items(
    library: &library::Library,
    liked_set: &HashSet<String>,
    playlist_set: &HashSet<String>,
    track_ids: Vec<String>,
) -> Vec<DownloadJobItem> {
    let mut items = Vec::new();
    for (position, track_id) in track_ids.into_iter().enumerate() {
        let track = match library.get_track(&track_id) {
            Ok(Some(track)) => track,
            Ok(None) => {
                items.push(failed_item(position, track_id, "track not found"));
                continue;
            }
            Err(err) => {
                items.push(failed_item(
                    position,
                    track_id,
                    format!("library error: {}", err),
                ));
                continue;
            }
        };
        let path = match library.resolve_relpath(&track.file_relpath) {
            Some(path) => path,
            None => {
                items.push(failed_item(position, track.id, "music root not configured"));
                continue;
            }
        };
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => {
                items.push(failed_item(position, track.id, "track file not found"));
                continue;
            }
        };
        let offline_metadata =
            match offline_metadata_for_track(library, &track, liked_set, playlist_set) {
                Ok(metadata) => metadata,
                Err(err) => {
                    items.push(failed_item(position, track.id, err));
                    continue;
                }
            };
        items.push(DownloadJobItem {
            position,
            track_id: track.id.clone(),
            status: DownloadJobItemStatus::ReadyToDownload,
            download_url: Some(format!(
                "/api/v1/download/v2/tracks/{}/file",
                percent_encode_path_component(&track.id)
            )),
            offline_metadata: Some(offline_metadata),
            byte_length: Some(metadata.len()),
            content_type: Some(download_content_type(&path)),
            etag: Some(download_etag_from_metadata(&track, &metadata)),
            sha256: None,
            error: None,
        });
    }
    items
}

fn expand_scope(
    state: &AppState,
    library: &library::Library,
    scope: &DownloadJobScope,
) -> Result<Vec<String>, (StatusCode, Json<crate::state::ErrorResponse>)> {
    let mut ids =
        match scope.kind.as_str() {
            "track" => vec![required_scope_id(scope, "track")?],
            "album" => {
                let album_id = required_scope_id(scope, "album")?;
                library
                    .get_album_tracks(&album_id)
                    .map_err(|err| {
                        json_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("library error: {}", err),
                        )
                    })?
                    .into_iter()
                    .map(|track| track.id)
                    .collect::<Vec<_>>()
            }
            "artist" => {
                let artist_id = required_scope_id(scope, "artist")?;
                let mut ids = Vec::new();
                let albums = library.list_artist_albums(&artist_id).map_err(|err| {
                    json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("library error: {}", err),
                    )
                })?;
                if albums.is_empty() {
                    return Err(json_error(StatusCode::NOT_FOUND, "artist not found"));
                }
                for album in albums {
                    let mut tracks = library.get_album_tracks(&album.id).map_err(|err| {
                        json_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("library error: {}", err),
                        )
                    })?;
                    tracks.sort_by(|a, b| {
                        a.disc_no
                            .unwrap_or(0)
                            .cmp(&b.disc_no.unwrap_or(0))
                            .then_with(|| a.track_no.unwrap_or(0).cmp(&b.track_no.unwrap_or(0)))
                            .then_with(|| a.title.cmp(&b.title))
                    });
                    ids.extend(tracks.into_iter().map(|track| track.id));
                }
                ids
            }
            "playlist" => {
                let playlist_id = required_scope_id(scope, "playlist")?;
                state
                    .user_data
                    .get_playlist(&playlist_id)
                    .map_err(|err| {
                        json_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("playlist error: {:?}", err),
                        )
                    })?
                    .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "playlist not found"))?
                    .track_ids
            }
            "liked" => state.user_data.list_likes().map_err(|err| {
                json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("likes error: {:?}", err),
                )
            })?,
            "track_set" => scope.track_ids.clone(),
            _ => return Err(json_error(
                StatusCode::BAD_REQUEST,
                "download scope kind must be track, album, artist, playlist, liked, or track_set",
            )),
        };
    let mut seen = HashSet::new();
    ids.retain(|id| {
        let id = id.trim();
        !id.is_empty() && seen.insert(id.to_string())
    });
    if ids.is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "download scope did not resolve to any tracks",
        ));
    }
    Ok(ids)
}

fn normalize_scope(
    mut scope: DownloadJobScope,
) -> Result<DownloadJobScope, (StatusCode, Json<crate::state::ErrorResponse>)> {
    scope.kind = scope.kind.trim().to_ascii_lowercase();
    scope.id = scope
        .id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    scope.track_ids = scope
        .track_ids
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    match scope.kind.as_str() {
        "track" | "album" | "artist" | "playlist" if scope.id.is_none() => {
            Err(json_error(StatusCode::BAD_REQUEST, "scope id is required"))
        }
        "track_set" if scope.track_ids.is_empty() => Err(json_error(
            StatusCode::BAD_REQUEST,
            "track_set scope requires track_ids",
        )),
        "liked" | "track" | "album" | "artist" | "playlist" | "track_set" => Ok(scope),
        _ => Err(json_error(
            StatusCode::BAD_REQUEST,
            "unsupported download scope kind",
        )),
    }
}

fn required_scope_id(
    scope: &DownloadJobScope,
    label: &str,
) -> Result<String, (StatusCode, Json<crate::state::ErrorResponse>)> {
    scope
        .id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| json_error(StatusCode::BAD_REQUEST, format!("{} id is required", label)))
}

fn required_text(
    value: &str,
    label: &str,
) -> Result<String, (StatusCode, Json<crate::state::ErrorResponse>)> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            format!("{} is required", label),
        ));
    }
    Ok(value)
}

fn failed_item(
    position: usize,
    track_id: impl Into<String>,
    error: impl Into<String>,
) -> DownloadJobItem {
    DownloadJobItem {
        position,
        track_id: track_id.into(),
        status: DownloadJobItemStatus::MetadataFailed,
        download_url: None,
        offline_metadata: None,
        byte_length: None,
        content_type: None,
        etag: None,
        sha256: None,
        error: Some(error.into()),
    }
}

fn store_error(message: String) -> (StatusCode, Json<crate::state::ErrorResponse>) {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("download job store error: {}", message),
    )
}
