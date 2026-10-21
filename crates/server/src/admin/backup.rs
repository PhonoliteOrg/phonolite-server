//! Authenticated, bounded portable metadata archives. No archive path is extracted.
mod archive;
pub(crate) mod jobs;
mod parts;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Component, Path};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::{
    body::Body,
    extract::{Path as AxumPath, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::bufread::GzDecoder;
#[cfg(test)]
use flate2::{write::GzEncoder, Compression};
use futures_util::StreamExt;
use library::{
    backup::{CatalogBackup, RestoreReport},
    Library,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use tokio_util::io::ReaderStream;

use super::{admin_user_from_headers, is_admin};
use crate::{
    assets::{metadata_root_path, METADATA_ARCHIVE_LOCK},
    http::json_error_response,
    state::{AppState, LibraryStatus},
};

const MAGIC: &[u8] = b"PHONOLITE-METADATA\x01\n";
// Compatibility bounds for the original in-memory v1 format only.
const MAX_COMPRESSED: usize = 64 * 1024 * 1024;
const MAX_EXPANDED: usize = 256 * 1024 * 1024;
const MAX_IMAGE: usize = 16 * 1024 * 1024;

struct Download {
    file: tokio::fs::File,
    _scratch: Arc<archive::Scratch>,
}

impl AsyncRead for Download {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.file).poll_read(cx, buffer)
    }
}

pub(super) fn limit_label(mib: u64) -> String {
    if mib.is_multiple_of(1024) {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{mib} MiB")
    }
}

pub async fn limits(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    let Ok(_busy) = METADATA_ARCHIVE_LOCK.try_lock() else {
        return busy();
    };
    match archive::Limits::from_config(&state.config.read()) {
        Ok(limits) => Json(serde_json::json!({ "max_archive_bytes": limits.archive, "max_expanded_bytes": limits.expanded })).into_response(),
        Err(err) => json_error_response(StatusCode::BAD_REQUEST, err),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct BackupFile {
    format_version: u32,
    created_at: u64,
    server_version: String,
    catalogs: Vec<CatalogBackup>,
    /// Relative source references to base64-encoded downloaded image bytes.
    assets: BTreeMap<String, String>,
}

fn authorize(state: &AppState, headers: &HeaderMap) -> Result<Library, Response> {
    authorize_admin(state, headers)?;
    let guard = state.library_state.read();
    if !matches!(guard.status, LibraryStatus::Ready(_)) {
        return Err(json_error_response(
            StatusCode::CONFLICT,
            "Configure music folders and finish scanning before managing backups.",
        ));
    }
    guard
        .library
        .clone()
        .ok_or_else(|| json_error_response(StatusCode::CONFLICT, "library unavailable"))
}

fn authorize_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    match admin_user_from_headers(state, headers) {
        Ok(Some(user)) if is_admin(&user) => {}
        Ok(Some(_)) => {
            return Err(json_error_response(
                StatusCode::FORBIDDEN,
                "administrator required",
            ))
        }
        Ok(None) => {
            return Err(json_error_response(
                StatusCode::UNAUTHORIZED,
                "sign in to manage backups",
            ))
        }
        Err(_) => {
            return Err(json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication failed",
            ))
        }
    }
    Ok(())
}

pub async fn export(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let library = match authorize(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Ok(_busy) = METADATA_ARCHIVE_LOCK.try_lock() else {
        return busy();
    };
    let limits = match archive::Limits::from_config(&state.config.read()) {
        Ok(limits) => limits,
        Err(err) => return json_error_response(StatusCode::BAD_REQUEST, err),
    };
    let scratch = match archive::Scratch::new() {
        Ok(scratch) => Arc::new(scratch),
        Err(err) => return json_error_response(StatusCode::INTERNAL_SERVER_ERROR, err),
    };
    let snapshot = jobs::Snapshot::new("export", "preparing", "Preparing metadata backup…");
    let snapshot = match state.backup_jobs.insert(jobs::Job::new(snapshot, scratch)) {
        Ok(snapshot) => snapshot,
        Err(err) => return json_error_response(StatusCode::CONFLICT, err),
    };
    let id = snapshot.id.clone();
    let root = metadata_root_path(&state);
    let store = state.backup_jobs.clone();
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            let _busy = _busy;
            let catalogs = library.export_metadata().map_err(|err| err.to_string())?;
            let archive = archive::export(catalogs, &root, parts::whole_limits(limits))?;
            parts::split(archive, limits.archive)
        })
        .await;
        match result {
            Ok(Ok(prepared)) => {
                store.with(&id, |job| {
                    job.scratch = prepared.scratch;
                    job.filenames = prepared
                        .files
                        .iter()
                        .map(|(name, _)| name.clone())
                        .collect();
                    job.snapshot.parts = prepared
                        .files
                        .into_iter()
                        .enumerate()
                        .map(|(index, (name, bytes))| jobs::DownloadPart {
                            index,
                            name: if name == "export.phonolite" {
                                "phonolite-metadata.phonolite".into()
                            } else {
                                name
                            },
                            bytes,
                            url: format!("/settings/backup/jobs/{id}/parts/{index}"),
                        })
                        .collect();
                    job.snapshot.total_parts = job.snapshot.parts.len();
                    job.snapshot.phase = "ready";
                    job.snapshot.message =
                        format!("Backup ready: {} file(s)", job.snapshot.total_parts);
                });
            }
            Ok(Err(err)) => store.phase(&id, "failed", err),
            Err(_) => store.phase(&id, "failed", "Backup creation failed."),
        }
    });
    (StatusCode::ACCEPTED, Json(snapshot)).into_response()
}

