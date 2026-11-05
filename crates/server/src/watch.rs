use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use library::Library;
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::scan::start_changed_scan;
use crate::shutdown::Shutdown;
use crate::state::AppState;

const WORKER_ARGUMENT: &str = "--phonolite-watch-worker-v1";
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const EVENT_CAPACITY: usize = 256;
const MAX_CHANGED_PATHS: usize = 4096;

/// The parent owns only cancellation. The child owns every platform watcher,
/// so a blocked recursive registration never retains AppState or a database.
pub struct WatchHandle {
    cancellation: CancellationToken,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Change {
    paths: Vec<PathBuf>,
    full_scan: bool,
}

impl Change {
    fn full_scan() -> Self {
        Self {
            paths: Vec::new(),
            full_scan: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum WorkerMessage {
    Ready,
    Change(Change),
    Failed(String),
}

pub(crate) fn clear_watcher(state: &AppState) {
    // Dropping an owner notifies the async supervisor; it never joins native
    // notify threads or waits for a registration response.
    let previous = state.watcher.write().take();
    drop(previous);
}

pub fn configure_watcher(state: &AppState, library: &Library) {
    if state.shutdown.is_cancelled() {
        return;
    }
    let config = state.config.read().clone();
    let roots: Vec<PathBuf> = library
        .roots()
        .iter()
        .map(|root| root.path.clone())
        .collect();
    if !config.watch_music || roots.is_empty() {
        clear_watcher(state);
        return;
    }

    let cancellation = CancellationToken::new();
    let handle = WatchHandle {
        cancellation: cancellation.clone(),
    };
    let previous = {
        let mut slot = state.watcher.write();
        if state.shutdown.is_cancelled() {
            return;
        }
        slot.replace(handle)
    };
    drop(previous);

    let debounce = Duration::from_secs(if config.watch_debounce_secs == 0 {
        2
    } else {
        config.watch_debounce_secs
    });
    let (events, receiver) = tokio::sync::mpsc::channel(EVENT_CAPACITY);
    let shutdown = state.shutdown.clone();
    let monitor = shutdown.clone();
    shutdown.spawn_graceful_named("watcher-process", async move {
        if let Err(error) = supervise_watcher(roots, events, cancellation, monitor.clone()).await {
            if !monitor.is_cancelled() {
                warn!("Music watcher stopped: {error}");
            }
        }
    });
    let state = state.clone();
    let library = library.clone();
    state
        .shutdown
        .clone()
        .spawn_named("watcher-events", async move {
            watch_loop(state, library, receiver, debounce).await;
        });
}

async fn supervise_watcher(
    roots: Vec<PathBuf>,
    events: Sender<Change>,
    cancellation: CancellationToken,
    shutdown: Shutdown,
) -> Result<(), String> {
    if shutdown.is_cancelled() || cancellation.is_cancelled() {
        return Ok(());
    }
    let Some(operation) = shutdown.track_operation("watcher-child") else {
        return Ok(());
    };
    operation.stage("starting");
    let mut command =
        std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command
        .arg(WORKER_ARGUMENT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = crate::process_metrics::worker_cpu::TrackedChild::new(
        command
            .spawn()
            .map_err(|e| format!("start watcher worker: {e}"))?,
    );
    let stdin = child
        .stdin
        .take()
        .ok_or("watcher worker stdin unavailable")?;
    let stdin = tokio::process::ChildStdin::from_std(stdin).map_err(|e| e.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or("watcher worker stdout unavailable")?;
    let stdout = tokio::process::ChildStdout::from_std(stdout).map_err(|e| e.to_string())?;
    let result = {
        operation.stage("registering-roots");
        let protocol = watcher_protocol(stdin, stdout, roots, events, &operation);
        until_stopped(&shutdown, &cancellation, protocol).await
    };
    // kill is immediate, then reap against a bound. The helper contains
    // no durable data or parent database handles.
    operation.stage("reaping");
    let _ = child.kill();
    match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => return Err(format!("reap watcher worker: {error}")),
        Err(_) => return Err("watcher worker did not reap within one second".into()),
    }
    result
}

async fn until_stopped(
    shutdown: &Shutdown,
    cancellation: &CancellationToken,
    work: impl std::future::Future<Output = Result<(), String>>,
) -> Result<(), String> {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => Ok(()),
        _ = cancellation.cancelled() => Ok(()),
        result = work => result,
    }
}

async fn watcher_protocol(
    mut stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    roots: Vec<PathBuf>,
    events: Sender<Change>,
    operation: &crate::shutdown::WorkGuard,
) -> Result<(), String> {
    let mut request = serde_json::to_vec(&roots).map_err(|e| e.to_string())?;
    if request.len() > MAX_MESSAGE_BYTES {
        return Err("watch roots exceed worker request limit".into());
    }
    request.push(b'\n');
    stdin.write_all(&request).await.map_err(|e| e.to_string())?;
    // This pipe also proves the parent's lifetime to the child. Keep it open
    // even after registration; a killed parent cannot leave an orphan watcher.
    let _parent_lifetime = stdin;
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut buffer = Vec::new();
    let mut overflow = false;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            biased;
            _ = events.closed() => return Ok(()),
            message = read_message(&mut reader, &mut buffer) => {
                match message? {
                    Some(WorkerMessage::Ready) => {
                        operation.stage("listening");
                        info!("Watching {} music roots in an isolated worker.", roots.len());
                    },
                    Some(WorkerMessage::Change(event)) => {
                        match events.try_send(event) {
                            Ok(()) => {},
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => overflow = true,
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                        }
                    },
                    Some(WorkerMessage::Failed(error)) => return Err(error),
                    None => return Err("watcher worker exited".into()),
                }
            },
            _ = tick.tick() => {
                if overflow && events.try_send(Change::full_scan()).is_ok() {
                    overflow = false;
                }
            }
        }
    }
}

/// Keep a partially read line in caller-owned memory: cancellation of this
/// future by the protocol tick cannot lose bytes or bypass its memory bound.
async fn read_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
) -> Result<Option<WorkerMessage>, String> {
    loop {
        let available = reader.fill_buf().await.map_err(|e| e.to_string())?;
        if available.is_empty() {
            return if buffer.is_empty() {
                Ok(None)
            } else {
                Err("truncated watcher message".into())
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |offset| offset + 1);
        if buffer.len().saturating_add(count) > MAX_MESSAGE_BYTES {
            return Err("watcher message exceeds limit".into());
        }
        buffer.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            let result =
                serde_json::from_slice(buffer).map_err(|e| format!("invalid watcher message: {e}"));
            buffer.clear();
            return result.map(Some);
        }
    }
}

/// Executed before runtime/config/database initialization, only in child mode.
pub(crate) fn dispatch_if_worker() -> Option<Result<(), String>> {
    let arguments: Vec<_> = std::env::args_os().collect();
    if arguments.get(1).is_none_or(|arg| arg != WORKER_ARGUMENT) {
        return None;
    }
    Some(if arguments.len() == 2 {
        watch_worker()
    } else {
        Err("invalid watcher worker arguments".into())
    })
}

fn watch_worker() -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .lock()
        .take((MAX_MESSAGE_BYTES + 1) as u64)
        .read_line(&mut input)
        .map_err(|e| e.to_string())?;
    if input.len() > MAX_MESSAGE_BYTES || !input.ends_with('\n') {
        return Err("invalid watcher worker request".into());
    }
    let roots: Vec<PathBuf> = serde_json::from_str(&input).map_err(|e| e.to_string())?;
    // Only this database-free child mode may exit from the watchdog. It also
    // covers a registration stuck inside notify when its parent disappears.
    let _parent = parent_watchdog(std::io::stdin(), || std::process::exit(0))
        .map_err(|e| format!("watcher parent watchdog: {e}"))?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(EVENT_CAPACITY);
    let overflow = Arc::new(AtomicBool::new(false));
    let callback_overflow = overflow.clone();
    let mut watcher = RecommendedWatcher::new(
        move |result: notify::Result<Event>| {
            let change = match result {
                Ok(event) if is_relevant_event(&event) => Change {
                    full_scan: event.need_rescan() || event.paths.is_empty(),
                    paths: event.paths,
                },
                Ok(_) => return,
                Err(_) => Change::full_scan(),
            };
            if sender.try_send(change).is_err() {
                callback_overflow.store(true, Ordering::Release);
            }
        },
        NotifyConfig::default(),
    )
    .map_err(|e| e.to_string())?;

