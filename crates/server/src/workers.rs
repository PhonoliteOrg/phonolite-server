//! Killable, database-free source readers and media computations.
//!
//! Only the parent publishes results. Worker mode is dispatched before server
//! initialization, receives one bounded request on stdin and exits after one job.
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
#[cfg(not(test))]
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
#[cfg(not(test))]
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::shutdown::Shutdown;
use crate::stream_cache::CacheWriter;
use crate::transcode::{BitrateSelector, RawOpusMeta, TranscodeQuality, TranscodeSink};

const ARGUMENT: &str = "--phonolite-internal-worker-v1";
const VERSION: u8 = 1;
const MAX_REQUEST: usize = 256 * 1024;
const MAX_FRAME: usize = 1024 * 1024;
const MAX_OUTPUT: usize = 32 * 1024 * 1024;
const JOB_TIMEOUT: Duration = Duration::from_secs(60);
const REAP_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(not(test))]
const POLL: Duration = Duration::from_millis(10);
const META: u8 = 1;
const DATA: u8 = 2;
const AUDIO_HEADER: u8 = 3;
const AUDIO_FRAME: u8 = 4;
const DONE: u8 = 5;
const ERROR: u8 = 6;

#[derive(Serialize, Deserialize)]
struct Request {
    version: u8,
    job: Job,
}

#[derive(Serialize, Deserialize)]
enum Job {
    Tags {
        source: PathBuf,
    },
    Cover {
        source: PathBuf,
    },
    Thumbnail {
        source: PathBuf,
        size: u32,
        expected_hash: Option<String>,
        limits: crate::resources::ResourceLimits,
    },
    Transcode {
        source: PathBuf,
        quality: TranscodeQuality,
        frame_ms: u32,
        meta: RawOpusMeta,
        start_ms: u32,
        end_ms: u32,
    },
}
impl Job {
    fn source(&self) -> &Path {
        match self {
            Self::Tags { source }
            | Self::Cover { source }
            | Self::Thumbnail { source, .. }
            | Self::Transcode { source, .. } => source,
        }
    }
    #[cfg(not(test))]
    fn name(&self) -> &'static str {
        match self {
            Self::Tags { .. } => "tag reader",
            Self::Cover { .. } => "embedded cover",
            Self::Thumbnail { .. } => "thumbnail decoder",
            Self::Transcode { .. } => "range transcoder",
        }
    }
}

#[derive(Serialize, Deserialize)]
enum Metadata {
    Tags(metadata::TagInfo),
    Cover { present: bool, mime: Option<String> },
    Thumbnail,
}

enum Output {
    Tags(metadata::TagInfo),
    Cover(Option<metadata::CoverArt>),
    Thumbnail(Vec<u8>),
    Transcode,
}

/// Returns before configuration, logging, databases or listeners are opened.
pub(crate) fn dispatch_if_worker() -> Option<Result<(), String>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(ARGUMENT)) {
        return None;
    }
    Some((|| {
        if args.next().is_some() {
            return Err("unexpected worker arguments".into());
        }
        let request = read_request(&mut io::stdin().lock())?;
        // The parent retains stdin for the whole job. If it dies abruptly,
        // stop native parsing/decoding here rather than leaving an orphan.
        std::thread::Builder::new()
            .name("source-worker-parent".into())
            .spawn(|| {
                let mut byte = [0; 1];
                let _ = io::stdin().read(&mut byte);
                std::process::exit(125);
            })
            .map_err(|error| error.to_string())?;
        let mut output = io::stdout().lock();
        match perform(request, &mut output) {
            Ok(()) => write_frame(&mut output, DONE, &[]),
            Err(error) => {
                // Bound even errors from an unusually verbose codec.
                let error = error.chars().take(2048).collect::<String>();
                write_frame(&mut output, ERROR, error.as_bytes())
            }
        }
    })())
}