pub async fn list_jobs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_admin(&state, &headers) {
        return response;
    }
    Json(serde_json::json!({"jobs": state.backup_jobs.list()})).into_response()
}

pub async fn job_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize_admin(&state, &headers) {
        return response;
    }
    match state.backup_jobs.get(&id) {
        Some(snapshot) => Json(snapshot).into_response(),
        None => json_error_response(
            StatusCode::NOT_FOUND,
            "Backup expired or was removed. Upload the files again.",
        ),
    }
}

pub async fn remove_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize_admin(&state, &headers) {
        return response;
    }
    let Ok(_guard) = METADATA_ARCHIVE_LOCK.try_lock() else {
        return busy();
    };
    match state.backup_jobs.remove(&id) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(err) => json_error_response(StatusCode::CONFLICT, err),
    }
}

pub async fn download_part(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((id, index)): AxumPath<(String, usize)>,
) -> Response {
    if let Err(response) = authorize_admin(&state, &headers) {
        return response;
    }
    let part = state
        .backup_jobs
        .with(&id, |job| {
            if job.snapshot.kind != "export" || job.snapshot.phase != "ready" {
                return None;
            }
            Some((
                job.scratch.clone(),
                job.filenames.get(index)?.clone(),
                job.snapshot.parts.get(index)?.clone(),
            ))
        })
        .flatten();
    let Some((scratch, filename, part)) = part else {
        return json_error_response(StatusCode::NOT_FOUND, "Backup file unavailable.");
    };
    let file = match tokio::fs::File::open(scratch.path(&filename)).await {
        Ok(file) => file,
        Err(err) => return json_error_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    state.backup_jobs.with(&id, |job| {
        job.started.insert(index);
        job.snapshot.started_downloads = job.started.len();
        job.snapshot.message = format!(
            "Backup downloads started: {}/{}",
            job.started.len(),
            job.snapshot.total_parts
        );
    });
    let reader = Download {
        file,
        _scratch: scratch,
    };
    Response::builder()
        .header(header::CONTENT_TYPE, "application/vnd.phonolite.metadata")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", part.name),
        )
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::CONTENT_LENGTH, part.bytes)
        .body(Body::from_stream(ReaderStream::with_capacity(
            reader,
            64 * 1024,
        )))
        .unwrap()
}

pub async fn preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    upload(state, headers, request, false).await
}

pub async fn restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    upload(state, headers, request, true).await
}

fn busy() -> Response {
    json_error_response(
        StatusCode::CONFLICT,
        "Another backup operation is in progress.",
    )
}