    let mut output = std::io::stdout().lock();
    for root in roots {
        if let Err(error) = watcher.watch(&root, RecursiveMode::Recursive) {
            write_message(&mut output, &WorkerMessage::Failed(error.to_string()))?;
            return Ok(());
        }
    }
    write_message(&mut output, &WorkerMessage::Ready)?;
    loop {
        if overflow.swap(false, Ordering::AcqRel) {
            write_message(&mut output, &WorkerMessage::Change(Change::full_scan()))?;
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(change) => write_message(&mut output, &WorkerMessage::Change(change))?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn parent_watchdog(
    mut input: impl Read + Send + 'static,
    on_closed: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("watch-parent-lifetime".into())
        .spawn(move || {
            let mut bytes = [0; 256];
            loop {
                match input.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            on_closed();
        })
}

fn write_message(output: &mut impl Write, message: &WorkerMessage) -> Result<(), String> {
    let bytes = serde_json::to_vec(message).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err("watcher event exceeds limit".into());
    }
    output
        .write_all(&bytes)
        .and_then(|_| output.write_all(b"\n"))
        .and_then(|_| output.flush())
        .map_err(|e| e.to_string())
}

async fn watch_loop(
    state: AppState,
    library: Library,
    mut rx: Receiver<Change>,
    debounce: Duration,
) {
    loop {
        let event = match tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => return,
            event = rx.recv() => event,
        } {
            Some(event) => event,
            None => break,
        };
        let mut changed_paths = std::collections::HashSet::new();
        let mut full_scan = event.full_scan || event.paths.is_empty();
        changed_paths.extend(event.paths);
        let mut deadline = tokio::time::Instant::now() + debounce;
        loop {
            tokio::select! {
                biased;
                _ = state.shutdown.cancelled() => return,
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = state
                        .activity
                        .add_activity("Library auto-scan started.");
                    info!("Library auto-scan reconciling changed files");
                    let paths = if full_scan { None } else { Some(changed_paths.into_iter().collect()) };
                    let _ = start_changed_scan(state.clone(), library.clone(), paths).await;
                    break;
                }
                maybe_event = rx.recv() => {
                    if let Some(event) = maybe_event {
                        full_scan |= event.full_scan || event.paths.is_empty();
                        if !full_scan {
                            changed_paths.extend(event.paths);
                            if changed_paths.len() > MAX_CHANGED_PATHS {
                                full_scan = true;
                            }
                        }
                        if full_scan { changed_paths.clear(); }
                        deadline = tokio::time::Instant::now() + debounce;
                    } else {
                        return;
                    }
                }
            }
        }
    }
}

fn is_relevant_event(event: &Event) -> bool {
    event.need_rescan()
        || matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ObserveDrop(Arc<AtomicBool>);
    impl Drop for ObserveDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn discarding_a_watch_owner_cancels_an_unanswered_registration() {
        let shutdown = Shutdown::new();
        let cancellation = CancellationToken::new();
        let owner = WatchHandle {
            cancellation: cancellation.clone(),
        };
        let dropped = Arc::new(AtomicBool::new(false));
        let resource = ObserveDrop(dropped.clone());
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            until_stopped(&shutdown, &cancellation, async move {
                let _resource = resource;
                let _ = ready.send(());
                std::future::pending::<Result<(), String>>().await
            })
            .await
        });
        started.await.unwrap();
        drop(owner);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn a_partial_worker_message_survives_cancellation_of_the_read_future() {
        let (mut writer, reader) = tokio::io::duplex(256);
        let mut reader = tokio::io::BufReader::new(reader);
        let bytes = serde_json::to_vec(&WorkerMessage::Change(Change::full_scan())).unwrap();
        let mut buffer = Vec::new();
        writer.write_all(&bytes[..5]).await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            read_message(&mut reader, &mut buffer)
        )
        .await
        .is_err());
        assert_eq!(buffer, bytes[..5]);
        writer.write_all(&bytes[5..]).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        assert!(matches!(
            read_message(&mut reader, &mut buffer).await.unwrap(),
            Some(WorkerMessage::Change(Change {
                full_scan: true,
                ..
            }))
        ));
        assert!(buffer.is_empty());
    }

    #[tokio::test]
    async fn worker_messages_have_a_strict_memory_bound() {
        let mut reader =
            tokio::io::BufReader::new(std::io::Cursor::new(vec![b'x'; MAX_MESSAGE_BYTES + 1]));
        let mut buffer = Vec::new();
        assert!(read_message(&mut reader, &mut buffer)
            .await
            .unwrap_err()
            .contains("exceeds limit"));
        assert!(buffer.len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn loss_of_the_parent_pipe_stops_an_isolated_watcher() {
        struct Pipe {
            reader: std::sync::mpsc::Receiver<()>,
            entered: Option<std::sync::mpsc::Sender<()>>,
        }
        impl Read for Pipe {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                if let Some(entered) = self.entered.take() {
                    entered.send(()).unwrap();
                }
                let _ = self.reader.recv();
                Ok(0)
            }
        }
        let (parent, reader) = std::sync::mpsc::channel();
        let (entered, ready) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let observed = stopped.clone();
        let watchdog = parent_watchdog(
            Pipe {
                reader,
                entered: Some(entered),
            },
            move || {
                observed.store(true, Ordering::Release);
            },
        )
        .unwrap();
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!stopped.load(Ordering::Acquire));
        drop(parent);
        watchdog.join().unwrap();
        assert!(stopped.load(Ordering::Acquire));
    }
}
