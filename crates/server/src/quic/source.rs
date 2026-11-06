//! Original-file streams with bounded read-ahead. Source bytes never enter an encoder.
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::shutdown::Shutdown;

pub(super) const MAX_SOURCE_RANGE_BYTES: u32 = 256 * 1024;
const MAX_SOURCE_IO: usize = 16;
const READ_CHUNK_BYTES: usize = 16 * 1024;
pub(super) const SOURCE_CHUNK_BYTES: usize = 64 * 1024;
const SOURCE_QUEUE_CHUNKS: usize = 4;

#[derive(Clone, Debug)]
struct SourceCancellation {
    flag: Arc<AtomicBool>,
    token: CancellationToken,
}

impl SourceCancellation {
    fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            token: CancellationToken::new(),
        }
    }

    fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.token.cancel();
    }
}

#[derive(Debug)]
pub(super) struct SourceGuard {
    cancellation: SourceCancellation,
}

impl Drop for SourceGuard {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(super) struct SourceTransfer {
    pub offset: u64,
    pub length: u64,
    pub total_length: u64,
    pub source_version: String,
    pub rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
    pub guard: SourceGuard,
}

#[derive(Debug)]
pub(super) struct SourceWindow {
    pub offset: u64,
    pub total_length: u64,
    pub source_version: String,
    pub bytes: Bytes,
}

/// A pinned source identity shared with the client's parallel Auto decoder.
#[derive(Clone, Debug, serde::Serialize)]
pub(super) struct OriginalDescriptor {
    pub source_version: String,
    pub total_length: u64,
    pub duration_ms: u32,
}

pub(super) fn describe(
    path: &Path,
    track_id: &str,
    duration_ms: u32,
) -> Result<OriginalDescriptor, String> {
    let file = File::open(path).map_err(|error| format!("source open failed: {error}"))?;
    let identity = SourceIdentity::read(&file, path)?;
    Ok(OriginalDescriptor {
        source_version: identity.version(path, track_id),
        total_length: identity.length,
        duration_ms,
    })
}

pub(super) struct PendingSource {
    pub request_id: u64,
    pub track_id: String,
    cancelled: Option<SourceCancellation>,
    ready: oneshot::Receiver<Result<SourceTransfer, String>>,
}

impl PendingSource {
    pub fn take_ready(&mut self) -> Option<Result<SourceTransfer, String>> {
        match self.ready.try_recv() {
            Ok(result) => {
                // The returned transfer now owns cancellation. Dropping this
                // pending record must not stop a successfully handed-off stream.
                if result.is_ok() {
                    self.cancelled = None;
                }
                Some(result)
            }
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => {
                Some(Err("source range worker stopped".into()))
            }
        }
    }
}

impl Drop for PendingSource {
    fn drop(&mut self) {
        if let Some(cancelled) = &self.cancelled {
            cancelled.cancel();
        }
    }
}

fn slots() -> &'static Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(MAX_SOURCE_IO)))
}

pub(super) fn prepare(
    shutdown: &Shutdown,
    track_id: String,
    path: PathBuf,
    request_id: u64,
    offset: u64,
    length: u32,
    source_version: Option<String>,
) -> Result<PendingSource, String> {
    prepare_with_slots(
        slots(),
        shutdown,
        track_id,
        path,
        request_id,
        offset,
        length,
        source_version,
    )
}