async fn upload(state: AppState, headers: HeaderMap, request: Request, commit: bool) -> Response {
    // Check authentication before buffering or decompressing an upload.
    let library = match authorize(&state, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Ok(_busy) = METADATA_ARCHIVE_LOCK.try_lock() else {
        return busy();
    };
    let limits = match archive::Limits::from_config(&state.config.read()) {
        Ok(limits) => limits,
        Err(err) => return json_error_response(StatusCode::BAD_REQUEST, err),
    };
    let scratch = match archive::Scratch::new() {
        Ok(scratch) => scratch,
        Err(err) => return json_error_response(StatusCode::INTERNAL_SERVER_ERROR, err),
    };
    let transfer = state.backup_jobs.transfer();
    let mut file =
        match stage_upload_with_progress(request.into_body(), &scratch, limits.archive, |bytes| {
            transfer.progress(bytes)
        })
        .await
        {
            Ok(file) => file,
            Err(response) => return response,
        };
    let scratch = Arc::new(scratch);
    let store = state.backup_jobs.clone();
    let operation_state = state.clone();
    let staged = tokio::task::spawn_blocking(move || {
        // Once the body is on disk, accepting/checking it survives cancellation
        // of this HTTP request. Keep the operation lock inside that work too.
        let guard = _busy;
        transfer.checking();
        let header = parts::inspect(&mut file, limits);
        drop(file);
        let (id, complete) = register_upload(&store, scratch, header?)?;
        drop(transfer);
        if complete {
            start_processing(operation_state, library, id.clone(), limits, commit, guard);
        }
        Ok::<_, String>(id)
    })
    .await;
    let id = match staged {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => return json_error_response(StatusCode::BAD_REQUEST, err),
        Err(_) => {
            return json_error_response(StatusCode::INTERNAL_SERVER_ERROR, "Backup staging failed.")
        }
    };
    (StatusCode::ACCEPTED, Json(state.backup_jobs.get(&id))).into_response()
}

fn add_part(
    job: &mut jobs::Job,
    incoming: &archive::Scratch,
    header: parts::Header,
) -> Result<(), String> {
    if let Some(first) = job.received.values().next() {
        if !first.same_set(&header) {
            return Err("Backup parts have conflicting set information.".into());
        }
    }
    if let Some(previous) = job.received.get(&header.index) {
        if previous != &header {
            return Err("A different file already occupies this backup part number.".into());
        }
        return Ok(());
    }
    let total = job
        .received
        .values()
        .try_fold(header.payload_bytes, |total, part| {
            total.checked_add(part.payload_bytes)
        })
        .ok_or("Backup size overflow.")?;
    if total > header.total_bytes {
        return Err("Backup parts exceed their declared total size.".into());
    }
    std::fs::rename(
        incoming.path("upload.phonolite"),
        job.scratch
            .path(&format!("part-{}.phonolite", header.index)),
    )
    .map_err(|e| e.to_string())?;
    job.snapshot.total_parts = header.count as usize;
    job.snapshot.set_id = Some(header.set.to_string());
    job.received.insert(header.index, header);
    job.snapshot.received_parts = job.received.len();
    let missing: Vec<_> = (0..job.snapshot.total_parts as u32)
        .filter(|index| !job.received.contains_key(index))
        .take(8)
        .map(|index| (index + 1).to_string())
        .collect();
    job.snapshot.message = if missing.is_empty() {
        format!("All {} backup parts received.", job.snapshot.total_parts)
    } else {
        format!(
            "Backup received: {}/{} parts. Missing: {}{}.",
            job.snapshot.received_parts,
            job.snapshot.total_parts,
            missing.join(", "),
            if job.snapshot.total_parts - job.snapshot.received_parts > 8 {
                ", …"
            } else {
                ""
            }
        )
    };
    Ok(())
}

fn register_upload(
    store: &jobs::Store,
    scratch: Arc<archive::Scratch>,
    header: Option<parts::Header>,
) -> Result<(String, bool), String> {
    let snapshot = jobs::Snapshot::new("restore", "waiting", "Backup uploaded to disk.");
    if let Some(header) = header {
        let id = if let Some(id) = store.find_set(&header) {
            store
                .with(&id, |job| add_part(job, &scratch, header))
                .ok_or("Backup expired.")??;
            id
        } else {
            // A separate folder lets us atomically move the first validated file
            // exactly like later parts, without depending on its original name.
            let mut job = jobs::Job::new(snapshot, Arc::new(archive::Scratch::new()?));
            add_part(&mut job, &scratch, header)?;
            store.insert(job)?.id
        };
        let snapshot = store.get(&id).ok_or("Backup expired.")?;
        Ok((id, snapshot.received_parts == snapshot.total_parts))
    } else {
        let mut job = jobs::Job::new(snapshot, scratch);
        job.filenames.push("upload.phonolite".into());
        job.snapshot.received_parts = 1;
        job.snapshot.total_parts = 1;
        Ok((store.insert(job)?.id, true))
    }
}

pub async fn restore_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let library = match authorize(&state, &headers) {
        Ok(library) => library,
        Err(response) => return response,
    };
    let Ok(guard) = METADATA_ARCHIVE_LOCK.try_lock() else {
        return busy();
    };
    let Some(snapshot) = state.backup_jobs.get(&id) else {
        return json_error_response(
            StatusCode::NOT_FOUND,
            "Backup expired. Upload the files again.",
        );
    };
    if snapshot.kind != "restore" || snapshot.phase != "checked" {
        return json_error_response(
            StatusCode::CONFLICT,
            "Upload every part and finish checking the backup before restoring.",
        );
    }
    let limits = match archive::Limits::from_config(&state.config.read()) {
        Ok(limits) => limits,
        Err(err) => return json_error_response(StatusCode::BAD_REQUEST, err),
    };
    start_processing(state.clone(), library, id.clone(), limits, true, guard);
    (StatusCode::ACCEPTED, Json(state.backup_jobs.get(&id))).into_response()
}

