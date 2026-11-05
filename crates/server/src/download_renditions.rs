//! Bounded, shutdown-owned offline audio preparation and immutable file delivery.
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::UNIX_EPOCH;

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::download_jobs::{DownloadJob, DownloadJobItemStatus, DownloadJobStatus};
use crate::download_profile::DownloadProfile;
use crate::state::AppState;

const MAX_PENDING_JOBS: usize = 32;
static ACTIVE: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
static WORKERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
static CACHE_WRITER: OnceLock<Mutex<()>> = OnceLock::new();
static JOB_LOCKS: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();

struct ActiveGuard(String);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.get().unwrap().lock().remove(&self.0);
    }
}

pub(crate) fn job_lock(id: &str) -> Arc<Mutex<()>> {
    let mut locks = JOB_LOCKS.get_or_init(Default::default).lock();
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(id.to_owned(), Arc::downgrade(&lock));
    lock
}

pub(crate) fn root(state: &AppState) -> PathBuf {
    state
        .config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("download_renditions")
}

pub(crate) fn file_path(state: &AppState, id: &str) -> Option<PathBuf> {
    (id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| root(state).join(format!("{id}.opus")))
}

pub(crate) fn is_ready(state: &AppState, id: &str) -> bool {
    file_path(state, id).is_some_and(|path| ready_path(&path))
}

fn ready_path(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Ok(mut marker) = File::open(path.with_extension("ready")) else {
        return false;
    };
    let expected = crate::stream_cache::PRESENTATION_POLICY_VERSION.as_bytes();
    let mut bytes = [0u8; 64];
    if expected.len() >= bytes.len() || marker.read_exact(&mut bytes[..expected.len()]).is_err() {
        return false;
    }
    bytes[..expected.len()] == *expected && matches!(marker.read(&mut bytes[..1]), Ok(0))
}

/// Recreate evicted files from the same profile on the next client refresh.
pub(crate) fn refresh_missing(state: &AppState, job: &mut DownloadJob) -> Result<(), String> {
    if job.profile == DownloadProfile::Original
        || matches!(
            job.status,
            DownloadJobStatus::Canceled | DownloadJobStatus::Removing | DownloadJobStatus::Paused
        )
    {
        return Ok(());
    }
    let mut changed = false;
    for item in &mut job.items {
        if item.status == DownloadJobItemStatus::ReadyToDownload
            && item
                .rendition_id
                .as_deref()
                .is_none_or(|id| !is_ready(state, id))
        {
            item.status = DownloadJobItemStatus::Queued;
            item.download_url = None;
            changed = true;
        }
    }
    if changed {
        job.status = DownloadJobStatus::Queued;
        recount(job);
        *job = state.download_jobs.upsert_job(job.clone())?;
    }
    Ok(())
}

/// A bounded number of asynchronous jobs wait for two worker permits. Heavy I/O
/// is serialized at cache publication so the disk limit includes temporary files.
pub(crate) fn schedule(state: &AppState, job: &DownloadJob) {
    if job.profile == DownloadProfile::Original
        || state.shutdown.is_cancelled()
        || matches!(
            job.status,
            DownloadJobStatus::Paused | DownloadJobStatus::Canceled | DownloadJobStatus::Removing
        )
        || !job
            .items
            .iter()
            .any(|item| item.status == DownloadJobItemStatus::Queued)
    {
        return;
    }
    let mut active = ACTIVE.get_or_init(Default::default).lock();
    if active.len() >= MAX_PENDING_JOBS || active.contains_key(&job.job_id) {
        return;
    }
    let interrupted = Arc::new(AtomicBool::new(false));
    active.insert(job.job_id.clone(), Arc::clone(&interrupted));
    drop(active);
    let state = state.clone();
    let id = job.job_id.clone();
    let active_guard = ActiveGuard(id.clone());
    let shutdown = state.shutdown.clone();
    shutdown.spawn_named("download_rendition_queue", async move {
        // Constructed outside the future so rejection/drop before its first poll
        // also releases queue admission during shutdown.
        let _active = active_guard;
        let permits = WORKERS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)));
        let Ok(_permit) = permits.acquire().await else {
            return;
        };
        let worker_state = state.clone();
        let worker_id = id.clone();
        let result = state
            .shutdown
            .spawn_cancellable_blocking("download_rendition_prepare", move || {
                prepare_job(&worker_state, &worker_id, &interrupted)
            })
            .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(job_id = %id, %error, "Download preparation failed"),
            Err(error) => {
                tracing::warn!(job_id = %id, %error, "Download preparation worker failed")
            }
        }
    });
}