fn perform(request: Request, output: &mut impl Write) -> Result<(), String> {
    if request.version != VERSION {
        return Err("unsupported source worker protocol".into());
    }
    let source = request.job.source().to_owned();
    let identity = SourceIdentity::read(&source)?;
    match request.job {
        Job::Tags { source } => {
            let tags = metadata::read_tags(&source).map_err(|error| format!("{error:?}"))?;
            send_metadata(output, &Metadata::Tags(tags))
        }
        Job::Cover { source } => {
            let cover = metadata::read_cover(&source).map_err(|error| format!("{error:?}"))?;
            send_metadata(
                output,
                &Metadata::Cover {
                    present: cover.is_some(),
                    mime: cover.as_ref().and_then(|cover| cover.mime.clone()),
                },
            )?;
            if let Some(cover) = cover {
                send_data(output, &cover.data)?;
            }
            Ok(())
        }
        Job::Thumbnail {
            source,
            size,
            expected_hash,
            limits,
        } => {
            if !crate::artwork_cache::ArtworkCache::valid_size(size) {
                return Err("unsupported artwork size".into());
            }
            limits.validate().map_err(str::to_owned)?;
            let data = crate::artwork_cache::render_thumbnail(
                &source,
                size,
                expected_hash.as_deref(),
                &limits,
            )?;
            send_metadata(output, &Metadata::Thumbnail)?;
            send_data(output, &data)
        }
        Job::Transcode {
            source,
            quality,
            frame_ms,
            meta,
            start_ms,
            end_ms,
        } => {
            if end_ms < start_ms || end_ms - start_ms > crate::stream_cache::CACHE_RANGE_MS {
                return Err("worker range exceeds range limit".into());
            }
            let mut sink = PipeSink { output, total: 0 };
            crate::transcode::transcode_raw_opus_range_cache(
                &source,
                BitrateSelector {
                    quality,
                    fixed_bitrate_bps: None,
                },
                frame_ms,
                meta,
                start_ms,
                end_ms,
                &mut sink,
            )
        }
    }?;
    if SourceIdentity::read(&source)? != identity {
        return Err("source changed during worker computation".into());
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct SourceIdentity {
    length: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}
impl SourceIdentity {
    fn read(source: &Path) -> Result<Self, String> {
        if !source.is_absolute() {
            return Err("worker requires an absolute source path".into());
        }
        let metadata = std::fs::metadata(source).map_err(|error| error.to_string())?;
        if !metadata.is_file() {
            return Err("source worker accepts regular files only".into());
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
}

fn absolute(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(path))
    }
}

fn encode_request(job: Job) -> Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec(&Request {
        version: VERSION,
        job,
    })
    .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_REQUEST {
        return Err("source worker request exceeds limit".into());
    }
    let mut framed = Vec::with_capacity(4 + bytes.len());
    framed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    framed.extend(bytes);
    Ok(framed)
}

fn read_request(input: &mut impl Read) -> Result<Request, String> {
    let mut prefix = [0; 4];
    input
        .read_exact(&mut prefix)
        .map_err(|error| error.to_string())?;
    let length = u32::from_le_bytes(prefix) as usize;
    if length > MAX_REQUEST {
        return Err("source worker request exceeds limit".into());
    }
    let mut bytes = vec![0; length];
    input
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

fn write_frame(output: &mut impl Write, kind: u8, data: &[u8]) -> Result<(), String> {
    if data.len() > MAX_FRAME {
        return Err("source worker frame exceeds limit".into());
    }
    output
        .write_all(&[VERSION, kind])
        .map_err(|error| error.to_string())?;
    output
        .write_all(&(data.len() as u32).to_le_bytes())
        .map_err(|error| error.to_string())?;
    output.write_all(data).map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())
}

fn send_metadata(output: &mut impl Write, meta: &Metadata) -> Result<(), String> {
    write_frame(
        output,
        META,
        &serde_json::to_vec(meta).map_err(|error| error.to_string())?,
    )
}

fn send_data(output: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_OUTPUT {
        return Err("source worker result exceeds limit".into());
    }
    for chunk in bytes.chunks(MAX_FRAME) {
        write_frame(output, DATA, chunk)?;
    }
    Ok(())
}

struct PipeSink<'a, W> {
    output: &'a mut W,
    total: usize,
}
impl<W: Write> PipeSink<'_, W> {
    fn send(&mut self, kind: u8, bytes: &[u8]) -> io::Result<()> {
        self.total = self.total.saturating_add(bytes.len());
        if self.total > 4 * 1024 * 1024 {
            return Err(io::Error::other("worker audio range exceeds cache limit"));
        }
        write_frame(self.output, kind, bytes).map_err(io::Error::other)
    }
}
impl<W: Write> TranscodeSink for PipeSink<'_, W> {
    fn check_cancelled(&self) -> io::Result<()> {
        Ok(())
    }
    fn write_header(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.send(AUDIO_HEADER, bytes)
    }
    fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.send(AUDIO_FRAME, bytes)
    }
}