fn start_processing(
    state: AppState,
    library: Library,
    id: String,
    limits: archive::Limits,
    commit: bool,
    guard: tokio::sync::MutexGuard<'static, ()>,
) {
    state.backup_jobs.phase(
        &id,
        if commit { "restoring" } else { "checking" },
        if commit {
            "Restoring metadata from disk…"
        } else {
            "Checking backup from disk…"
        },
    );
    tokio::spawn(async move {
        let root = metadata_root_path(&state);
        let events = state.metadata_events.clone();
        let operation_state = state.clone();
        let operation_id = id.clone();
        let result =
        tokio::task::spawn_blocking(move || -> Result<RestoreReport, String> {
            let _busy = guard;
            let (uploaded, received) = operation_state.backup_jobs.with(&operation_id, |job| (job.scratch.clone(), job.received.clone())).ok_or("Backup expired.")?;
            let scratch = archive::Scratch::new()?;
            let mut file = if received.is_empty() {
                std::fs::File::open(uploaded.path("upload.phonolite")).map_err(|e| e.to_string())?
            } else {
                operation_state.backup_jobs.phase(&operation_id, if commit { "restoring" } else { "checking" }, "Reading backup parts from disk…");
                parts::assemble(&uploaded, &received, &scratch)?
            };
            let parsed = archive::import(&mut file, &scratch, if received.is_empty() { limits } else { parts::whole_limits(limits) });
            drop(file);
            let file = parsed?;
            // Pin the destination through the restore. Configuration edits and
            // new scans wait until the source-driven transaction has finished.
            let config = operation_state.config.read();
            let library_state = operation_state.library_state.read();
            let same_roots = library_state.library.as_ref().is_some_and(|current|
                current.roots().len() == library.roots().len()
                    && current.roots().iter().zip(library.roots()).all(|(a, b)| a.id == b.id && a.path == b.path));
            if !matches!(library_state.status, LibraryStatus::Ready(_)) || !same_roots
                || crate::config::resolve_path(&operation_state.config_path, &config.metadata_path) != root {
                return Err("The library or metadata folder changed during upload. Finish scanning, then upload the backup again.".into());
            }
            if commit {
                operation_state.backup_jobs.phase(&operation_id, "restoring", "Restoring artwork from disk…");
                file.write_assets(&root)?;
            }
            operation_state.backup_jobs.phase(&operation_id, if commit { "restoring" } else { "checking" }, if commit { "Applying saved metadata…" } else { "Matching backup to library…" });
            let report = if commit {
                library.restore_metadata(&file.catalogs)
            } else {
                library.preview_metadata(&file.catalogs)
            }
            .map_err(|err| err.to_string())?;
            drop(library_state);
            drop(config);
            if commit {
                for album in library
                    .list_albums(None, usize::MAX, 0)
                    .map_err(|err| err.to_string())?
                    .0
                {
                    events.emit_album(album.id, Some(album.artist_id));
                }
                for artist in library
                    .list_artists(None, usize::MAX, 0)
                    .map_err(|err| err.to_string())?
                    .0
                {
                    events.emit_artist(artist.id);
                }
            }
            Ok(report)
        })
        .await;
        match result {
            Ok(Ok(report)) => {
                if commit {
                    {
                        let mut guard = state.library_state.write();
                        if matches!(guard.status, LibraryStatus::Ready(_)) {
                            // A scan can finish after the restore transaction.
                            // Read the current catalog instead of caching older counts.
                            if let Some(stats) = guard
                                .library
                                .as_ref()
                                .and_then(|library| library.stats().ok())
                            {
                                guard.status = LibraryStatus::Ready(stats);
                            }
                        }
                    }
                    let _ = state.activity.add_activity(
                        "Metadata backup restored; unmatched records retained for future scans.",
                    );
                }
                state.backup_jobs.with(&id, |job| {
                    job.snapshot.phase = if commit { "complete" } else { "checked" };
                    job.snapshot.message = if commit {
                        format!(
                            "Backup restored: {} artists, {} albums",
                            report.updated_artists, report.updated_albums
                        )
                    } else {
                        "Backup checked. Ready to restore missing metadata.".into()
                    };
                    job.snapshot.report = Some(report);
                });
            }
            Ok(Err(err)) => state.backup_jobs.phase(&id, "failed", err),
            Err(_) => state
                .backup_jobs
                .phase(&id, "failed", "Backup processing failed."),
        }
    });
}

