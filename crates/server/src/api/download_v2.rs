use std::collections::HashSet;
use std::convert::Infallible;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    response::Response,
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use crate::download_jobs::{
    DownloadJob, DownloadJobItem, DownloadJobItemStatus, DownloadJobScope, DownloadJobStatus,
    DownloadJobStore,
};
use crate::download_profile::DownloadProfile;
use crate::http::{json_error, json_error_response};
use crate::state::{AppState, AuthContext, JsonResult};

use super::{
    library_or_json_error,
    views::{build_album_view, build_artist_view, liked_set, playlist_set},
};
use crate::download_files::{
    download_content_type, download_etag_from_metadata, percent_encode_path_component,
};
use crate::offline_metadata::{offline_metadata_for_track, OFFLINE_METADATA_SCHEMA_VERSION};

#[derive(Serialize)]
pub struct ServerCapabilitiesResponse {
    pub download_profiles: [&'static str; 5],
    pub account_events_v2: &'static str,
    pub shuffle_presets: &'static str,
    pub last_track: &'static str,
    pub playback_resume: &'static str,
    pub playback_presence: bool,
    pub adaptive_audio_ranges: bool,
    pub adaptive_original_audio: bool,
    pub session_management: &'static str,
    pub catalogue_events: &'static str,

    pub account_collections_sync: &'static str,
    pub atomic_playlist_tracks: &'static str,
    pub download_jobs_v2: &'static str,
    pub metadata_snapshots_v2: &'static str,
    pub event_replay_v2: &'static str,
    pub track_file_contract_v2: &'static str,
    pub artist_summaries: &'static str,
    pub artist_thumbnails: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct CreateDownloadJobRequest {
    #[serde(default)]
    pub profile: DownloadProfile,
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
        download_profiles: DownloadProfile::ALL,
        account_events_v2: "1",
        shuffle_presets: "1",
        last_track: "1",
        playback_resume: "1",
        playback_presence: true,
        adaptive_audio_ranges: true,
        adaptive_original_audio: true,
        session_management: "1",
        catalogue_events: "1",

        account_collections_sync: "1",
        atomic_playlist_tracks: "1",
        download_jobs_v2: "1",
        metadata_snapshots_v2: "1",
        event_replay_v2: "1",
        track_file_contract_v2: "1",
        artist_summaries: "1",
        artist_thumbnails: "1",
    })
}

pub async fn create_download_job(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(request): Json<CreateDownloadJobRequest>,
) -> JsonResult<DownloadJob> {
    ensure_download_mutation_running(&state)?;
    let client_id = required_text(&request.client_id, "client_id")?;
    let client_request_id = required_text(&request.client_request_id, "client_request_id")?;
    let scope = normalize_scope(request.scope)?;
    let profile = request.profile;

    // A cached job must not outlive access to its source playlist.
    ensure_playlist_scope_access(&state, &ctx.user.id, &scope)?;

    if let Some(job) = state
        .download_jobs
        .get_by_request(&ctx.user.id, &client_id, &client_request_id)
        .map_err(store_error)?
    {
        if job.profile != profile {
            return Err(json_error(
                StatusCode::CONFLICT,
                "request id already belongs to a different download profile",
            ));
        }
        // Idempotent request lookup returns the stored scope, which can differ
        // from the retried request's body.
        ensure_playlist_scope_access(&state, &ctx.user.id, &job.scope)?;
        crate::download_renditions::schedule(&state, &job);
        return Ok(Json(job));
    }
    if let Some(job) = state
        .download_jobs
        .get_by_scope_profile(&ctx.user.id, &client_id, &scope, profile)
        .map_err(store_error)?
    {
        crate::download_renditions::schedule(&state, &job);
        return Ok(Json(job));
    }

    let library = library_or_json_error(&state)?;
    let preparation_state = state.clone();
    let preparation_user = ctx.user.id.clone();
    let preparation_scope = scope.clone();
    let preparation_flag = state.shutdown.flag();
    let mut items = state
        .shutdown
        .spawn_cancellable_blocking("download_job_prepare", move || {
            Ok((|| {
                let liked = liked_set(&preparation_state, &preparation_user)?;
                let playlists = playlist_set(&preparation_state, &preparation_user)?;
                let track_ids = expand_scope(
                    &preparation_state,
                    &library,
                    &preparation_user,
                    &preparation_scope,
                )?;
                resolve_job_items(&library, &liked, &playlists, track_ids, &preparation_flag)
            })())
        })
        .await
        .map_err(download_task_error)?
        .map_err(|err| json_error(StatusCode::SERVICE_UNAVAILABLE, err))??;
    for item in &mut items {
        item.profile = profile;
        if profile != DownloadProfile::Original
            && item.status == DownloadJobItemStatus::ReadyToDownload
        {
            item.status = DownloadJobItemStatus::Queued;
            item.download_url = None;
            item.byte_length = None;
            item.content_type = None;
            item.etag = None;
            item.sha256 = None;
        }
    }
    let result = run_download_commit(&state, "download_job_create", move |store| {
        let job = store.create_record_with_profile(
            ctx.user.id.clone(),
            client_id,
            client_request_id,
            scope,
            items,
            profile,
        );
        let job = store.upsert_job(job).map_err(store_error)?;
        let _ = store.append_event(job.clone(), "job_created", "download job created");
        Ok(Json(
            store
                .get_job(&job.job_id)
                .map_err(store_error)?
                .unwrap_or(job),
        ))
    })
    .await?;
    crate::download_renditions::schedule(&state, &result.0);
    Ok(result)
}