fn prepare_with_slots(
    slots: &Arc<Semaphore>,
    shutdown: &Shutdown,
    track_id: String,
    path: PathBuf,
    request_id: u64,
    offset: u64,
    length: u32,
    source_version: Option<String>,
) -> Result<PendingSource, String> {
    validate_length(length)?;
    if shutdown.is_cancelled() {
        return Err("server shutting down".into());
    }
    let (reply, ready) = oneshot::channel();
    let cancelled = SourceCancellation::new();
    let worker_cancelled = cancelled.clone();
    let worker_track = track_id.clone();
    let stopping = shutdown.flag();
    let lifecycle = shutdown.clone();
    let io_slots = slots.clone();
    shutdown.spawn_graceful_named("original QUIC source range", async move {
        let read_cancelled = worker_cancelled.clone();
        let result = source_io(&io_slots, &lifecycle, &worker_cancelled, move || {
            read_window(
                &path,
                &worker_track,
                offset,
                length,
                source_version.as_deref(),
                &read_cancelled.flag,
                &stopping,
            )
        })
        .await
        .map(|window| SourceTransfer {
            offset: window.offset,
            length: window.bytes.len() as u64,
            total_length: window.total_length,
            source_version: window.source_version,
            rx: window_receiver(window.bytes),
            guard: SourceGuard {
                cancellation: worker_cancelled.clone(),
            },
        });
        let _ = reply.send(result);
    });
    Ok(PendingSource {
        request_id,
        track_id,
        cancelled: Some(cancelled),
        ready,
    })
}

pub(super) fn prepare_open(
    shutdown: &Shutdown,
    track_id: String,
    path: PathBuf,
    request_id: u64,
    offset: u64,
    source_version: Option<String>,
) -> Result<PendingSource, String> {
    prepare_open_with_slots(
        slots(),
        shutdown,
        track_id,
        path,
        request_id,
        offset,
        source_version,
    )
}

fn prepare_open_with_slots(
    slots: &Arc<Semaphore>,
    shutdown: &Shutdown,
    track_id: String,
    path: PathBuf,
    request_id: u64,
    offset: u64,
    source_version: Option<String>,
) -> Result<PendingSource, String> {
    if shutdown.is_cancelled() {
        return Err("server shutting down".into());
    }
    let (reply, ready) = oneshot::channel();
    let cancelled = SourceCancellation::new();
    let worker_cancelled = cancelled.clone();
    let worker_track = track_id.clone();
    let stopping = shutdown.flag();
    let lifecycle = shutdown.clone();
    let io_slots = slots.clone();
    shutdown.spawn_graceful_named("original QUIC source stream", async move {
        let read_cancelled = worker_cancelled.clone();
        let opened = source_io(&io_slots, &lifecycle, &worker_cancelled, move || {
            let (mut file, identity, source_version) = open_source(
                &path,
                &worker_track,
                offset,
                source_version.as_deref(),
                &read_cancelled.flag,
                &stopping,
            )?;
            file.seek(SeekFrom::Start(offset))
                .map_err(|error| format!("source seek failed: {error}"))?;
            Ok((file, identity, source_version, path))
        })
        .await;
        let (file, identity, source_version, path) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let length = identity.length - offset;
        let (tx, rx) = mpsc::channel(SOURCE_QUEUE_CHUNKS);
        let transfer = SourceTransfer {
            offset,
            length,
            total_length: identity.length,
            source_version,
            rx,
            guard: SourceGuard {
                cancellation: worker_cancelled.clone(),
            },
        };
        if reply.send(Ok(transfer)).is_err() {
            return;
        }
        let result = produce_stream(
            file,
            length,
            &tx,
            &worker_cancelled,
            &lifecycle,
            &io_slots,
            move |file| {
                if SourceIdentity::read(file, &path)? != identity {
                    return Err("source changed during stream".into());
                }
                Ok(())
            },
        )
        .await;
        if let Err(error) = result {
            if !worker_cancelled.flag.load(Ordering::Acquire) && !lifecycle.is_cancelled() {
                let _ = send_cancellable(
                    &tx,
                    Err(std::io::Error::other(error)),
                    &worker_cancelled,
                    &lifecycle,
                )
                .await;
            }
        }
    });
    Ok(PendingSource {
        request_id,
        track_id,
        cancelled: Some(cancelled),
        ready,
    })
}