#[cfg(test)]
async fn stage_upload(
    body: Body,
    scratch: &archive::Scratch,
    max_bytes: u64,
) -> Result<std::fs::File, Response> {
    stage_upload_with_progress(body, scratch, max_bytes, |_| {}).await
}

async fn stage_upload_with_progress(
    body: Body,
    scratch: &archive::Scratch,
    max_bytes: u64,
    mut progress: impl FnMut(u64),
) -> Result<std::fs::File, Response> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .open(scratch.path("upload.phonolite"))
        .await
        .map_err(|err| json_error_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let mut stream = body.into_data_stream();
    let mut length = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            json_error_response(StatusCode::BAD_REQUEST, "Backup upload was interrupted.")
        })?;
        length = length
            .checked_add(chunk.len() as u64)
            .filter(|length| *length <= max_bytes)
            .ok_or_else(|| {
                json_error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Backup exceeds the configured upload size limit.",
                )
            })?;
        file.write_all(&chunk).await.map_err(|err| {
            json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Cannot stage backup upload: {err}"),
            )
        })?;
        progress(length);
    }
    file.flush()
        .await
        .map_err(|err| json_error_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(file.into_std().await)
}

fn asset_refs(catalogs: &[CatalogBackup]) -> BTreeSet<String> {
    catalogs
        .iter()
        .flat_map(|catalog| &catalog.artists)
        .flat_map(|artist| [&artist.logo_ref, &artist.banner_ref])
        .flatten()
        .cloned()
        .collect()
}

fn image_extension(reference: &str) -> Option<&str> {
    if reference.contains(['\\', ':', '\0'])
        || reference.starts_with('/')
        || !Path::new(reference)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    match reference.rsplit_once('.')?.1 {
        ext @ ("jpg" | "jpeg" | "png" | "gif" | "webp") => Some(ext),
        _ => None,
    }
}

#[cfg(test)]
fn encode(file: &BackupFile) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(file).map_err(|err| err.to_string())?;
    if json.len() > MAX_EXPANDED {
        return Err("Metadata exceeds the 256 MiB expanded backup limit.".into());
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&json).map_err(|err| err.to_string())?;
    let compressed = encoder.finish().map_err(|err| err.to_string())?;
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(blake3::hash(&compressed).as_bytes());
    bytes.extend_from_slice(&compressed);
    if bytes.len() > MAX_COMPRESSED {
        return Err("Backup exceeds the 64 MiB upload limit.".into());
    }
    Ok(bytes)
}

#[cfg(test)]
fn decode(bytes: &[u8]) -> Result<BackupFile, String> {
    decode_with_limit(bytes, MAX_EXPANDED)
}