pub async fn list_download_jobs(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(query): Query<DownloadJobQuery>,
) -> JsonResult<Vec<DownloadJob>> {
    let mut jobs = state
        .download_jobs
        .list_jobs(
            &ctx.user.id,
            query.client_id.as_deref(),
            query.scope_kind.as_deref(),
            query.scope_id.as_deref(),
        )
        .map_err(store_error)?;
    for job in &mut jobs {
        refresh_preparation(&state, job)?;
    }
    Ok(Json(jobs))
}

pub async fn get_download_job(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(job_id): AxumPath<String>,
) -> JsonResult<DownloadJob> {
    let Some(mut job) = state.download_jobs.get_job(&job_id).map_err(store_error)? else {
        return Err(json_error(StatusCode::NOT_FOUND, "download job not found"));
    };
    if job.user_id != ctx.user.id {
        return Err(json_error(StatusCode::NOT_FOUND, "download job not found"));
    }
    refresh_preparation(&state, &mut job)?;
    Ok(Json(job))
}

pub async fn apply_download_job_action(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath(job_id): AxumPath<String>,
    Json(request): Json<DownloadJobActionRequest>,
) -> JsonResult<DownloadJob> {
    let interrupted = matches!(request.action.trim(), "pause" | "cancel" | "delete");
    let result = run_download_commit(&state, "download_job_action", move |store| {
        let lock = crate::download_renditions::job_lock(&job_id);
        let _guard = lock.lock();
        let Some(mut job) = store.get_job(&job_id).map_err(store_error)? else {
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
                store.delete_job(&job).map_err(store_error)?;
                return Ok(Json(job));
            }
            _ => {
                return Err(json_error(
                    StatusCode::BAD_REQUEST,
                    "unsupported download job action",
                ))
            }
        }
        let job = store.upsert_job(job).map_err(store_error)?;
        let _ = store.append_event(
            job.clone(),
            "job_updated",
            format!("download job action applied: {}", request.action.trim()),
        );
        Ok(Json(
            store
                .get_job(&job.job_id)
                .map_err(store_error)?
                .unwrap_or(job),
        ))
    })
    .await?;
    if interrupted {
        crate::download_renditions::interrupt(&result.0.job_id);
    } else {
        crate::download_renditions::schedule(&state, &result.0);
    }
    Ok(result)
}

fn refresh_preparation(
    state: &AppState,
    job: &mut DownloadJob,
) -> Result<(), (StatusCode, Json<crate::state::ErrorResponse>)> {
    let lock = crate::download_renditions::job_lock(&job.job_id);
    let _guard = lock.lock();
    let Some(current) = state
        .download_jobs
        .get_job(&job.job_id)
        .map_err(store_error)?
    else {
        return Err(json_error(StatusCode::NOT_FOUND, "download job not found"));
    };
    *job = current;
    crate::download_renditions::refresh_missing(state, job).map_err(store_error)?;
    crate::download_renditions::schedule(state, job);
    Ok(())
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
    Extension(ctx): Extension<AuthContext>,
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
            let liked = liked_set(&state, &ctx.user.id)?;
            let playlists = playlist_set(&state, &ctx.user.id)?;
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
        schema_version: OFFLINE_METADATA_SCHEMA_VERSION,
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
    super::track_files::track_file_response(
        &state,
        &track_id,
        &headers,
        crate::download_files::FileDelivery::Download,
    )
    .await
}

#[derive(Deserialize)]
pub struct RenditionQuery {
    pub rendition_id: String,
}

