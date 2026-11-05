//! Shared cancellation, admission and diagnostics for server-owned work.
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_util::{
    sync::CancellationToken,
    task::{task_tracker::TaskTrackerToken, TaskTracker},
};

pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(20);
pub const RUNTIME_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct Registry {
    stopping: bool,
    next: u64,
    jobs: BTreeMap<u64, Job>,
}
#[derive(Debug)]
struct Job {
    name: String,
    kind: &'static str,
    stage: String,
    started: Instant,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingJob {
    pub name: String,
    pub kind: &'static str,
    pub stage: String,
    pub elapsed: Duration,
}
#[derive(Debug)]
pub struct WorkGuard {
    registry: Arc<Mutex<Registry>>,
    id: u64,
    _token: TaskTrackerToken,
}
impl WorkGuard {
    pub fn stage(&self, stage: &str) {
        if let Some(job) = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs
            .get_mut(&self.id)
        {
            job.stage = stage.to_owned();
        }
    }
}
impl Drop for WorkGuard {
    fn drop(&mut self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs
            .remove(&self.id);
    }
}
#[derive(Clone, Debug)]
pub struct Shutdown {
    token: CancellationToken,
    flag: Arc<AtomicBool>,
    tasks: TaskTracker,
    registry: Arc<Mutex<Registry>>,
    // 0: no action, 1: restart, 2: explicit stop. Stop always wins.
    action: Arc<AtomicU8>,
}
impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}
impl Shutdown {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            flag: Arc::new(AtomicBool::new(false)),
            tasks: TaskTracker::new(),
            registry: Arc::new(Mutex::new(Registry::default())),
            action: Arc::new(AtomicU8::new(0)),
        }
    }
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
    pub fn request_stop(&self) {
        self.action.store(2, Ordering::Release);
        self.cancel();
    }
    pub fn request_restart(&self) {
        let _ = self
            .action
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
        self.cancel();
    }
    pub fn restart_requested(&self) -> bool {
        self.action.load(Ordering::Acquire) == 1
    }
    pub fn cancel(&self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stopping = true;
        self.flag.store(true, Ordering::Release);
        self.token.cancel();
    }
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
    fn register(&self, name: String, kind: &'static str, cleanup: bool) -> Option<WorkGuard> {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if registry.stopping && !cleanup {
            return None;
        }
        let id = registry.next;
        registry.next += 1;
        registry.jobs.insert(
            id,
            Job {
                name,
                kind,
                stage: "admitted".into(),
                started: Instant::now(),
            },
        );
        Some(WorkGuard {
            registry: self.registry.clone(),
            id,
            _token: self.tasks.token(),
        })
    }
    pub fn track_operation(&self, name: &str) -> Option<WorkGuard> {
        self.register(name.to_owned(), "disposable", false)
    }
    pub fn pending_jobs(&self) -> Vec<PendingJob> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs
            .values()
            .map(|job| PendingJob {
                name: job.name.clone(),
                kind: job.kind,
                stage: job.stage.clone(),
                elapsed: job.started.elapsed(),
            })
            .collect()
    }
    pub fn log_pending(&self) {
        for job in self.pending_jobs() {
            tracing::warn!(job = %job.name, kind = job.kind, stage = %job.stage,
                elapsed_ms = job.elapsed.as_millis() as u64, "Shutdown work still pending");
        }
    }
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.spawn_named(&std::panic::Location::caller().to_string(), future)
    }
    pub fn spawn_named<F>(&self, name: &str, future: F) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let guard = self.register(name.to_owned(), "async", false);
        let token = self.token.clone();
        self.tasks.spawn(async move {
            let Some(_guard) = guard else {
                drop(future);
                return;
            };
            tokio::select! { biased; _ = token.cancelled() => {}, _ = future => {} }
        })
    }
    #[track_caller]
    pub fn spawn_graceful<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.spawn_graceful_named(&std::panic::Location::caller().to_string(), future)
    }
    pub fn spawn_graceful_named<F>(&self, name: &str, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let guard = self.register(name.to_owned(), "graceful", false);
        let admitted = guard.is_some();
        let handle = self.tasks.spawn(async move {
            let Some(_guard) = guard else {
                return std::future::pending().await;
            };
            future.await
        });
        if !admitted {
            handle.abort();
        }
        handle
    }
    #[track_caller]
    pub fn spawn_blocking<F, R>(&self, work: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.spawn_blocking_named(&std::panic::Location::caller().to_string(), work)
    }
    pub fn spawn_blocking_named<F, R>(&self, name: &str, work: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        // Already-owned commits and resource destructors remain tracked even
        // after cancellation. Disposable callers use the checked variant.
        let guard = self.register(name.to_owned(), "blocking", true);
        self.tasks.spawn_blocking(move || {
            let _guard = guard;
            if let Some(guard) = &_guard {
                guard.stage("running");
            }
            work()
        })
    }
    pub fn spawn_blocking_commit<F, R>(&self, name: &str, work: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let guard = self.register(name.to_owned(), "commit", true);
        self.tasks.spawn_blocking(move || {
            let _guard = guard;
            if let Some(guard) = &_guard {
                guard.stage("commit-or-rollback");
            }
            work()
        })
    }
    pub fn spawn_cleanup<F, R>(&self, name: &str, work: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let guard = self.register(name.to_owned(), "cleanup", true);
        self.tasks.spawn_blocking(move || {
            let _guard = guard;
            if let Some(guard) = &_guard {
                guard.stage("cleanup");
            }
            work()
        })
    }
    pub fn spawn_cancellable_blocking<F, R>(
        &self,
        name: &str,
        work: F,
    ) -> JoinHandle<Result<R, String>>
    where
        F: FnOnce() -> Result<R, String> + Send + 'static,
        R: Send + 'static,
    {
        let guard = self.register(name.to_owned(), "disposable", false);
        let flag = self.flag();
        self.tasks.spawn_blocking(move || {
            let Some(guard) = guard else {
                return Err("server shutting down".into());
            };
            guard.stage("running");
            if flag.load(Ordering::Acquire) {
                return Err("server shutting down".into());
            }
            let result = work();
            if flag.load(Ordering::Acquire) {
                return Err("server shutting down".into());
            }
            result
        })
    }
    pub async fn drain(&self, timeout: Duration) -> bool {
        self.cancel();
        self.tasks.close();
        let complete = tokio::time::timeout(timeout, self.tasks.wait())
            .await
            .is_ok();
        if !complete {
            self.log_pending();
        }
        complete
    }
}