pub(super) fn window_receiver(bytes: Bytes) -> mpsc::Receiver<Result<Bytes, std::io::Error>> {
    let (tx, rx) = mpsc::channel(bytes.len().div_ceil(SOURCE_CHUNK_BYTES).max(1));
    for start in (0..bytes.len()).step_by(SOURCE_CHUNK_BYTES) {
        tx.try_send(Ok(
            bytes.slice(start..bytes.len().min(start + SOURCE_CHUNK_BYTES))
        ))
        .expect("bounded source window channel");
    }
    rx
}

async fn produce_stream<R, V>(
    mut input: R,
    mut remaining: u64,
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    cancelled: &SourceCancellation,
    shutdown: &Shutdown,
    io_slots: &Arc<Semaphore>,
    mut verify_identity: V,
) -> Result<(), String>
where
    R: Read + Send + 'static,
    V: FnMut(&R) -> Result<(), String> + Send + 'static,
{
    while remaining > 0 {
        // Reserve consumer capacity before joining the fair I/O queue. Paused
        // and prepared streams retain only their bounded buffers and file;
        // they occupy neither a blocking thread nor a scarce read permit.
        let destination = reserve_send(tx, cancelled, shutdown).await?;
        let length = remaining.min(SOURCE_CHUNK_BYTES as u64) as usize;
        let read_cancelled = cancelled.clone();
        let stopping = shutdown.flag();
        (input, verify_identity, remaining) = source_io(io_slots, shutdown, cancelled, move || {
            verify_identity(&input)?;
            let bytes = read_next_bytes(&mut input, length, &read_cancelled.flag, &stopping)?;
            verify_identity(&input)?;
            Ok((input, verify_identity, (bytes, remaining - length as u64)))
        })
        .await
        .map(|(input, verify_identity, (bytes, remaining))| {
            destination.send(Ok(bytes));
            (input, verify_identity, remaining)
        })?;
    }
    source_io(io_slots, shutdown, cancelled, move || {
        verify_identity(&input)
    })
    .await
}

async fn reserve_send<'a>(
    tx: &'a mpsc::Sender<Result<Bytes, std::io::Error>>,
    cancelled: &SourceCancellation,
    shutdown: &Shutdown,
) -> Result<mpsc::Permit<'a, Result<Bytes, std::io::Error>>, String> {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => Err("server shutting down".into()),
        _ = cancelled.token.cancelled() => Err("source stream cancelled".into()),
        permit = tx.reserve() => permit.map_err(|_| "source stream cancelled".into()),
    }
}

async fn send_cancellable(
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    message: Result<Bytes, std::io::Error>,
    cancelled: &SourceCancellation,
    shutdown: &Shutdown,
) -> Result<(), String> {
    reserve_send(tx, cancelled, shutdown).await?.send(message);
    Ok(())
}

async fn source_io<T, F>(
    slots: &Arc<Semaphore>,
    shutdown: &Shutdown,
    cancelled: &SourceCancellation,
    work: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    let permit = tokio::select! {
        biased;
        _ = shutdown.cancelled() => return Err("server shutting down".into()),
        _ = cancelled.token.cancelled() => return Err("source stream cancelled".into()),
        permit = slots.clone().acquire_owned() => permit.map_err(|_| "source I/O unavailable".to_owned())?,
    };
    let work_cancelled = cancelled.clone();
    let stopping = shutdown.flag();
    let worker = shutdown.spawn_blocking_named("original QUIC source I/O", move || {
        // A cancelled owner cannot release capacity while native I/O still
        // runs. The detached, tracked worker owns both its file and permit.
        let _permit = permit;
        check_cancelled(&work_cancelled.flag, &stopping)?;
        work()
    });
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => Err("server shutting down".into()),
        _ = cancelled.token.cancelled() => Err("source stream cancelled".into()),
        result = worker => result.map_err(|_| "source I/O worker stopped".to_owned())?,
    }
}