/// Job ownership is checked independently of a cached file's reusable identity.
pub async fn download_prepared_file(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    AxumPath((job_id, track_id)): AxumPath<(String, String)>,
    Query(query): Query<RenditionQuery>,
    headers: HeaderMap,
) -> Response {
    let job = match state.download_jobs.get_job(&job_id) {
        Ok(Some(job)) if job.user_id == ctx.user.id => job,
        _ => return json_error_response(StatusCode::NOT_FOUND, "download job not found"),
    };
    if let Err((status, error)) = ensure_playlist_scope_access(&state, &ctx.user.id, &job.scope) {
        return json_error_response(status, error.0.error);
    }
    if matches!(
        job.status,
        DownloadJobStatus::Canceled | DownloadJobStatus::Removing
    ) {
        return json_error_response(StatusCode::GONE, "download job no longer available");
    }
    let Some(item) = job.items.iter().find(|item| {
        item.track_id == track_id
            && item.status == DownloadJobItemStatus::ReadyToDownload
            && item.rendition_id.as_deref() == Some(query.rendition_id.as_str())
            && item.profile == job.profile
            && job.profile != DownloadProfile::Original
    }) else {
        return json_error_response(
            StatusCode::CONFLICT,
            "download rendition is not ready; refresh job",
        );
    };
    let Some(path) = crate::download_renditions::file_path(&state, &query.rendition_id)
        .filter(|_| crate::download_renditions::is_ready(&state, &query.rendition_id))
    else {
        return json_error_response(StatusCode::GONE, "download rendition expired; refresh job");
    };
    let library = match library_or_json_error(&state) {
        Ok(library) => library,
        Err((status, error)) => return json_error_response(status, error.0.error),
    };
    let track = match library.get_track(&track_id) {
        Ok(Some(track)) => track,
        _ => return json_error_response(StatusCode::NOT_FOUND, "track not found"),
    };
    let mut response = crate::download_files::track_file_response(
        &track,
        &path,
        &headers,
        crate::download_files::FileDelivery::Download,
        &state.shutdown,
    )
    .await;
    if let Ok(value) = axum::http::HeaderValue::from_str(item.profile.id()) {
        response
            .headers_mut()
            .insert("x-phonolite-download-profile", value);
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(&query.rendition_id) {
        response
            .headers_mut()
            .insert("x-phonolite-rendition-id", value);
    }
    response
}

fn resolve_job_items(
    library: &library::Library,
    liked_set: &HashSet<String>,
    playlist_set: &HashSet<String>,
    track_ids: Vec<String>,
    cancellation: &std::sync::atomic::AtomicBool,
) -> Result<Vec<DownloadJobItem>, (StatusCode, Json<crate::state::ErrorResponse>)> {
    let mut items = Vec::new();
    for (position, track_id) in track_ids.into_iter().enumerate() {
        if cancellation.load(std::sync::atomic::Ordering::Acquire) {
            return Err(json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "server shutting down",
            ));
        }
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
        let metadata = match std::fs::metadata(&path) {
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
            profile: DownloadProfile::Original,
            rendition_id: None,
            position,
            track_id: track.id.clone(),
            status: DownloadJobItemStatus::ReadyToDownload,
            download_url: Some(format!(
                "/api/download/tracks/{}/file",
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
    Ok(items)
}

fn ensure_playlist_scope_access(
    state: &AppState,
    user_id: &str,
    scope: &DownloadJobScope,
) -> Result<(), (StatusCode, Json<crate::state::ErrorResponse>)> {
    if scope.kind == "playlist" {
        let playlist_id = required_scope_id(scope, "playlist")?;
        state
            .user_data
            .get_playlist(user_id, &playlist_id)
            .map_err(|err| {
                json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("playlist error: {:?}", err),
                )
            })?
            .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "playlist not found"))?;
    }
    Ok(())
}

fn expand_scope(
    state: &AppState,
    library: &library::Library,
    user_id: &str,
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
                let tracks =
                    crate::shuffle::artist_album_tracks(library, &artist_id).map_err(|err| {
                        json_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("library error: {}", err),
                        )
                    })?;
                if tracks.is_empty() {
                    return Err(json_error(
                        StatusCode::NOT_FOUND,
                        "artist has no headline albums",
                    ));
                }
                tracks.into_iter().map(|track| track.id).collect()
            }
            "playlist" => {
                let playlist_id = required_scope_id(scope, "playlist")?;
                state
                    .user_data
                    .get_playlist(user_id, &playlist_id)
                    .map_err(|err| {
                        json_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("playlist error: {:?}", err),
                        )
                    })?
                    .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "playlist not found"))?
                    .track_ids
            }
            "liked" => state.user_data.list_likes(user_id).map_err(|err| {
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
        profile: DownloadProfile::Original,
        rendition_id: None,
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

fn ensure_download_mutation_running(
    state: &AppState,
) -> Result<(), (StatusCode, Json<crate::state::ErrorResponse>)> {
    if state.shutdown.is_cancelled() {
        Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "server shutting down",
        ))
    } else {
        Ok(())
    }
}
fn download_task_error(
    error: tokio::task::JoinError,
) -> (StatusCode, Json<crate::state::ErrorResponse>) {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("download job task failed: {error}"),
    )
}
async fn run_download_commit<R, F>(
    state: &AppState,
    name: &str,
    work: F,
) -> Result<R, (StatusCode, Json<crate::state::ErrorResponse>)>
where
    R: Send + 'static,
    F: FnOnce(DownloadJobStore) -> Result<R, (StatusCode, Json<crate::state::ErrorResponse>)>
        + Send
        + 'static,
{
    ensure_download_mutation_running(state)?;
    let store = state.download_jobs.clone();
    state
        .shutdown
        .spawn_blocking_commit(name, move || work(store))
        .await
        .map_err(download_task_error)?
}