/// The supervisor owns both capacity and partial cache data until kill/reap.
struct Collector {
    metadata: Option<Metadata>,
    bytes: Vec<u8>,
    total: usize,
    writer: Option<CacheWriter>,
    audio_started: bool,
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl Collector {
    fn plain() -> Self {
        Self {
            metadata: None,
            bytes: Vec::new(),
            total: 0,
            writer: None,
            audio_started: false,
            _permit: None,
        }
    }
    fn accept(&mut self, kind: u8, data: Vec<u8>) -> Result<bool, String> {
        self.total = self.total.saturating_add(data.len());
        if self.total > MAX_OUTPUT {
            return Err("source worker result exceeds limit".into());
        }
        match kind {
            META if self.writer.is_none() && self.metadata.is_none() => {
                self.metadata =
                    Some(serde_json::from_slice(&data).map_err(|error| error.to_string())?);
            }
            DATA if matches!(
                self.metadata,
                Some(Metadata::Cover { .. } | Metadata::Thumbnail)
            ) =>
            {
                self.bytes.extend(data)
            }
            AUDIO_HEADER
                if self.writer.is_some()
                    && !self.audio_started
                    && data.len() >= 40
                    && &data[..8] == b"OPUSR01\0" =>
            {
                self.writer
                    .as_mut()
                    .unwrap()
                    .write_header(&data)
                    .map_err(|error| error.to_string())?;
                self.audio_started = true;
            }
            AUDIO_FRAME
                if self.writer.is_some()
                    && self.audio_started
                    && data.len() >= 2
                    && u16::from_le_bytes(data[..2].try_into().unwrap()) as usize
                        == data.len() - 2 =>
            {
                self.writer
                    .as_mut()
                    .unwrap()
                    .write_frame(&data)
                    .map_err(|error| error.to_string())?;
            }
            DONE if data.is_empty() => return Ok(true),
            ERROR => return Err(String::from_utf8_lossy(&data).into_owned()),
            _ => return Err("unexpected source worker response".into()),
        }
        Ok(false)
    }
    fn finish(mut self) -> Result<Output, String> {
        if let Some(writer) = self.writer.take() {
            if !self.audio_started {
                return Err("source worker returned no audio header".into());
            }
            writer.finalize().map_err(|error| error.to_string())?;
            return Ok(Output::Transcode);
        }
        match self.metadata {
            Some(Metadata::Tags(tags)) => Ok(Output::Tags(tags)),
            Some(Metadata::Cover { present, mime }) => {
                Ok(Output::Cover(present.then_some(metadata::CoverArt {
                    data: self.bytes,
                    mime,
                })))
            }
            Some(Metadata::Thumbnail) => Ok(Output::Thumbnail(self.bytes)),
            None => Err("source worker returned no result".into()),
        }
    }
}

fn frame_length(header: &[u8; 6]) -> Result<usize, String> {
    if header[0] != VERSION {
        return Err("unsupported source worker response version".into());
    }
    let length = u32::from_le_bytes(header[2..6].try_into().unwrap()) as usize;
    if length > MAX_FRAME {
        return Err("source worker frame exceeds limit".into());
    }
    Ok(length)
}

async fn receive(
    input: &mut (impl tokio::io::AsyncRead + Unpin),
    collector: &mut Collector,
) -> Result<(), String> {
    loop {
        let mut header = [0; 6];
        input
            .read_exact(&mut header)
            .await
            .map_err(|error| error.to_string())?;
        let mut bytes = vec![0; frame_length(&header)?];
        input
            .read_exact(&mut bytes)
            .await
            .map_err(|error| error.to_string())?;
        if collector.accept(header[1], bytes)? {
            return Ok(());
        }
    }
}

type ProcessFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;
trait WorkerProcess: Send {
    type Input: tokio::io::AsyncWrite + Unpin + Send;
    type Output: tokio::io::AsyncRead + Unpin + Send;
    fn take_input(&mut self) -> Option<Self::Input>;
    fn take_output(&mut self) -> Option<Self::Output>;
    fn wait(&mut self) -> ProcessFuture<'_, bool>;
    fn terminate(&mut self) -> ProcessFuture<'_, ()>;
}
#[cfg(not(test))]
struct ChildProcess {
    child: crate::process_metrics::worker_cpu::TrackedChild,
    input: Option<tokio::process::ChildStdin>,
    output: Option<tokio::process::ChildStdout>,
}
#[cfg(not(test))]
impl WorkerProcess for ChildProcess {
    type Input = tokio::process::ChildStdin;
    type Output = tokio::process::ChildStdout;
    fn take_input(&mut self) -> Option<Self::Input> {
        self.input.take()
    }
    fn take_output(&mut self) -> Option<Self::Output> {
        self.output.take()
    }
    fn wait(&mut self) -> ProcessFuture<'_, bool> {
        Box::pin(async {
            loop {
                match self.child.try_wait().map_err(|error| error.to_string())? {
                    Some(status) => return Ok(status.success()),
                    None => tokio::time::sleep(POLL).await,
                }
            }
        })
    }
    fn terminate(&mut self) -> ProcessFuture<'_, ()> {
        Box::pin(async {
            let _ = self.child.kill();
            self.wait().await.map(|_| ())
        })
    }
}