pub(crate) fn interrupt(id: &str) {
    if let Some(flag) = ACTIVE
        .get()
        .and_then(|active| active.lock().get(id).cloned())
    {
        flag.store(true, Ordering::Release);
    }
}

fn runnable(state: &AppState, id: &str) -> bool {
    !state.shutdown.is_cancelled()
        && state
            .download_jobs
            .get_job(id)
            .ok()
            .flatten()
            .is_some_and(|job| {
                !matches!(
                    job.status,
                    DownloadJobStatus::Paused
                        | DownloadJobStatus::Canceled
                        | DownloadJobStatus::Removing
                )
            })
}

fn prepare_job(state: &AppState, id: &str, interrupted: &AtomicBool) -> Result<(), String> {
    loop {
        if interrupted.load(Ordering::Acquire) || !runnable(state, id) {
            return Ok(());
        }
        let Some(job) = state.download_jobs.get_job(id)? else {
            return Ok(());
        };
        let Some(item) = job
            .items
            .iter()
            .find(|item| item.status == DownloadJobItemStatus::Queued)
        else {
            return Ok(());
        };
        let track_id = item.track_id.clone();
        let result = prepare_track(state, &job, &track_id, interrupted);
        let lock = job_lock(id);
        let _guard = lock.lock();
        if interrupted.load(Ordering::Acquire) || !runnable(state, id) {
            return Ok(());
        }
        let Some(mut job) = state.download_jobs.get_job(id)? else {
            return Ok(());
        };
        let Some(item) = job.items.iter_mut().find(|item| item.track_id == track_id) else {
            continue;
        };
        // An intervening action owns its state; workers only complete queued intent.
        if item.status != DownloadJobItemStatus::Queued {
            continue;
        }
        match result {
            Ok(file) => {
                if item.offline_metadata.is_none() {
                    item.offline_metadata = file.offline_metadata;
                }
                item.status = DownloadJobItemStatus::ReadyToDownload;
                item.profile = job.profile;
                item.download_url = Some(format!(
                    "/api/download/jobs/{}/tracks/{}/file?rendition_id={}",
                    crate::download_files::percent_encode_path_component(id),
                    crate::download_files::percent_encode_path_component(&track_id),
                    file.id
                ));
                item.rendition_id = Some(file.id);
                item.byte_length = Some(file.size);
                item.content_type = Some("audio/ogg; codecs=opus".into());
                item.etag = Some(file.etag);
                item.sha256 = Some(file.sha256);
                item.error = None;
            }
            Err(error) => {
                item.status = DownloadJobItemStatus::Failed;
                item.error = Some(error);
            }
        }
        recount(&mut job);
        job.status = if job
            .items
            .iter()
            .any(|item| item.status == DownloadJobItemStatus::Queued)
        {
            DownloadJobStatus::Queued
        } else if job.ready_count > 0 {
            DownloadJobStatus::ReadyToDownload
        } else {
            DownloadJobStatus::Failed
        };
        let job = state.download_jobs.upsert_job(job)?;
        if !job
            .items
            .iter()
            .any(|item| item.status == DownloadJobItemStatus::Queued)
        {
            // Events contain full immutable job snapshots. One completion event
            // avoids quadratic retained metadata for large collections; clients
            // poll the current job for incremental ready items.
            state.download_jobs.append_event(
                job,
                "job_updated",
                "download file preparation updated",
            )?;
        }
    }
}

fn recount(job: &mut DownloadJob) {
    job.ready_count = job
        .items
        .iter()
        .filter(|item| item.status == DownloadJobItemStatus::ReadyToDownload)
        .count();
    job.failed_count = job
        .items
        .iter()
        .filter(|item| {
            matches!(
                item.status,
                DownloadJobItemStatus::Failed | DownloadJobItemStatus::MetadataFailed
            )
        })
        .count();
}