fn validate_length(length: u32) -> Result<(), String> {
    if length == 0 || length > MAX_SOURCE_RANGE_BYTES {
        return Err(format!(
            "source range length must be between 1 and {MAX_SOURCE_RANGE_BYTES} bytes"
        ));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct SourceIdentity {
    length: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl SourceIdentity {
    fn from_metadata(metadata: &Metadata) -> Result<Self, String> {
        if !metadata.is_file() {
            return Err("source is not a regular file".into());
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified().map_err(|error| error.to_string())?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }

    fn read(file: &File, path: &Path) -> Result<Self, String> {
        let opened = Self::from_metadata(&file.metadata().map_err(|error| error.to_string())?)?;
        let current = Self::from_metadata(&path.metadata().map_err(|error| error.to_string())?)?;
        if opened != current {
            return Err("source changed during range request".into());
        }
        Ok(opened)
    }

    fn version(&self, path: &Path, track_id: &str) -> String {
        blake3::hash(format!("{track_id}:{}:{self:?}", path.display()).as_bytes())
            .to_hex()
            .to_string()
    }
}

fn check_cancelled(cancelled: &AtomicBool, stopping: &AtomicBool) -> Result<(), String> {
    if stopping.load(Ordering::Acquire) {
        return Err("server shutting down".into());
    }
    if cancelled.load(Ordering::Acquire) {
        return Err("source range cancelled".into());
    }
    Ok(())
}

fn read_window(
    path: &Path,
    track_id: &str,
    offset: u64,
    length: u32,
    expected_version: Option<&str>,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
) -> Result<SourceWindow, String> {
    validate_length(length)?;
    let (mut file, identity, source_version) = open_source(
        path,
        track_id,
        offset,
        expected_version,
        cancelled,
        stopping,
    )?;
    let actual_length = u64::from(length).min(identity.length - offset) as usize;
    let bytes = read_bytes(&mut file, offset, actual_length, cancelled, stopping)?;
    // Publish only a fully read and validated window. A replacement/truncation
    // cannot race a successful FIN containing stale or incomplete bytes.
    if SourceIdentity::read(&file, path)? != identity {
        return Err("source changed during range request".into());
    }
    check_cancelled(cancelled, stopping)?;
    Ok(SourceWindow {
        offset,
        total_length: identity.length,
        source_version,
        bytes,
    })
}

fn open_source(
    path: &Path,
    track_id: &str,
    offset: u64,
    expected_version: Option<&str>,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
) -> Result<(File, SourceIdentity, String), String> {
    check_cancelled(cancelled, stopping)?;
    let file = File::open(path).map_err(|error| format!("source open failed: {error}"))?;
    let identity = SourceIdentity::read(&file, path)?;
    let source_version = identity.version(path, track_id);
    if expected_version.is_some_and(|expected| expected != source_version) {
        return Err("source changed; reopen original playback".into());
    }
    if offset > identity.length {
        return Err("source range offset exceeds file length".into());
    }
    Ok((file, identity, source_version))
}

fn read_bytes(
    input: &mut (impl Read + Seek),
    offset: u64,
    length: usize,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
) -> Result<Bytes, String> {
    check_cancelled(cancelled, stopping)?;
    input
        .seek(SeekFrom::Start(offset))
        .map_err(|error| format!("source seek failed: {error}"))?;
    read_next_bytes(input, length, cancelled, stopping)
}

fn read_next_bytes(
    input: &mut impl Read,
    length: usize,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
) -> Result<Bytes, String> {
    let mut bytes = vec![0; length];
    let mut read = 0;
    while read < length {
        check_cancelled(cancelled, stopping)?;
        let end = length.min(read + READ_CHUNK_BYTES);
        let count = input
            .read(&mut bytes[read..end])
            .map_err(|error| format!("source read failed: {error}"))?;
        if count == 0 {
            return Err("source ended before requested range was complete".into());
        }
        read += count;
    }
    check_cancelled(cancelled, stopping)?;
    Ok(Bytes::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct SourceFile(PathBuf);

    impl SourceFile {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "phonolite-quic-source-{}.flac",
                uuid::Uuid::new_v4()
            ));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for SourceFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn window(
        path: &Path,
        offset: u64,
        length: u32,
        version: Option<&str>,
    ) -> Result<SourceWindow, String> {
        read_window(
            path,
            "original",
            offset,
            length,
            version,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        )
    }

    async fn ready(pending: &mut PendingSource) -> Result<SourceTransfer, String> {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(result) = pending.take_ready() {
                    break result;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("source metadata must complete")
    }

    #[tokio::test]
    async fn original_open_streams_many_megabytes_and_reopens_exact_seek_offsets() {
        let bytes: Vec<u8> = (0..4 * 1024 * 1024 + 23)
            .map(|index| (index * 37) as u8)
            .collect();
        let source = SourceFile::new(&bytes);
        let slots = Arc::new(Semaphore::new(1));
        let shutdown = Shutdown::new();
        let mut version = None;
        for offset in [0, 3 * 1024 * 1024 + 7, bytes.len() as u64] {
            let mut pending = prepare_open_with_slots(
                &slots,
                &shutdown,
                "original".into(),
                source.0.clone(),
                offset + 1,
                offset,
                version.clone(),
            )
            .unwrap();
            let mut transfer = ready(&mut pending).await.unwrap();
            drop(pending);
            assert!(
                !transfer.guard.cancellation.flag.load(Ordering::Acquire),
                "metadata handoff must not cancel the live producer"
            );
            assert_eq!(transfer.offset, offset);
            assert_eq!(transfer.total_length, bytes.len() as u64);
            assert_eq!(transfer.length, bytes.len() as u64 - offset);
            if let Some(version) = &version {
                assert_eq!(transfer.source_version, *version);
            }
            version = Some(transfer.source_version.clone());
            let mut actual = Vec::new();
            while let Some(chunk) = transfer.rx.recv().await {
                let chunk = chunk.unwrap();
                assert!(chunk.len() <= SOURCE_CHUNK_BYTES);
                actual.extend_from_slice(&chunk);
            }
            assert_eq!(actual, bytes[offset as usize..]);
            assert_eq!(
                slots.available_permits(),
                1,
                "a completed producer releases I/O even while its transfer is retained"
            );
            drop(transfer);
            assert_eq!(slots.available_permits(), 1);
        }
        assert!(shutdown.drain(std::time::Duration::from_secs(2)).await);
    }

    #[tokio::test]
    async fn original_stream_queue_bounds_read_ahead_and_cancel_or_shutdown_unblocks_a_full_receiver(
    ) {
        struct CountingReader {
            input: Cursor<Vec<u8>>,
            read_bytes: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Read for CountingReader {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                let count = self.input.read(bytes)?;
                self.read_bytes.fetch_add(count, Ordering::Release);
                Ok(count)
            }
        }
        for shutdown_requested in [false, true] {
            let read_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let input = CountingReader {
                input: Cursor::new(vec![7; 4 * 1024 * 1024]),
                read_bytes: read_bytes.clone(),
            };
            let (tx, rx) = mpsc::channel(SOURCE_QUEUE_CHUNKS);
            let cancelled = SourceCancellation::new();
            let worker_cancelled = cancelled.clone();
            let shutdown = Shutdown::new();
            let worker_shutdown = shutdown.clone();
            let slots = Arc::new(Semaphore::new(1));
            let worker_slots = slots.clone();
            let worker = tokio::spawn(async move {
                produce_stream(
                    input,
                    4 * 1024 * 1024,
                    &tx,
                    &worker_cancelled,
                    &worker_shutdown,
                    &worker_slots,
                    |_| Ok(()),
                )
                .await
            });
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
            let maximum_read = SOURCE_QUEUE_CHUNKS * SOURCE_CHUNK_BYTES;
            while rx.len() < SOURCE_QUEUE_CHUNKS {
                assert!(tokio::time::Instant::now() < deadline);
                tokio::task::yield_now().await;
            }
            assert_eq!(read_bytes.load(Ordering::Acquire), maximum_read);
            assert_eq!(
                slots.available_permits(),
                1,
                "backpressure holds no I/O permit"
            );
            if shutdown_requested {
                shutdown.cancel();
            } else {
                cancelled.cancel();
            }
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), worker)
                .await
                .unwrap()
                .unwrap();
            let error = result.unwrap_err();
            assert!(error.contains(if shutdown_requested {
                "shutting down"
            } else {
                "cancelled"
            }));
            assert_eq!(
                rx.len(),
                SOURCE_QUEUE_CHUNKS,
                "the receiver stays alive and full until the worker has stopped"
            );
            assert_eq!(read_bytes.load(Ordering::Acquire), maximum_read);
            drop(rx);
            assert!(shutdown.drain(std::time::Duration::from_secs(2)).await);
        }
    }

    #[tokio::test]
    async fn original_open_detects_truncation_and_rejects_stale_versions_without_successful_eof() {
        let source = SourceFile::new(&vec![11; 2 * 1024 * 1024]);
        let slots = Arc::new(Semaphore::new(1));
        let shutdown = Shutdown::new();
        let mut pending = prepare_open_with_slots(
            &slots,
            &shutdown,
            "original".into(),
            source.0.clone(),
            1,
            0,
            None,
        )
        .unwrap();
        let mut transfer = ready(&mut pending).await.unwrap();
        drop(pending);
        let version = transfer.source_version.clone();
        assert!(transfer.rx.recv().await.unwrap().is_ok());
        // The bounded channel prevents the producer from finishing this source
        // before the replacement is observed, regardless of filesystem speed.
        std::fs::write(&source.0, b"replaced").unwrap();
        let mut error = None;
        while let Some(chunk) = transfer.rx.recv().await {
            if let Err(failure) = chunk {
                error = Some(failure.to_string());
                break;
            }
        }
        let error = error.expect("source mutation must be an error rather than successful EOF");
        assert!(error.contains("source changed") || error.contains("source ended"));
        drop(transfer);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while slots.available_permits() == 0 {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        let mut pending = prepare_open_with_slots(
            &slots,
            &shutdown,
            "original".into(),
            source.0.clone(),
            2,
            0,
            Some(version),
        )
        .unwrap();
        let failure = ready(&mut pending)
            .await
            .err()
            .expect("a stale source cannot be reopened");
        assert!(failure.contains("source changed"));
        drop(pending);
        assert!(shutdown.drain(std::time::Duration::from_secs(2)).await);
    }

    #[tokio::test]
    async fn original_open_cancelled_while_backpressured_releases_the_slot_and_source() {
        let source = SourceFile::new(&vec![13; 2 * 1024 * 1024]);
        let slots = Arc::new(Semaphore::new(1));
        let shutdown = Shutdown::new();
        let mut pending = prepare_open_with_slots(
            &slots,
            &shutdown,
            "original".into(),
            source.0.clone(),
            1,
            0,
            None,
        )
        .unwrap();
        let transfer = ready(&mut pending).await.unwrap();
        drop(pending);
        drop(transfer);
        assert!(shutdown.drain(std::time::Duration::from_secs(2)).await);
        assert_eq!(slots.available_permits(), 1);
        assert!(shutdown.pending_jobs().is_empty());
    }

    #[tokio::test]
    async fn original_stream_short_reads_fail_the_announced_continuous_length() {
        let (tx, mut rx) = mpsc::channel(SOURCE_QUEUE_CHUNKS);
        let shutdown = Shutdown::new();
        let result = produce_stream(
            Cursor::new(vec![1; SOURCE_CHUNK_BYTES]),
            SOURCE_CHUNK_BYTES as u64 + 1,
            &tx,
            &SourceCancellation::new(),
            &shutdown,
            &Arc::new(Semaphore::new(1)),
            |_| Ok(()),
        )
        .await;
        assert!(result.unwrap_err().contains("source ended"));
        assert_eq!(rx.try_recv().unwrap().unwrap().len(), SOURCE_CHUNK_BYTES);
        assert!(rx.try_recv().is_err());
        assert!(shutdown.drain(std::time::Duration::from_secs(2)).await);
    }

    #[test]
    fn auto_descriptor_pins_the_same_identity_as_original_reads() {
        let source = SourceFile::new(b"exact original bytes");
        let descriptor = describe(&source.0, "original", 20_000).unwrap();
        let read = window(&source.0, 0, 20, Some(&descriptor.source_version)).unwrap();
        assert_eq!(descriptor.source_version, read.source_version);
        assert_eq!(descriptor.total_length, read.total_length);
        assert_eq!(descriptor.duration_ms, 20_000);
        std::fs::write(&source.0, b"replacement file content").unwrap();
        assert!(window(&source.0, 0, 20, Some(&descriptor.source_version)).is_err());
    }

    #[test]
    fn original_windows_keep_source_bytes_and_validate_eof_and_versions() {
        let bytes: Vec<u8> = (0..MAX_SOURCE_RANGE_BYTES as usize + 23)
            .map(|index| (index * 37) as u8)
            .collect();
        let source = SourceFile::new(&bytes);
        let first = window(&source.0, 0, MAX_SOURCE_RANGE_BYTES, None).unwrap();
        assert_eq!(
            first.bytes.as_ref(),
            &bytes[..MAX_SOURCE_RANGE_BYTES as usize]
        );
        assert_eq!(first.total_length, bytes.len() as u64);
        let tail = window(
            &source.0,
            u64::from(MAX_SOURCE_RANGE_BYTES),
            MAX_SOURCE_RANGE_BYTES,
            Some(&first.source_version),
        )
        .unwrap();
        assert_eq!(
            tail.bytes.as_ref(),
            &bytes[MAX_SOURCE_RANGE_BYTES as usize..]
        );
        assert_eq!(tail.source_version, first.source_version);
        let eof = window(
            &source.0,
            bytes.len() as u64,
            1,
            Some(&first.source_version),
        )
        .unwrap();
        assert!(eof.bytes.is_empty());
        assert!(window(&source.0, bytes.len() as u64 + 1, 1, None).is_err());
        assert!(window(&source.0, u64::MAX, 1, None).is_err());
        assert!(window(&source.0, 0, 0, None).is_err());
        assert!(window(&source.0, 0, MAX_SOURCE_RANGE_BYTES + 1, None).is_err());
        std::fs::write(&source.0, b"replacement").unwrap();
        assert!(window(&source.0, 0, 1, Some(&first.source_version))
            .unwrap_err()
            .contains("source changed"));
    }

    #[test]
    fn original_reads_reject_short_files_and_cancel_between_chunks() {
        let cancelled = AtomicBool::new(false);
        let stopping = AtomicBool::new(false);
        assert!(read_bytes(&mut Cursor::new(b"short"), 0, 8, &cancelled, &stopping).is_err());
        struct CancellingReader<'a> {
            input: Cursor<Vec<u8>>,
            cancelled: &'a AtomicBool,
        }
        impl Read for CancellingReader<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                let count = self.input.read(bytes)?;
                self.cancelled.store(true, Ordering::Release);
                Ok(count)
            }
        }
        impl Seek for CancellingReader<'_> {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                self.input.seek(position)
            }
        }
        let mut input = CancellingReader {
            input: Cursor::new(vec![1; READ_CHUNK_BYTES * 2]),
            cancelled: &cancelled,
        };
        assert!(
            read_bytes(&mut input, 0, READ_CHUNK_BYTES * 2, &cancelled, &stopping)
                .unwrap_err()
                .contains("cancelled")
        );
        assert_eq!(input.input.position(), READ_CHUNK_BYTES as u64);
    }

    #[tokio::test]
    async fn original_window_releases_io_before_transfer_drop_and_shutdown_rejects_new_work() {
        let source = SourceFile::new(b"source bytes");
        let slots = Arc::new(Semaphore::new(1));
        let shutdown = Shutdown::new();
        let mut pending = prepare_with_slots(
            &slots,
            &shutdown,
            "original".into(),
            source.0.clone(),
            1,
            0,
            256,
            None,
        )
        .unwrap();
        let window = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(result) = pending.take_ready() {
                    break result.unwrap();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(slots.available_permits(), 1);
        let mut next = prepare_with_slots(
            &slots,
            &shutdown,
            "original".into(),
            source.0.clone(),
            2,
            0,
            256,
            None,
        )
        .unwrap();
        let next_window = ready(&mut next).await.unwrap();
        assert_eq!(next_window.length, window.length);
        drop(pending);
        drop(next);
        assert_eq!(slots.available_permits(), 1);
        drop(window);
        drop(next_window);
        assert_eq!(slots.available_permits(), 1);
        shutdown.cancel();
        assert!(prepare_with_slots(
            &slots,
            &shutdown,
            "original".into(),
            source.0.clone(),
            3,
            0,
            256,
            None
        )
        .is_err());
        assert!(shutdown.drain(std::time::Duration::from_secs(2)).await);
    }

    #[tokio::test]
    async fn twenty_four_backpressured_sources_leave_io_available_for_new_playback() {
        let source = SourceFile::new(&vec![17; 2 * 1024 * 1024]);
        let slots = Arc::new(Semaphore::new(MAX_SOURCE_IO));
        let shutdown = Shutdown::new();
        let mut paused = Vec::new();
        for request_id in 0..24 {
            let mut pending = prepare_open_with_slots(
                &slots,
                &shutdown,
                "prepared".into(),
                source.0.clone(),
                request_id,
                0,
                None,
            )
            .unwrap();
            paused.push(ready(&mut pending).await.unwrap());
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while paused
                .iter()
                .any(|stream| stream.rx.len() < SOURCE_QUEUE_CHUNKS)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            slots.available_permits(),
            MAX_SOURCE_IO,
            "paused streams retain neither I/O capacity nor a blocking reader"
        );
        assert!(shutdown
            .pending_jobs()
            .iter()
            .all(|job| job.name != "original QUIC source I/O"));
        let mut pending = prepare_open_with_slots(
            &slots,
            &shutdown,
            "foreground".into(),
            source.0.clone(),
            25,
            0,
            None,
        )
        .unwrap();
        let mut foreground = ready(&mut pending).await.unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), foreground.rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first.len(), SOURCE_CHUNK_BYTES);
        drop(foreground);
        drop(pending);
        drop(paused);
        assert!(shutdown.drain(std::time::Duration::from_secs(2)).await);
        assert_eq!(slots.available_permits(), MAX_SOURCE_IO);
        assert!(shutdown.pending_jobs().is_empty());
    }

    #[tokio::test]
    async fn cancelled_io_keeps_its_permit_until_the_blocked_read_really_finishes() {
        let slots = Arc::new(Semaphore::new(1));
        let shutdown = Shutdown::new();
        let cancelled = SourceCancellation::new();
        let worker_slots = slots.clone();
        let worker_shutdown = shutdown.clone();
        let worker_cancelled = cancelled.clone();
        let (release, blocked) = std::sync::mpsc::channel();
        let (started, running) = oneshot::channel();
        let owner = tokio::spawn(async move {
            source_io(
                &worker_slots,
                &worker_shutdown,
                &worker_cancelled,
                move || {
                    started.send(()).unwrap();
                    blocked
                        .recv_timeout(std::time::Duration::from_secs(2))
                        .unwrap();
                    Ok(())
                },
            )
            .await
        });
        running.await.unwrap();
        cancelled.cancel();
        assert!(owner.await.unwrap().unwrap_err().contains("cancelled"));
        assert_eq!(
            slots.available_permits(),
            0,
            "a cancelled response must not admit another physical read yet"
        );
        release.send(()).unwrap();
        assert!(shutdown.drain(std::time::Duration::from_secs(2)).await);
        assert_eq!(slots.available_permits(), 1);
    }
}