#[cfg(not(test))]
async fn supervise(
    shutdown: Shutdown,
    abandoned: CancellationToken,
    request: Vec<u8>,
    collector: Collector,
) -> Result<Output, String> {
    if shutdown.is_cancelled() || abandoned.is_cancelled() {
        return Err("server shutting down".into());
    }
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = std::process::Command::new(executable);
    command
        .arg(ARGUMENT)
        .env_remove("PHONOLITE_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let child = command
        .spawn()
        .map_err(|error| format!("source worker launch failed: {error}"))?;
    let mut child = crate::process_metrics::worker_cpu::TrackedChild::new(child);
    let input = child
        .stdin
        .take()
        .map(tokio::process::ChildStdin::from_std)
        .transpose()
        .map_err(|error| error.to_string())?;
    let output = child
        .stdout
        .take()
        .map(tokio::process::ChildStdout::from_std)
        .transpose()
        .map_err(|error| error.to_string())?;
    run_process(
        shutdown,
        abandoned,
        request,
        collector,
        ChildProcess {
            child,
            input,
            output,
        },
    )
    .await
}

async fn run_process<P: WorkerProcess>(
    shutdown: Shutdown,
    abandoned: CancellationToken,
    request: Vec<u8>,
    mut collector: Collector,
    mut child: P,
) -> Result<Output, String> {
    let record = shutdown.track_operation("isolated source worker");
    if let Some(record) = &record {
        record.stage("waiting for source worker output");
    }
    let Some(mut input) = child.take_input() else {
        let _ = tokio::time::timeout(REAP_TIMEOUT, child.terminate()).await;
        return Err("source worker stdin missing".into());
    };
    let Some(mut output) = child.take_output() else {
        let _ = tokio::time::timeout(REAP_TIMEOUT, child.terminate()).await;
        return Err("source worker stdout missing".into());
    };
    let result: Result<(), String> = {
        let operation = async {
            input
                .write_all(&request)
                .await
                .map_err(|error| error.to_string())?;
            input.flush().await.map_err(|error| error.to_string())?;
            receive(&mut output, &mut collector).await?;
            if !child.wait().await? {
                return Err("source worker exited unsuccessfully".into());
            }
            Ok(())
        };
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => Err("server shutting down".into()),
            _ = abandoned.cancelled() => Err("source worker caller cancelled".into()),
            result = tokio::time::timeout(JOB_TIMEOUT, operation) => match result {
                Ok(result) => result,
                Err(_) => Err("source worker exceeded deadline".into()),
            },
        }
    };
    if let Err(error) = result {
        // Kill and wait even when the HTTP/background caller has been dropped.
        if let Some(record) = &record {
            record.stage("killing and reaping source worker");
        }
        if tokio::time::timeout(REAP_TIMEOUT, child.terminate())
            .await
            .is_err()
        {
            tracing::warn!("Source worker did not reap within cleanup allowance; no database handles are held by the worker.");
        }
        if let Some(writer) = collector.writer.as_mut() {
            writer.fail(error.clone());
        }
        return Err(error);
    }
    if shutdown.is_cancelled() || abandoned.is_cancelled() {
        return Err("server shutting down".into());
    }
    collector.finish()
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[cfg(not(test))]
async fn execute(shutdown: &Shutdown, job: Job, collector: Collector) -> Result<Output, String> {
    if shutdown.is_cancelled() {
        return Err("server shutting down".into());
    }
    let name = format!("{} supervisor", job.name());
    let request = encode_request(job)?;
    let abandoned = CancellationToken::new();
    let _cancel = CancelOnDrop(abandoned.clone());
    let (reply, result) = tokio::sync::oneshot::channel();
    let lifecycle = shutdown.clone();
    shutdown.spawn_graceful_named(&name, async move {
        let output = supervise(lifecycle, abandoned, request, collector).await;
        let _ = reply.send(output);
    });
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => Err("server shutting down".into()),
        result = result => result.map_err(|_| "source worker supervisor stopped".to_owned())?,
    }
}

