//! Socketless regressions for the production shutdown lifecycle.
//! Test runtimes and worker threads are always drained or manually released.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

#[path = "../src/shutdown.rs"]
#[allow(dead_code)]
mod shutdown;

// The production cache uses these enums only as key labels. No encoding or
// transcoding behavior is replaced in the lifecycle/cache regression.
#[allow(dead_code)]
mod transcode {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum TranscodeMode {
        Auto,
        Fixed,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum TranscodeQuality {
        High,
        Medium,
        Low,
    }
}

#[path = "../src/stream_cache.rs"]
#[allow(dead_code)]
mod stream_cache;

use shutdown::Shutdown;

const TEST_DEADLINE: Duration = Duration::from_secs(3);

#[tokio::test]
async fn stop_rejects_disposable_work_without_starting_it() {
    let lifecycle = Shutdown::new();
    lifecycle.request_stop();
    let called = Arc::new(AtomicBool::new(false));
    let worker_called = called.clone();
    let result = lifecycle
        .spawn_cancellable_blocking("late-decoder", move || {
            worker_called.store(true, Ordering::Release);
            Ok(42)
        })
        .await
        .unwrap();
    assert!(result.is_err());
    assert!(!called.load(Ordering::Acquire));
    assert!(lifecycle.pending_jobs().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_diagnostics_keep_the_name_and_stage_until_work_really_finishes() {
    let lifecycle = Shutdown::new();
    let (release, blocked) = mpsc::channel::<()>();
    let (started, ready) = oneshot::channel();
    let guard = lifecycle.track_operation("catalogue-publication").unwrap();
    guard.stage("commit");
    let worker = lifecycle.spawn_blocking_commit("catalogue-commit", move || {
        let _guard = guard;
        let _ = started.send(());
        let _ = blocked.recv();
        42
    });
    ready.await.unwrap();
    let drained = lifecycle.drain(Duration::from_millis(20)).await;
    let pending = lifecycle.pending_jobs();
    drop(release);
    assert_eq!(worker.await.unwrap(), 42);
    assert!(!drained);
    assert!(pending
        .iter()
        .any(|job| job.name == "catalogue-publication" && job.stage == "commit"));
    assert!(lifecycle.drain(TEST_DEADLINE).await);
    assert!(lifecycle.pending_jobs().is_empty());
}

#[test]
fn explicit_stop_always_overrides_restart() {
    let lifecycle = Shutdown::new();
    lifecycle.request_restart();
    assert!(lifecycle.restart_requested());
    lifecycle.request_stop();
    lifecycle.request_restart();
    assert!(!lifecycle.restart_requested());
}

#[tokio::test]
async fn admission_reservations_are_counted_before_a_worker_is_spawned() {
    let lifecycle = Shutdown::new();
    let reservation = lifecycle.track_operation("not-yet-spawned").unwrap();
    let drained = lifecycle.drain(Duration::from_millis(20)).await;
    drop(reservation);
    assert!(!drained);
    assert!(lifecycle.track_operation("too-late").is_none());
    assert!(lifecycle.drain(TEST_DEADLINE).await);
}

#[tokio::test]
async fn cancellation_discards_a_completed_disposable_result() {
    let lifecycle = Shutdown::new();
    let inside = lifecycle.clone();
    let result = lifecycle
        .spawn_cancellable_blocking("late-result", move || {
            inside.cancel();
            Ok("discard me")
        })
        .await
        .unwrap();
    assert!(result.is_err());
    assert!(lifecycle.drain(TEST_DEADLINE).await);
}

struct ReleaseFlagOnDrop(Arc<AtomicBool>);

impl Drop for ReleaseFlagOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct ObserveFuture {
    polls: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

impl Future for ObserveFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        self.polls.fetch_add(1, Ordering::AcqRel);
        Poll::Pending
    }
}

impl Drop for ObserveFuture {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_wakes_every_current_waiter_and_late_waiters_without_lost_notifications() {
    let lifecycle = Shutdown::new();
    let completed = Arc::new(AtomicUsize::new(0));
    let mut waiters = Vec::new();
    let mut started = Vec::new();
    for _ in 0..8 {
        let shutdown = lifecycle.clone();
        let completed = completed.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        started.push(ready_rx);
        waiters.push(tokio::spawn(async move {
            ready_tx.send(()).unwrap();
            shutdown.cancelled().await;
            completed.fetch_add(1, Ordering::AcqRel);
        }));
    }
    for ready in started {
        tokio::time::timeout(TEST_DEADLINE, ready)
            .await
            .unwrap()
            .unwrap();
    }
    assert!(!lifecycle.is_cancelled());
    assert!(!lifecycle.flag().load(Ordering::Acquire));
    lifecycle.cancel();
    lifecycle.cancel();
    assert!(lifecycle.is_cancelled());
    assert!(lifecycle.flag().load(Ordering::Acquire));
    for waiter in waiters {
        tokio::time::timeout(TEST_DEADLINE, waiter)
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(completed.load(Ordering::Acquire), 8);
    tokio::time::timeout(TEST_DEADLINE, lifecycle.cancelled())
        .await
        .unwrap();
    assert!(lifecycle.drain(TEST_DEADLINE).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_sleeping_async_job_drops_its_resource_and_releases_a_blocking_reader() {
    let lifecycle = Shutdown::new();
    let (resource, reader) = mpsc::channel::<()>();
    let (async_started_tx, async_started_rx) = oneshot::channel();
    let (reader_started_tx, reader_started_rx) = oneshot::channel();
    let async_job = lifecycle.spawn(async move {
        let resource_kept_open = resource;
        async_started_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_secs(3600)).await;
        // Keep the sender alive across the await; cancellation must drop it.
        drop(resource_kept_open);
    });
    let blocking_reader = lifecycle.spawn_blocking(move || {
        reader_started_tx.send(()).unwrap();
        reader.recv().is_err()
    });
    tokio::time::timeout(TEST_DEADLINE, async_started_rx)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(TEST_DEADLINE, reader_started_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(lifecycle.drain(TEST_DEADLINE).await);
    assert!(lifecycle.is_cancelled());
    assert!(tokio::time::timeout(TEST_DEADLINE, blocking_reader)
        .await
        .unwrap()
        .unwrap());
    let async_result = tokio::time::timeout(TEST_DEADLINE, async_job)
        .await
        .unwrap();
    assert!(async_result.is_ok() || async_result.unwrap_err().is_cancelled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cooperative_blocking_work_observes_the_shared_flag_and_is_included_in_drain() {
    let lifecycle = Shutdown::new();
    let flag = lifecycle.flag();
    let release_if_test_panics = ReleaseFlagOnDrop(flag.clone());
    let (started_tx, started_rx) = oneshot::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let finished_in_worker = finished.clone();
    let worker = lifecycle.spawn_blocking(move || {
        started_tx.send(()).unwrap();
        while !flag.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        finished_in_worker.store(true, Ordering::Release);
        42
    });
    tokio::time::timeout(TEST_DEADLINE, started_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(!finished.load(Ordering::Acquire));
    lifecycle.cancel();
    assert!(lifecycle.drain(TEST_DEADLINE).await);
    assert!(finished.load(Ordering::Acquire));
    assert_eq!(
        tokio::time::timeout(TEST_DEADLINE, worker)
            .await
            .unwrap()
            .unwrap(),
        42
    );
    drop(release_if_test_panics);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_blocking_work_causes_a_bounded_false_drain_and_can_finish_later() {
    let lifecycle = Shutdown::new();
    let (release, blocked) = mpsc::channel::<()>();
    let (started_tx, started_rx) = oneshot::channel();
    let worker = lifecycle.spawn_blocking(move || {
        started_tx.send(()).unwrap();
        // Dropping `release` also unblocks this worker if an assertion fails.
        let _ = blocked.recv();
    });
    tokio::time::timeout(TEST_DEADLINE, started_rx)
        .await
        .unwrap()
        .unwrap();
    lifecycle.cancel();
    let before_drain = Instant::now();
    let drained = lifecycle.drain(Duration::from_millis(40)).await;
    let elapsed = before_drain.elapsed();
    // Release the worker before assertions so a failure never strands a thread.
    drop(release);
    tokio::time::timeout(TEST_DEADLINE, worker)
        .await
        .unwrap()
        .unwrap();
    assert!(!drained);
    assert!(elapsed < TEST_DEADLINE, "bounded drain took {elapsed:?}");
    assert!(lifecycle.drain(TEST_DEADLINE).await);
}

#[tokio::test]
async fn an_empty_lifecycle_drains_and_repeated_cancellation_is_harmless() {
    let lifecycle = Shutdown::default();
    lifecycle.cancel();
    lifecycle.cancel();
    assert!(lifecycle.is_cancelled());
    assert!(lifecycle.drain(Duration::from_millis(100)).await);
    assert!(lifecycle.drain(Duration::from_millis(100)).await);
}

#[tokio::test]
async fn asynchronous_work_admitted_after_shutdown_is_dropped_without_being_polled() {
    let lifecycle = Shutdown::new();
    assert!(lifecycle.drain(TEST_DEADLINE).await);
    let polls = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let late_job = lifecycle.spawn(ObserveFuture {
        polls: polls.clone(),
        dropped: dropped.clone(),
    });
    tokio::time::timeout(TEST_DEADLINE, late_job)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(polls.load(Ordering::Acquire), 0);
    assert!(dropped.load(Ordering::Acquire));
    assert!(lifecycle.drain(TEST_DEADLINE).await);
}

#[tokio::test]
async fn late_blocking_work_keeps_its_result_and_can_observe_shutdown_before_doing_io() {
    let lifecycle = Shutdown::new();
    assert!(lifecycle.drain(TEST_DEADLINE).await);
    let flag = lifecycle.flag();
    let late_job = lifecycle.spawn_blocking(move || {
        if flag.load(Ordering::Acquire) {
            return "cancelled-before-io";
        }
        panic!("late blocking work did not see cancellation");
    });
    assert!(lifecycle.drain(TEST_DEADLINE).await);
    assert_eq!(
        tokio::time::timeout(TEST_DEADLINE, late_job)
            .await
            .unwrap()
            .unwrap(),
        "cancelled-before-io"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_blocking_cleanup_is_tracked_even_after_an_earlier_drain_closed_the_tracker() {
    let lifecycle = Shutdown::new();
    assert!(lifecycle.drain(TEST_DEADLINE).await);
    let flag = lifecycle.flag();
    let (release_cleanup, cleanup_permission) = mpsc::channel::<()>();
    let (started_tx, started_rx) = oneshot::channel();
    let cleanup = lifecycle.spawn_blocking(move || {
        let observed_cancel = flag.load(Ordering::Acquire);
        started_tx.send(()).unwrap();
        // Deliberately hold a cleanup step; this is not new source I/O.
        let _ = cleanup_permission.recv();
        observed_cancel
    });
    tokio::time::timeout(TEST_DEADLINE, started_rx)
        .await
        .unwrap()
        .unwrap();
    let drained_while_cleanup_waits = lifecycle.drain(Duration::from_millis(40)).await;
    drop(release_cleanup);
    assert!(tokio::time::timeout(TEST_DEADLINE, cleanup)
        .await
        .unwrap()
        .unwrap());
    assert!(!drained_while_cleanup_waits);
    assert!(lifecycle.drain(TEST_DEADLINE).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_waits_for_graceful_jobs_to_finish_explicit_cancellation_cleanup() {
    let lifecycle = Shutdown::new();
    let graceful_shutdown = lifecycle.clone();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_cleanup, cleanup_allowed) = oneshot::channel();
    let cleaned_up = Arc::new(AtomicBool::new(false));
    let cleaned_up_in_job = cleaned_up.clone();
    let graceful_job = lifecycle.spawn_graceful(async move {
        started_tx.send(()).unwrap();
        graceful_shutdown.cancelled().await;
        let _ = cleanup_allowed.await;
        cleaned_up_in_job.store(true, Ordering::Release);
        "cleanup-finished"
    });
    tokio::time::timeout(TEST_DEADLINE, started_rx)
        .await
        .unwrap()
        .unwrap();
    let drain_shutdown = lifecycle.clone();
    let drain = tokio::spawn(async move { drain_shutdown.drain(TEST_DEADLINE).await });
    tokio::time::timeout(TEST_DEADLINE, lifecycle.cancelled())
        .await
        .unwrap();
    assert!(!cleaned_up.load(Ordering::Acquire));
    release_cleanup.send(()).unwrap();
    assert!(tokio::time::timeout(TEST_DEADLINE, drain)
        .await
        .unwrap()
        .unwrap());
    assert!(cleaned_up.load(Ordering::Acquire));
    assert_eq!(
        tokio::time::timeout(TEST_DEADLINE, graceful_job)
            .await
            .unwrap()
            .unwrap(),
        "cleanup-finished"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_shutdown_flag_releases_a_real_cache_reader_waiting_for_its_header() {
    use stream_cache::{CacheKey, StreamCache};
    use transcode::{TranscodeMode, TranscodeQuality};

    let lifecycle = Shutdown::new();
    let cache =
        StreamCache::new(std::path::PathBuf::new(), true).with_shutdown_flag(lifecycle.flag());
    let key = CacheKey::new(
        "unproduced",
        20,
        TranscodeMode::Fixed,
        TranscodeQuality::High,
    );
    let writer = cache.writer(key.clone()).unwrap().unwrap();
    let reader = cache.reader(&key).unwrap().unwrap();
    let (tx, receiver_kept_open) = tokio::sync::mpsc::channel(1);
    let (started_tx, started_rx) = oneshot::channel();
    let reader_job = lifecycle.spawn_blocking(move || {
        started_tx.send(()).unwrap();
        reader.stream_to(0, &tx)
    });
    tokio::time::timeout(TEST_DEADLINE, started_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(lifecycle.drain(TEST_DEADLINE).await);
    assert_eq!(
        tokio::time::timeout(TEST_DEADLINE, reader_job)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err(),
        "server shutting down"
    );
    // The connection receiver and cache writer still exist. The shared stop
    // flag, rather than either channel/resource closing, released this reader.
    drop(receiver_kept_open);
    drop(writer);
}