fn decode_with_limit(bytes: &[u8], expanded_limit: usize) -> Result<BackupFile, String> {
    let offset = MAGIC.len() + 32;
    if bytes.len() > MAX_COMPRESSED || bytes.len() < offset || !bytes.starts_with(MAGIC) {
        return Err("Choose a supported Phonolite metadata backup.".into());
    }
    let compressed = &bytes[offset..];
    if blake3::hash(compressed).as_bytes() != &bytes[MAGIC.len()..offset] {
        return Err("Backup integrity check failed; the file is damaged.".into());
    }
    let mut decoder = GzDecoder::new(compressed);
    let mut json = Vec::new();
    decoder
        .by_ref()
        .take(expanded_limit as u64 + 1)
        .read_to_end(&mut json)
        .map_err(|_| "Invalid compressed backup.")?;
    if json.len() > expanded_limit {
        return Err(format!(
            "Expanded legacy backup exceeds its {expanded_limit}-byte limit."
        ));
    }
    if !decoder.into_inner().is_empty() {
        return Err("Unexpected data after the backup.".into());
    }
    let file: BackupFile =
        serde_json::from_slice(&json).map_err(|_| "Invalid backup contents.".to_string())?;
    if file.format_version != 1 {
        return Err("Unsupported backup version; upgrade the server first.".into());
    }
    if file.catalogs.is_empty() {
        return Err("The backup has no metadata catalog.".into());
    }
    Ok(file)
}

/// Validate every asset before any filesystem or database write. Imported names
/// become content-addressed files, never paths supplied by the archive.
fn prepare_assets(file: &mut BackupFile) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut images = BTreeMap::new();
    let mut mapped = BTreeMap::new();
    for (reference, encoded) in &file.assets {
        let ext = image_extension(reference).ok_or("Invalid artwork path in backup.")?;
        if encoded.len() > (MAX_IMAGE + 2) / 3 * 4 {
            return Err("Backup artwork exceeds 16 MiB.".into());
        }
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| "Invalid backup artwork.")?;
        if bytes.len() > MAX_IMAGE {
            return Err("Backup artwork exceeds 16 MiB.".into());
        }
        let target = format!("restored/{}.{}", blake3::hash(&bytes).to_hex(), ext);
        mapped.insert(reference.clone(), target.clone());
        images.insert(target, bytes);
    }
    for catalog in &mut file.catalogs {
        for artist in &mut catalog.artists {
            for reference in [&mut artist.logo_ref, &mut artist.banner_ref] {
                // Missing artwork is a missing field, not a dangling path that
                // prevents normal providers from supplying it later.
                *reference = reference
                    .as_ref()
                    .and_then(|value| mapped.get(value))
                    .cloned();
            }
        }
    }
    Ok(images)
}