// Unit verification exercises real framing and computation in memory. It must
// never launch this test executable as an application or start a helper process.
#[cfg(test)]
async fn execute(
    shutdown: &Shutdown,
    job: Job,
    mut collector: Collector,
) -> Result<Output, String> {
    if shutdown.is_cancelled() {
        return Err("server shutting down".into());
    }
    let bytes = encode_request(job)?;
    let request = read_request(&mut &bytes[..])?;
    let mut output = Vec::new();
    perform(request, &mut output)?;
    write_frame(&mut output, DONE, &[])?;
    receive(&mut &output[..], &mut collector).await?;
    if shutdown.is_cancelled() {
        return Err("server shutting down".into());
    }
    collector.finish()
}

pub(crate) async fn read_cover(
    shutdown: &Shutdown,
    source: &Path,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Option<metadata::CoverArt>, String> {
    let mut collector = Collector::plain();
    collector._permit = Some(permit);
    match execute(
        shutdown,
        Job::Cover {
            source: absolute(source)?,
        },
        collector,
    )
    .await?
    {
        Output::Cover(cover) => Ok(cover),
        _ => Err("unexpected cover worker result".into()),
    }
}

pub(crate) async fn thumbnail(
    shutdown: &Shutdown,
    source: &Path,
    size: u32,
    expected_hash: Option<&str>,
    limits: &crate::resources::ResourceLimits,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Vec<u8>, String> {
    let mut collector = Collector::plain();
    collector._permit = Some(permit);
    match execute(
        shutdown,
        Job::Thumbnail {
            source: absolute(source)?,
            size,
            expected_hash: expected_hash.map(str::to_owned),
            limits: limits.clone(),
        },
        collector,
    )
    .await?
    {
        Output::Thumbnail(bytes) => Ok(bytes),
        _ => Err("unexpected thumbnail worker result".into()),
    }
}

pub(crate) async fn transcode(
    shutdown: &Shutdown,
    source: &Path,
    quality: TranscodeQuality,
    frame_ms: u32,
    meta: RawOpusMeta,
    start_ms: u32,
    end_ms: u32,
    writer: CacheWriter,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<(), String> {
    let collector = Collector {
        writer: Some(writer),
        _permit: Some(permit),
        ..Collector::plain()
    };
    match execute(
        shutdown,
        Job::Transcode {
            source: absolute(source)?,
            quality,
            frame_ms,
            meta,
            start_ms,
            end_ms,
        },
        collector,
    )
    .await?
    {
        Output::Transcode => Ok(()),
        _ => Err("unexpected transcode worker result".into()),
    }
}

#[cfg(not(test))]
static TAG_CHILDREN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(not(test))]
struct TagSlot;
#[cfg(not(test))]
impl TagSlot {
    fn acquire(cancellation: &AtomicBool) -> Result<Self, String> {
        loop {
            if cancellation.load(Ordering::Acquire) {
                return Err("server shutting down".into());
            }
            let count = TAG_CHILDREN.load(Ordering::Acquire);
            if count < 16
                && TAG_CHILDREN
                    .compare_exchange(count, count + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                return Ok(Self);
            }
            std::thread::sleep(POLL);
        }
    }
}
#[cfg(not(test))]
impl Drop for TagSlot {
    fn drop(&mut self) {
        TAG_CHILDREN.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(not(test))]
struct SyncChild {
    child: Option<crate::process_metrics::worker_cpu::TrackedChild>,
    slot: Option<TagSlot>,
}
#[cfg(not(test))]
impl SyncChild {
    fn new(child: std::process::Child, slot: Option<TagSlot>) -> Self {
        Self {
            child: Some(crate::process_metrics::worker_cpu::TrackedChild::new(child)),
            slot,
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Err(io::Error::other(
                "source child already transferred to reaper",
            ));
        };
        child.try_wait()
    }

    fn stop(&mut self) {
        if self.child.is_none() {
            return;
        }
        let _ = self.child.as_mut().unwrap().kill();
        let deadline = Instant::now() + REAP_TIMEOUT;
        loop {
            match self.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
                _ => break,
            }
        }
        // A kernel-stalled source read may not reap immediately even after a
        // kill. Its reaper owns only the child and its finite capacity slot,
        // never the scanner's database or library state.
        if let Some(mut child) = self.child.take() {
            let slot = self.slot.take();
            let _ = std::thread::Builder::new()
                .name("source-worker-reaper".into())
                .spawn(move || {
                    let _slot = slot;
                    loop {
                        match child.try_wait() {
                            Ok(Some(_)) | Err(_) => break,
                            Ok(None) => std::thread::sleep(POLL),
                        }
                    }
                });
        }
    }
}
#[cfg(not(test))]
impl std::ops::Deref for SyncChild {
    type Target = crate::process_metrics::worker_cpu::TrackedChild;
    fn deref(&self) -> &Self::Target {
        self.child
            .as_ref()
            .expect("source child already transferred to reaper")
    }
}
#[cfg(not(test))]
impl std::ops::DerefMut for SyncChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.child
            .as_mut()
            .expect("source child already transferred to reaper")
    }
}
#[cfg(not(test))]
impl Drop for SyncChild {
    fn drop(&mut self) {
        self.stop();
    }
}