struct PreparedFile {
    id: String,
    size: u64,
    etag: String,
    sha256: String,
    offline_metadata: Option<crate::offline_metadata::OfflineTrackMetadata>,
}

fn source_identity(path: &Path, profile: DownloadProfile) -> Result<String, String> {
    let metadata = path.metadata().map_err(|error| error.to_string())?;
    let modified = metadata
        .modified()
        .map_err(|error| error.to_string())?
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let key = format!(
        "ogg-opus-{}:{}:{}:{}:{}",
        crate::stream_cache::PRESENTATION_POLICY_VERSION,
        path.display(),
        metadata.len(),
        modified,
        profile.id()
    );
    Ok(blake3::hash(key.as_bytes()).to_hex().to_string())
}

fn prepare_track(
    state: &AppState,
    job: &DownloadJob,
    track_id: &str,
    interrupted: &AtomicBool,
) -> Result<PreparedFile, String> {
    let job_id = job.job_id.as_str();
    let profile = job.profile;
    let (cache_limit, file_limit) = {
        let config = state.config.read();
        crate::config::validate_download_limits(&config)?;
        (
            config.download_cache_max_mib * 1024 * 1024,
            config.download_file_max_mib * 1024 * 1024,
        )
    };
    let library = state
        .library_state
        .read()
        .library
        .clone()
        .ok_or("library unavailable")?;
    let track = library
        .get_track(track_id)
        .map_err(|error| error.to_string())?
        .ok_or("track not found")?;
    let source = library
        .resolve_relpath(&track.file_relpath)
        .ok_or("source unavailable")?;
    let offline_metadata = if job
        .items
        .iter()
        .any(|item| item.track_id == track_id && item.offline_metadata.is_none())
    {
        let liked = crate::api::views::liked_set(state, &job.user_id)
            .map_err(|(_, response)| response.0.error)?;
        let playlists = crate::api::views::playlist_set(state, &job.user_id)
            .map_err(|(_, response)| response.0.error)?;
        Some(crate::offline_metadata::offline_metadata_for_track(
            &library, &track, &liked, &playlists,
        )?)
    } else {
        None
    };
    let id = source_identity(&source, profile)?;
    let path = file_path(state, &id).ok_or("invalid rendition identity")?;
    // Holding this lock bounds disk use during generation and coalesces duplicate encodes.
    let _cache = CACHE_WRITER.get_or_init(Default::default).lock();
    let cancelled = || interrupted.load(Ordering::Acquire) || state.shutdown.is_cancelled();
    if cancelled() || !runnable(state, job_id) {
        return Err("download preparation interrupted".into());
    }
    fs::create_dir_all(root(state)).map_err(|error| format!("create rendition cache: {error}"))?;
    if !is_ready(state, &id) {
        let _ = fs::remove_file(path.with_extension("ready"));
        reserve_space(&root(state), file_limit, cache_limit)?;
        let temp = root(state).join(format!("{id}-{}.part", uuid::Uuid::new_v4()));
        let result = (|| {
            let file = File::create(&temp)
                .map_err(|error| format!("create rendition temporary file: {error}"))?;
            let mut limited = LimitedWriter {
                file,
                written: 0,
                limit: file_limit,
            };
            crate::transcode::file::encode(
                &source,
                profile.bitrate().ok_or("compressed profile required")?,
                &mut limited,
                &cancelled,
            )?;
            limited
                .file
                .sync_all()
                .map_err(|error| format!("flush rendition: {error}"))?;
            drop(limited);
            if source_identity(&source, profile)? != id {
                return Err("source changed during preparation; retry download".into());
            }
            if cancelled() {
                return Err("download preparation interrupted".into());
            }
            publish(&temp, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
    }
    let metadata = path
        .metadata()
        .map_err(|error| format!("read prepared rendition metadata: {error}"))?;
    let mut file =
        File::open(&path).map_err(|error| format!("open prepared rendition: {error}"))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if cancelled() {
            return Err("download preparation interrupted".into());
        }
        let size = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    Ok(PreparedFile {
        id,
        size: metadata.len(),
        etag: crate::download_files::download_etag_from_metadata(&track, &metadata),
        sha256: format!("{:x}", digest.finalize()),
        offline_metadata,
    })
}

struct LimitedWriter {
    file: File,
    written: u64,
    limit: u64,
}

/// Some Windows filesystems deny rename while permitting a normal copy. A
/// separate completion marker keeps both publication paths invisible to readers
/// until all bytes have been flushed; interrupted copies are never served.
fn publish(temp: &Path, path: &Path) -> Result<(), String> {
    if let Err(error) = fs::rename(temp, path) {
        if !matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::CrossesDevices
        ) {
            return Err(format!("publish rendition: {error}"));
        }
        fs::copy(temp, path).map_err(|error| format!("copy prepared rendition: {error}"))?;
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        fs::remove_file(temp).map_err(|error| error.to_string())?;
    }
    File::create(path.with_extension("ready"))
        .and_then(|mut file| {
            file.write_all(crate::stream_cache::PRESENTATION_POLICY_VERSION.as_bytes())?;
            file.sync_all()
        })
        .map_err(|error| format!("commit prepared rendition: {error}"))
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.written.saturating_add(bytes.len() as u64) > self.limit {
            return Err(std::io::Error::other(
                "prepared download exceeds the server's file size limit",
            ));
        }
        let count = self.file.write(bytes)?;
        self.written += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn reserve_space(root: &Path, reserve: u64, cache_limit: u64) -> Result<(), String> {
    let mut entries = Vec::new();
    let mut total = 0u64;
    for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "part")
        {
            // Only one process-local cache writer is active; remnants are interrupted work.
            fs::remove_file(path).map_err(|error| error.to_string())?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "opus")
        {
            let metadata = entry.metadata().map_err(|error| error.to_string())?;
            total = total.saturating_add(metadata.len());
            entries.push((
                metadata.modified().unwrap_or(UNIX_EPOCH),
                path,
                metadata.len(),
            ));
        }
    }
    entries.sort_by_key(|entry| entry.0);
    for (_, path, length) in entries {
        if total.saturating_add(reserve) <= cache_limit {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            let _ = fs::remove_file(path.with_extension("ready"));
            total = total.saturating_sub(length);
        }
    }
    if total.saturating_add(reserve) > cache_limit {
        Err("download rendition cache is full; retry later".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_or_partial_presentation_markers_are_not_ready_renditions() {
        let path =
            std::env::temp_dir().join(format!("phonolite-policy-{}.opus", uuid::Uuid::new_v4()));
        fs::write(&path, b"encoded audio").unwrap();
        let marker = path.with_extension("ready");
        for bytes in [b"".as_slice(), b"presentation-v1", b"presentation-v2-extra"] {
            fs::write(&marker, bytes).unwrap();
            assert!(!ready_path(&path));
        }
        fs::write(&marker, crate::stream_cache::PRESENTATION_POLICY_VERSION).unwrap();
        assert!(ready_path(&path));
        fs::remove_file(marker).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn cache_budget_cleans_interrupted_files_and_retires_completion_markers() {
        let root =
            std::env::temp_dir().join(format!("phonolite-cache-limit-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        for name in ["first", "second"] {
            fs::write(root.join(format!("{name}.opus")), [0u8; 50]).unwrap();
            fs::write(root.join(format!("{name}.ready")), []).unwrap();
        }
        fs::write(root.join("interrupted.part"), [0u8; 12]).unwrap();
        reserve_space(&root, 60, 100).unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn rendition_identity_distinguishes_profiles_and_source_changes() {
        let path =
            std::env::temp_dir().join(format!("phonolite-rendition-{}", uuid::Uuid::new_v4()));
        fs::write(&path, b"first").unwrap();
        let first = source_identity(&path, DownloadProfile::Opus64).unwrap();
        let metadata = path.metadata().unwrap();
        let modified = metadata
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let legacy = format!(
            "ogg-opus-v1:{}:{}:{}:opus_64",
            path.display(),
            metadata.len(),
            modified
        );
        assert_ne!(first, blake3::hash(legacy.as_bytes()).to_hex().to_string());
        assert_ne!(
            first,
            source_identity(&path, DownloadProfile::Opus128).unwrap()
        );
        fs::write(&path, b"second revision").unwrap();
        assert_ne!(
            first,
            source_identity(&path, DownloadProfile::Opus64).unwrap()
        );
        fs::remove_file(path).unwrap();
    }
}