fn write_assets(root: &Path, images: &BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    if images.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(root.join("restored")).map_err(|err| err.to_string())?;
    let canonical_root = root.canonicalize().map_err(|err| err.to_string())?;
    let restored = root
        .join("restored")
        .canonicalize()
        .map_err(|err| err.to_string())?;
    if !restored.starts_with(&canonical_root) {
        return Err("Restored artwork folder must remain inside the metadata folder.".into());
    }
    for (reference, bytes) in images {
        let path = restored.join(
            Path::new(reference)
                .file_name()
                .ok_or("Invalid artwork filename.")?,
        );
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => file
                .write_all(bytes)
                .and_then(|_| file.sync_all())
                .map_err(|err| err.to_string())?,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&path).map_err(|err| err.to_string())?;
                if !metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || metadata.len() != bytes.len() as u64
                    || std::fs::read(&path).map_err(|err| err.to_string())? != *bytes
                {
                    return Err("Existing restored artwork failed its integrity check.".into());
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn staged_upload_enforces_the_total_limit_across_chunks() {
        let scratch = archive::Scratch::new().unwrap();
        let chunks = [b"abc".as_slice(), b"def".as_slice()]
            .map(|bytes| Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(bytes)));
        let response = stage_upload(
            Body::from_stream(futures_util::stream::iter(chunks)),
            &scratch,
            5,
        )
        .await
        .unwrap_err();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            std::fs::metadata(scratch.path("upload.phonolite"))
                .unwrap()
                .len()
                <= 3
        );
        let scratch = archive::Scratch::new().unwrap();
        let mut file = stage_upload(Body::from("abcdef"), &scratch, 6)
            .await
            .unwrap();
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"abcdef");
    }

    #[tokio::test]
    async fn dropping_a_download_cleans_its_temporary_archive() {
        let exported = archive::export(
            vec![CatalogBackup::default()],
            Path::new("."),
            archive::Limits::from_config(&crate::config::ServerConfig::default()).unwrap(),
        )
        .unwrap();
        let path = exported.scratch.path("export.phonolite");
        let reader = Download {
            file: tokio::fs::File::from_std(exported.file),
            _scratch: Arc::new(exported.scratch),
        };
        let body = Body::from_stream(ReaderStream::new(reader));
        assert!(path.exists());
        drop(body);
        assert!(!path.exists());
    }

    fn fixture() -> BackupFile {
        BackupFile {
            format_version: 1,
            created_at: 123,
            server_version: "test".into(),
            catalogs: vec![CatalogBackup::default()],
            assets: BTreeMap::new(),
        }
    }

    #[test]
    fn compressed_archive_roundtrip_validates_version_integrity_and_truncation() {
        let file = fixture();
        let bytes = encode(&file).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.created_at, 123);
        assert_eq!(decoded.catalogs.len(), 1);
        let mut corrupt = bytes.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert!(decode(&corrupt).unwrap_err().contains("integrity"));
        for end in [0, MAGIC.len(), bytes.len() - 1] {
            assert!(decode(&bytes[..end]).is_err());
        }
        let mut future = fixture();
        future.format_version = 2;
        assert!(decode(&encode(&future).unwrap())
            .unwrap_err()
            .contains("version"));
    }

    #[test]
    fn decompression_stops_at_its_expanded_limit_and_rejects_concatenated_streams() {
        let bytes = encode(&fixture()).unwrap();
        assert!(decode_with_limit(&bytes, 12)
            .unwrap_err()
            .contains("Expanded"));
        let offset = MAGIC.len() + 32;
        let mut concatenated = bytes[offset..].to_vec();
        concatenated.extend_from_slice(&bytes[offset..]);
        let mut archive = MAGIC.to_vec();
        archive.extend_from_slice(blake3::hash(&concatenated).as_bytes());
        archive.extend_from_slice(&concatenated);
        assert!(decode(&archive).unwrap_err().contains("Unexpected data"));
    }

    #[test]
    fn artwork_is_content_addressed_and_missing_images_become_missing_fields() {
        let mut file = fixture();
        file.catalogs[0].artists.push(common::Artist {
            id: "saved".into(),
            name: "Artist".into(),
            genres: vec![],
            summary: None,
            logo_ref: Some("logos/old.png".into()),
            banner_ref: Some("banners/missing.jpg".into()),
        });
        file.assets
            .insert("logos/old.png".into(), STANDARD.encode(b"image bytes"));
        let images = prepare_assets(&mut file).unwrap();
        let reference = file.catalogs[0].artists[0].logo_ref.clone().unwrap();
        assert!(reference.starts_with("restored/"));
        assert_eq!(images[&reference], b"image bytes");
        assert!(file.catalogs[0].artists[0].banner_ref.is_none());
        // Re-exported restored images do not acquire another directory prefix.
        let mut again = fixture();
        again.catalogs = file.catalogs;
        again
            .assets
            .insert(reference.clone(), STANDARD.encode(b"image bytes"));
        assert_eq!(prepare_assets(&mut again).unwrap(), images);
    }

    #[test]
    fn artwork_paths_and_encodings_are_validated_before_any_write() {
        for path in [
            "../escape.png",
            "/escape.png",
            "C:/escape.png",
            "logos\\escape.png",
            "logos/../../escape.png",
            "logos/payload.svg",
        ] {
            let mut file = fixture();
            file.assets.insert(path.into(), STANDARD.encode(b"image"));
            assert!(prepare_assets(&mut file).is_err(), "{path}");
        }
        let mut file = fixture();
        file.assets
            .insert("logos/image.png".into(), "not base64!".into());
        assert!(prepare_assets(&mut file).is_err());
    }

    #[test]
    fn artwork_restore_is_idempotent_and_never_overwrites_conflicting_files() {
        let root =
            std::env::temp_dir().join(format!("phonolite-backup-assets-{}", uuid::Uuid::new_v4()));
        let mut file = fixture();
        file.assets
            .insert("logos/image.png".into(), STANDARD.encode(b"image"));
        let images = prepare_assets(&mut file).unwrap();
        write_assets(&root, &images).unwrap();
        write_assets(&root, &images).unwrap();
        let reference = images.keys().next().unwrap();
        let path = root.join(reference);
        assert_eq!(std::fs::read(&path).unwrap(), b"image");
        std::fs::write(&path, b"wrong").unwrap();
        assert!(write_assets(&root, &images)
            .unwrap_err()
            .contains("integrity"));
        assert_eq!(std::fs::read(&path).unwrap(), b"wrong");
        std::fs::remove_dir_all(root).unwrap();
    }
}