/// For library scan threads: the only companion thread owns stdout, never a DB
/// handle. Killing/reaping the child closes its pipe before that thread is joined.
#[cfg(not(test))]
pub(crate) fn read_tags_blocking(
    source: &Path,
    cancellation: &AtomicBool,
) -> Result<metadata::TagInfo, String> {
    if cancellation.load(Ordering::Acquire) {
        return Err("server shutting down".into());
    }
    let request = encode_request(Job::Tags {
        source: absolute(source)?,
    })?;
    let slot = TagSlot::acquire(cancellation)?;
    let mut command =
        std::process::Command::new(std::env::current_exe().map_err(|error| error.to_string())?);
    command
        .arg(ARGUMENT)
        .env_remove("PHONOLITE_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = SyncChild::new(
        command.spawn().map_err(|error| error.to_string())?,
        Some(slot),
    );
    let operation = (|| {
        let mut input = child.stdin.take().ok_or("source worker stdin missing")?;
        let mut output = child.stdout.take().ok_or("source worker stdout missing")?;
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("source-worker-output".into())
            .spawn(move || {
                let mut collector = Collector::plain();
                let parsed = (|| {
                    input
                        .write_all(&request)
                        .map_err(|error| error.to_string())?;
                    input.flush().map_err(|error| error.to_string())?;
                    loop {
                        let mut header = [0; 6];
                        output
                            .read_exact(&mut header)
                            .map_err(|error| error.to_string())?;
                        let mut data = vec![0; frame_length(&header)?];
                        output
                            .read_exact(&mut data)
                            .map_err(|error| error.to_string())?;
                        if collector.accept(header[1], data)? {
                            break;
                        }
                    }
                    collector.finish()
                })();
                // Keep the parent-liveness pipe open until the supervising scan
                // thread has observed child exit, including after a DONE frame.
                let _ = send.send((parsed, input));
            })
            .map_err(|error| error.to_string())?;
        let started = Instant::now();
        let mut parsed = None;
        let mut _input_guard = None;
        let result = loop {
            if cancellation.load(Ordering::Acquire) {
                break Err("server shutting down".into());
            }
            if started.elapsed() >= JOB_TIMEOUT {
                break Err("source worker exceeded deadline".into());
            }
            if parsed.is_none() {
                match receive.try_recv() {
                    Ok((value, input)) => {
                        parsed = Some(value);
                        _input_guard = Some(input);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(_) => break Err("source worker output closed".into()),
                }
            }
            if parsed.as_ref().is_some_and(Result::is_err) {
                break parsed.take().unwrap();
            }
            match child.try_wait() {
                Ok(Some(status)) if !status.success() => {
                    break Err("source worker exited unsuccessfully".into())
                }
                Ok(Some(_)) if parsed.is_some() => break parsed.take().unwrap(),
                Ok(_) => {}
                Err(error) => break Err(error.to_string()),
            }
            std::thread::sleep(POLL);
        };
        child.stop();
        let deadline = Instant::now() + REAP_TIMEOUT;
        while !reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        if reader.is_finished() {
            let _ = reader.join();
        }
        result
    })();
    // Also cover failures before the output reader was constructed.
    child.stop();
    if cancellation.load(Ordering::Acquire) {
        return Err("server shutting down".into());
    }
    match operation? {
        Output::Tags(tags) => Ok(tags),
        _ => Err("unexpected tag worker result".into()),
    }
}

#[cfg(test)]
pub(crate) fn read_tags_blocking(
    source: &Path,
    cancellation: &AtomicBool,
) -> Result<metadata::TagInfo, String> {
    if cancellation.load(Ordering::Acquire) {
        return Err("server shutting down".into());
    }
    let result = metadata::read_tags(source).map_err(|error| format!("{error:?}"))?;
    if cancellation.load(Ordering::Acquire) {
        return Err("server shutting down".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn audio_worker_protocol_roundtrips_a_real_range_without_a_process() {
        use crate::stream_cache::{CacheKey, StreamCache};
        use crate::transcode::{TranscodeMode, TranscodeQuality};
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac");
        let cache = StreamCache::new(PathBuf::new(), true);
        let key = CacheKey::new(
            "worker-range",
            20,
            TranscodeMode::Fixed,
            TranscodeQuality::High,
        )
        .with_source(&source)
        .unwrap();
        let writer = cache.writer(key.clone()).unwrap().unwrap();
        let capacity = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let permit = capacity.clone().acquire_owned().await.unwrap();
        transcode(
            &Shutdown::new(),
            &source,
            TranscodeQuality::High,
            20,
            RawOpusMeta {
                track_id: "worker-range".into(),
                title: "tone".into(),
                artist: "fixture".into(),
                album: "tests".into(),
                duration_ms: 400,
                codec: "opus".into(),
                container: "raw".into(),
            },
            0,
            200,
            writer,
            permit,
        )
        .await
        .unwrap();
        assert_eq!(capacity.available_permits(), 1);
        let reader = cache.reader(&key).unwrap().unwrap();
        let (send, mut receive) = tokio::sync::mpsc::channel(64);
        reader.stream_to(0, &send).unwrap();
        drop(send);
        let header = receive.try_recv().unwrap().unwrap();
        assert_eq!(&header[..8], b"OPUSR01\0");
        let mut audio = 0;
        let mut eos = 0;
        while let Ok(packet) = receive.try_recv() {
            let packet = packet.unwrap();
            if packet.as_ref() == [0, 0] {
                eos += 1;
            } else {
                audio += 1;
            }
        }
        assert!(audio >= 10);
        assert_eq!(eos, 1);
    }

    #[test]
    fn malformed_or_oversized_frames_are_rejected_before_allocation() {
        assert!(frame_length(&[9, META, 0, 0, 0, 0]).is_err());
        let mut header = [VERSION, META, 0, 0, 0, 0];
        header[2..].copy_from_slice(&((MAX_FRAME + 1) as u32).to_le_bytes());
        assert!(frame_length(&header).is_err());
        let prefix = ((MAX_REQUEST + 1) as u32).to_le_bytes();
        assert!(read_request(&mut &prefix[..]).is_err());
    }

    #[tokio::test]
    async fn protocol_reads_fragmented_results_without_an_application_or_process() {
        let mut wire = Vec::new();
        send_metadata(&mut wire, &Metadata::Thumbnail).unwrap();
        send_data(&mut wire, b"image-result").unwrap();
        write_frame(&mut wire, DONE, &[]).unwrap();
        let (mut input, mut output) = tokio::io::duplex(7);
        let producer = tokio::spawn(async move {
            output.write_all(&wire).await.unwrap();
        });
        let mut collector = Collector::plain();
        receive(&mut input, &mut collector).await.unwrap();
        producer.await.unwrap();
        assert!(
            matches!(collector.finish().unwrap(), Output::Thumbnail(bytes) if bytes == b"image-result")
        );
    }

    #[test]
    fn incomplete_and_unexpected_results_cannot_be_published() {
        let mut collector = Collector::plain();
        assert!(collector.accept(DATA, b"late".to_vec()).is_err());
        assert!(Collector::plain().finish().is_err());
    }

    #[test]
    fn a_source_changed_while_returning_a_result_cannot_complete() {
        struct MutatingOutput {
            source: PathBuf,
            bytes: Vec<u8>,
            changed: bool,
        }
        impl Write for MutatingOutput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.changed {
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(&self.source)?
                        .write_all(b"changed")?;
                    self.changed = true;
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let source =
            std::env::temp_dir().join(format!("phonolite-source-{}.png", uuid::Uuid::new_v4()));
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
            .save(&source)
            .unwrap();
        let request = Request {
            version: VERSION,
            job: Job::Thumbnail {
                source: source.clone(),
                size: 128,
                expected_hash: None,
                limits: Default::default(),
            },
        };
        let mut output = MutatingOutput {
            source: source.clone(),
            bytes: Vec::new(),
            changed: false,
        };
        let result = perform(request, &mut output);
        std::fs::remove_file(source).unwrap();
        assert!(result.unwrap_err().contains("source changed"));
        assert!(output.changed);
        // perform deliberately does not emit DONE. Dispatch emits an ERROR,
        // so the parent never accepts the partially returned rendition.
    }

    struct FakeProcess {
        input: Option<tokio::io::DuplexStream>,
        output: Option<tokio::io::DuplexStream>,
        terminated: std::sync::Arc<AtomicBool>,
        reaped: std::sync::Arc<AtomicBool>,
        reap_permission: Option<tokio::sync::oneshot::Receiver<()>>,
    }
    impl WorkerProcess for FakeProcess {
        type Input = tokio::io::DuplexStream;
        type Output = tokio::io::DuplexStream;
        fn take_input(&mut self) -> Option<Self::Input> {
            self.input.take()
        }
        fn take_output(&mut self) -> Option<Self::Output> {
            self.output.take()
        }
        fn wait(&mut self) -> ProcessFuture<'_, bool> {
            Box::pin(std::future::pending())
        }
        fn terminate(&mut self) -> ProcessFuture<'_, ()> {
            Box::pin(async {
                self.terminated.store(true, Ordering::Release);
                if let Some(permission) = self.reap_permission.take() {
                    let _ = permission.await;
                }
                self.reaped.store(true, Ordering::Release);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn cancellation_kills_and_reaps_before_releasing_worker_capacity() {
        let shutdown = Shutdown::new();
        let abandoned = CancellationToken::new();
        let capacity = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let permit = capacity.clone().acquire_owned().await.unwrap();
        let mut collector = Collector::plain();
        collector._permit = Some(permit);
        let terminated = std::sync::Arc::new(AtomicBool::new(false));
        let reaped = std::sync::Arc::new(AtomicBool::new(false));
        let (permission, wait_permission) = tokio::sync::oneshot::channel();
        let (input, _fake_stdin) = tokio::io::duplex(64);
        let (output, _fake_stdout) = tokio::io::duplex(64);
        let process = FakeProcess {
            input: Some(input),
            output: Some(output),
            terminated: terminated.clone(),
            reaped: reaped.clone(),
            reap_permission: Some(wait_permission),
        };
        let task = tokio::spawn(run_process(
            shutdown.clone(),
            abandoned,
            b"request".to_vec(),
            collector,
            process,
        ));
        // The output stays open and never responds. No application is launched.
        tokio::task::yield_now().await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !terminated.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!reaped.load(Ordering::Acquire));
        assert_eq!(capacity.available_permits(), 0);
        permission.send(()).unwrap();
        assert!(task.await.unwrap().is_err());
        assert!(reaped.load(Ordering::Acquire));
        assert_eq!(capacity.available_permits(), 1);
    }

    #[tokio::test]
    async fn dropping_the_caller_stops_a_worker_even_without_global_shutdown() {
        let shutdown = Shutdown::new();
        let abandoned = CancellationToken::new();
        let owner = CancelOnDrop(abandoned.clone());
        let terminated = std::sync::Arc::new(AtomicBool::new(false));
        let reaped = std::sync::Arc::new(AtomicBool::new(false));
        let (input, _fake_stdin) = tokio::io::duplex(64);
        let (output, _fake_stdout) = tokio::io::duplex(64);
        let process = FakeProcess {
            input: Some(input),
            output: Some(output),
            terminated: terminated.clone(),
            reaped: reaped.clone(),
            reap_permission: None,
        };
        let task = tokio::spawn(run_process(
            shutdown.clone(),
            abandoned,
            Vec::new(),
            Collector::plain(),
            process,
        ));
        drop(owner);
        assert!(tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(terminated.load(Ordering::Acquire));
        assert!(reaped.load(Ordering::Acquire));
        assert!(!shutdown.is_cancelled());
    }

    #[tokio::test]
    async fn a_completed_response_still_waits_for_exit_and_is_rejected_on_cancel() {
        let shutdown = Shutdown::new();
        let terminated = std::sync::Arc::new(AtomicBool::new(false));
        let reaped = std::sync::Arc::new(AtomicBool::new(false));
        let (input, _fake_stdin) = tokio::io::duplex(64);
        let (output, mut fake_stdout) = tokio::io::duplex(128);
        let mut response = Vec::new();
        send_metadata(&mut response, &Metadata::Thumbnail).unwrap();
        send_data(&mut response, b"late-result").unwrap();
        write_frame(&mut response, DONE, &[]).unwrap();
        fake_stdout.write_all(&response).await.unwrap();
        let process = FakeProcess {
            input: Some(input),
            output: Some(output),
            terminated: terminated.clone(),
            reaped: reaped.clone(),
            reap_permission: None,
        };
        let task = tokio::spawn(run_process(
            shutdown.clone(),
            CancellationToken::new(),
            Vec::new(),
            Collector::plain(),
            process,
        ));
        tokio::task::yield_now().await;
        shutdown.cancel();
        assert!(task.await.unwrap().is_err());
        assert!(terminated.load(Ordering::Acquire));
        assert!(reaped.load(Ordering::Acquire));
    }
}
