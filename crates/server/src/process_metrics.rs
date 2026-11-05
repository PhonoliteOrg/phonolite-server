//! Bounded, on-demand telemetry. CPU includes the main process and registered
//! internal workers; memory is the main process only. No sampling task is created.
//! CPU is expressed as 100% per logical core.

#[path = "process_metrics/worker_cpu.rs"]
pub(crate) mod worker_cpu;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;

use crate::network_metrics::{NetworkMetrics, TrafficTotals};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const MAX_CPU_INTERVAL: Duration = Duration::from_secs(6);
const HISTORY_WINDOW: Duration = Duration::from_secs(5 * 60);
const MAX_SAMPLES: usize = 150;

#[derive(Clone)]
pub struct ProcessMetrics {
    inner: Arc<Mutex<Sampler>>,
    network: NetworkMetrics,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub pid: u32,
    pub uptime_secs: u64,
    pub logical_cpus: usize,
    pub interval_ms: u64,
    /// Devices currently playing server audio, including buffered playback.
    pub active_streams: u64,
    pub sent_bytes: u64,
    pub received_bytes: u64,
    pub history: Vec<Sample>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Sample {
    pub timestamp_ms: u64,
    pub cpu_percent: Option<f64>,
    pub cpu_status: CpuStatus,
    /// Resident memory (working set on Windows), not virtual or peak memory.
    pub memory_bytes: Option<u64>,
    pub sent_bytes_per_sec: Option<f64>,
    pub received_bytes_per_sec: Option<f64>,
    /// Playing devices at this sample, independent of open transfers.
    pub active_streams: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuStatus {
    Available,
    Measuring,
    Unavailable,
}

struct Sampler {
    started: Instant,
    logical_cpus: usize,
    last_sample_at: Option<Instant>,
    previous_cpu: Option<(Instant, Duration)>,
    network: NetworkMetrics,
    previous_traffic: Option<(Instant, TrafficTotals)>,
    history: VecDeque<(Instant, Sample)>,
}

struct Reading {
    cpu_time: Option<Duration>,
    memory_bytes: Option<u64>,
}

impl Default for ProcessMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessMetrics {
    /// Construct once during server startup and share across console requests.
    pub fn new() -> Self {
        let network = NetworkMetrics::default();
        Self {
            inner: Arc::new(Mutex::new(Sampler {
                started: Instant::now(),
                logical_cpus: logical_cpus(),
                last_sample_at: None,
                previous_cpu: None,
                network: network.clone(),
                previous_traffic: None,
                history: VecDeque::with_capacity(MAX_SAMPLES),
            })),
            network,
        }
    }

    pub fn network(&self) -> NetworkMetrics {
        self.network.clone()
    }

    /// All viewers share the same sample; requests inside two seconds only
    /// copy the bounded history. A missing native measurement stays null.
    pub fn snapshot(&self) -> Snapshot {
        let mut sampler = self.inner.lock();
        let now = Instant::now();
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        sampler.refresh(now, timestamp_ms, read_process);
        let totals = self.network.totals();
        Snapshot {
            pid: std::process::id(),
            uptime_secs: now.saturating_duration_since(sampler.started).as_secs(),
            logical_cpus: sampler.logical_cpus,
            interval_ms: SAMPLE_INTERVAL.as_millis() as u64,
            active_streams: self.network.playing_devices(),
            sent_bytes: totals.sent_bytes,
            received_bytes: totals.received_bytes,
            history: sampler
                .history
                .iter()
                .map(|(_, sample)| sample.clone())
                .collect(),
        }
    }
}

impl Sampler {
    fn refresh(&mut self, now: Instant, timestamp_ms: u64, read: impl FnOnce() -> Reading) {
        if self
            .last_sample_at
            .is_some_and(|at| now.saturating_duration_since(at) < SAMPLE_INTERVAL)
        {
            return;
        }
        let reading = read();
        let traffic = self.network.totals();
        let rates = self.previous_traffic.and_then(|(at, previous)| {
            let elapsed = now.saturating_duration_since(at);
            if elapsed.is_zero() || elapsed > MAX_CPU_INTERVAL {
                return None;
            }
            Some((
                traffic.sent_bytes.checked_sub(previous.sent_bytes)? as f64 / elapsed.as_secs_f64(),
                traffic
                    .received_bytes
                    .checked_sub(previous.received_bytes)? as f64
                    / elapsed.as_secs_f64(),
            ))
        });
        self.previous_traffic = Some((now, traffic));
        let cpu_percent = reading.cpu_time.and_then(|current| {
            let (previous_at, previous) = self.previous_cpu?;
            let elapsed = now.saturating_duration_since(previous_at);
            if elapsed > MAX_CPU_INTERVAL {
                return None;
            }
            cpu_percent(previous, current, elapsed)
        });
        let cpu_status = if reading.cpu_time.is_none() {
            CpuStatus::Unavailable
        } else if cpu_percent.is_some() {
            CpuStatus::Available
        } else {
            CpuStatus::Measuring
        };
        // A failed read breaks the CPU baseline so the next successful sample
        // does not misleadingly average over the period without measurements.
        // Resumed polling and reset counters also start a fresh baseline;
        // successful reads waiting for the next interval remain measuring.
        self.previous_cpu = reading.cpu_time.map(|cpu_time| (now, cpu_time));
        self.last_sample_at = Some(now);
        while self
            .history
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= HISTORY_WINDOW)
        {
            self.history.pop_front();
        }
        if self.history.len() == MAX_SAMPLES {
            self.history.pop_front();
        }
        self.history.push_back((
            now,
            Sample {
                timestamp_ms,
                cpu_percent,
                cpu_status,
                memory_bytes: reading.memory_bytes,
                sent_bytes_per_sec: rates.map(|(sent, _)| sent),
                received_bytes_per_sec: rates.map(|(_, received)| received),
                active_streams: self.network.playing_devices(),
            },
        ));
    }
}

fn cpu_percent(previous: Duration, current: Duration, elapsed: Duration) -> Option<f64> {
    if elapsed.is_zero() {
        return None;
    }
    let used = current.checked_sub(previous)?;
    let percent = used.as_secs_f64() / elapsed.as_secs_f64() * 100.0;
    percent.is_finite().then_some(percent)
}

fn logical_cpus() -> usize {
    #[cfg(windows)]
    let native = unsafe {
        // ALL_PROCESSOR_GROUPS covers hosts with more than 64 logical CPUs.
        windows_sys::Win32::System::Threading::GetActiveProcessorCount(0xffff) as usize
    };
    #[cfg(unix)]
    let native = usize::try_from(unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }).unwrap_or(0);
    #[cfg(not(any(unix, windows)))]
    let native = 0;
    if native > 0 {
        native
    } else {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
    }
}

fn include_workers(cpu_time: Option<Duration>) -> Option<Duration> {
    cpu_time?.checked_add(worker_cpu::cumulative_cpu()?)
}

#[cfg(windows)]
fn read_process() -> Reading {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    fn ticks(value: FILETIME) -> u64 {
        (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
    }

    // All output pointers refer to initialized, correctly sized local structs.
    // GetCurrentProcess returns a pseudo-handle that must not be closed.
    unsafe {
        let process = GetCurrentProcess();
        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let cpu_time =
            if GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) != 0 {
                ticks(kernel).checked_add(ticks(user)).map(|ticks| {
                    Duration::new(ticks / 10_000_000, ((ticks % 10_000_000) * 100) as u32)
                })
            } else {
                None
            };
        let mut memory: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        let memory_size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        memory.cb = memory_size;
        let memory_bytes = if K32GetProcessMemoryInfo(process, &mut memory, memory_size) != 0 {
            Some(memory.WorkingSetSize as u64)
        } else {
            None
        };
        Reading {
            cpu_time: include_workers(cpu_time),
            memory_bytes,
        }
    }
}

#[cfg(unix)]
fn read_process() -> Reading {
    fn timeval_duration(value: libc::timeval) -> Option<Duration> {
        let seconds = u64::try_from(value.tv_sec).ok()?;
        let micros = u32::try_from(value.tv_usec).ok()?;
        (micros < 1_000_000).then(|| Duration::new(seconds, micros * 1_000))
    }

    // RUSAGE_SELF sums user and system CPU for all threads in this process.
    // Registered workers are added below. ru_maxrss is a peak, so it is not
    // used as the current resident-memory measurement.
    let cpu_time = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
            timeval_duration(usage.ru_utime)
                .and_then(|user| user.checked_add(timeval_duration(usage.ru_stime)?))
        } else {
            None
        }
    };
    Reading {
        cpu_time: include_workers(cpu_time),
        memory_bytes: resident_memory(),
    }
}

#[cfg(target_os = "linux")]
fn resident_memory() -> Option<u64> {
    // statm's resident column is in pages. This cheap kernel counter can be
    // approximate; reading smaps would make regular console polling costly.
    let pages = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = u64::try_from(pages).ok().filter(|size| *size > 0)?;
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    parse_resident_pages(&statm, page_size)
}

#[cfg(any(target_os = "linux", test))]
fn parse_resident_pages(statm: &str, page_size: u64) -> Option<u64> {
    statm
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()?
        .checked_mul(page_size)
}

#[cfg(target_os = "macos")]
fn resident_memory() -> Option<u64> {
    // task_info writes the exact Mach structure represented by libc. The task
    // is this process only, so no task-for-pid privileges or process scan apply.
    #[allow(deprecated)]
    unsafe {
        let mut info: libc::mach_task_basic_info = std::mem::zeroed();
        let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
        let result = libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            (&mut info as *mut libc::mach_task_basic_info).cast(),
            &mut count,
        );
        (result == libc::KERN_SUCCESS && count == libc::MACH_TASK_BASIC_INFO_COUNT)
            .then_some(info.resident_size)
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn resident_memory() -> Option<u64> {
    None
}

#[cfg(not(any(unix, windows)))]
fn read_process() -> Reading {
    Reading {
        cpu_time: None,
        memory_bytes: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sampler(started: Instant) -> Sampler {
        Sampler {
            started,
            logical_cpus: 4,
            last_sample_at: None,
            previous_cpu: None,
            network: NetworkMetrics::default(),
            previous_traffic: None,
            history: VecDeque::new(),
        }
    }

    fn reading(cpu_millis: u64) -> Reading {
        Reading {
            cpu_time: Some(Duration::from_millis(cpu_millis)),
            memory_bytes: Some(12_345_678),
        }
    }

    #[test]
    fn cpu_usage_sums_cores_and_rejects_invalid_intervals() {
        assert_eq!(
            cpu_percent(
                Duration::ZERO,
                Duration::from_secs(3),
                Duration::from_secs(2)
            ),
            Some(150.0)
        );
        assert_eq!(
            cpu_percent(Duration::ZERO, Duration::ZERO, Duration::from_secs(2)),
            Some(0.0)
        );
        assert_eq!(
            cpu_percent(Duration::ZERO, Duration::from_secs(1), Duration::ZERO),
            None
        );
        assert_eq!(
            cpu_percent(
                Duration::from_secs(3),
                Duration::from_secs(2),
                Duration::from_secs(2)
            ),
            None
        );
    }

    #[test]
    fn polling_shares_cached_samples_and_failures_reset_the_cpu_baseline() {
        let start = Instant::now();
        let mut sampler = sampler(start);
        sampler.refresh(start, 0, || reading(1_000));
        assert_eq!(sampler.history.back().unwrap().1.cpu_percent, None);
        assert_eq!(
            sampler.history.back().unwrap().1.cpu_status,
            CpuStatus::Measuring
        );
        sampler.refresh(start + Duration::from_secs(1), 1_000, || {
            panic!("cached requests must not measure again")
        });
        assert_eq!(sampler.history.len(), 1);
        sampler.refresh(start + SAMPLE_INTERVAL, 2_000, || reading(2_000));
        assert_eq!(sampler.history.back().unwrap().1.cpu_percent, Some(50.0));
        assert_eq!(
            sampler.history.back().unwrap().1.cpu_status,
            CpuStatus::Available
        );
        sampler.refresh(start + Duration::from_secs(4), 4_000, || Reading {
            cpu_time: None,
            memory_bytes: None,
        });
        assert_eq!(sampler.history.back().unwrap().1.memory_bytes, None);
        assert_eq!(sampler.history.back().unwrap().1.cpu_percent, None);
        assert_eq!(
            sampler.history.back().unwrap().1.cpu_status,
            CpuStatus::Unavailable
        );
        sampler.refresh(start + Duration::from_secs(6), 6_000, || reading(3_000));
        assert_eq!(sampler.history.back().unwrap().1.cpu_percent, None);
        assert_eq!(
            sampler.history.back().unwrap().1.cpu_status,
            CpuStatus::Measuring
        );
        sampler.refresh(start + Duration::from_secs(8), 8_000, || reading(3_500));
        assert_eq!(sampler.history.back().unwrap().1.cpu_percent, Some(25.0));
        assert_eq!(
            sampler.history.back().unwrap().1.cpu_status,
            CpuStatus::Available
        );
    }

    #[test]
    fn failed_initial_measurement_is_unavailable_until_a_fresh_interval_is_collected() {
        let start = Instant::now();
        let mut sampler = sampler(start);
        sampler.refresh(start, 0, || Reading {
            cpu_time: None,
            memory_bytes: Some(12_345_678),
        });
        let sample = &sampler.history.back().unwrap().1;
        assert_eq!(sample.cpu_percent, None);
        assert_eq!(sample.cpu_status, CpuStatus::Unavailable);
        assert_eq!(
            serde_json::to_value(sample).unwrap()["cpu_status"],
            "unavailable"
        );
        sampler.refresh(start + SAMPLE_INTERVAL, 2_000, || reading(1_000));
        let sample = &sampler.history.back().unwrap().1;
        assert_eq!(sample.cpu_percent, None);
        assert_eq!(sample.cpu_status, CpuStatus::Measuring);
        assert_eq!(
            serde_json::to_value(sample).unwrap()["cpu_status"],
            "measuring"
        );
        sampler.refresh(start + SAMPLE_INTERVAL * 2, 4_000, || reading(1_000));
        let sample = &sampler.history.back().unwrap().1;
        assert_eq!(sample.cpu_percent, Some(0.0));
        assert_eq!(sample.cpu_status, CpuStatus::Available);
        assert_eq!(
            serde_json::to_value(sample).unwrap()["cpu_status"],
            "available"
        );
    }

    #[test]
    fn reset_cpu_counters_reestablish_a_baseline_instead_of_reporting_unavailable() {
        let start = Instant::now();
        let mut sampler = sampler(start);
        sampler.refresh(start, 0, || reading(2_000));
        sampler.refresh(start + SAMPLE_INTERVAL, 2_000, || reading(1_000));
        assert_eq!(sampler.history.back().unwrap().1.cpu_percent, None);
        assert_eq!(
            sampler.history.back().unwrap().1.cpu_status,
            CpuStatus::Measuring
        );
        sampler.refresh(start + SAMPLE_INTERVAL * 2, 4_000, || reading(1_500));
        assert_eq!(sampler.history.back().unwrap().1.cpu_percent, Some(25.0));
        assert_eq!(
            sampler.history.back().unwrap().1.cpu_status,
            CpuStatus::Available
        );
    }

    #[test]
    fn history_is_bounded_and_discards_samples_after_a_long_idle_period() {
        let start = Instant::now();
        let mut sampler = sampler(start);
        for index in 0..500_u64 {
            sampler.refresh(
                start + Duration::from_secs(index * 2),
                index * 2_000,
                || reading(index * 100),
            );
        }
        assert_eq!(sampler.history.len(), MAX_SAMPLES);
        let oldest = sampler.history.front().unwrap().1.timestamp_ms;
        assert_eq!(oldest, 700_000);
        sampler.refresh(start + Duration::from_secs(1_600), 1_600_000, || {
            reading(50_000)
        });
        assert_eq!(sampler.history.len(), 1);
    }

    #[test]
    fn resumed_polling_collects_a_fresh_cpu_interval_after_a_gap() {
        let start = Instant::now();
        let mut sampler = sampler(start);
        sampler.refresh(start, 0, || reading(1_000));
        sampler.refresh(start + Duration::from_secs(2), 2_000, || reading(2_000));
        assert_eq!(sampler.history.back().unwrap().1.cpu_percent, Some(50.0));
        sampler.refresh(start + Duration::from_secs(20), 20_000, || reading(10_000));
        assert_eq!(sampler.history.len(), 3);
        assert_eq!(sampler.history.back().unwrap().1.cpu_percent, None);
        assert_eq!(
            sampler.history.back().unwrap().1.cpu_status,
            CpuStatus::Measuring
        );
        assert_eq!(
            sampler.history.back().unwrap().1.memory_bytes,
            Some(12_345_678)
        );
        sampler.refresh(start + Duration::from_secs(22), 22_000, || reading(10_500));
        assert_eq!(sampler.history.back().unwrap().1.cpu_percent, Some(25.0));
        assert_eq!(
            sampler.history.back().unwrap().1.cpu_status,
            CpuStatus::Available
        );
    }

    #[test]
    fn traffic_rates_use_elapsed_time_and_restart_after_monitoring_gaps() {
        let start = Instant::now();
        let mut sampler = sampler(start);
        sampler.network.record_sent(4096);
        sampler.refresh(start, 0, || reading(0));
        assert_eq!(sampler.history.back().unwrap().1.sent_bytes_per_sec, None);
        let stream = sampler.network.stream_started();
        sampler
            .network
            .report_playback_presence("alice", "session", "desktop", 1, true)
            .unwrap();
        sampler.network.record_sent(2048);
        sampler.network.record_received(1024);
        sampler.refresh(start + Duration::from_secs(2), 2000, || reading(0));
        let sample = &sampler.history.back().unwrap().1;
        assert_eq!(sample.sent_bytes_per_sec, Some(1024.0));
        assert_eq!(sample.received_bytes_per_sec, Some(512.0));
        assert_eq!(sample.active_streams, 1);
        sampler.network.record_sent(2048);
        sampler.refresh(start + Duration::from_secs(3), 3000, || {
            panic!("cached observations must not advance the traffic baseline")
        });
        sampler.refresh(start + Duration::from_secs(4), 4000, || reading(0));
        assert_eq!(
            sampler.history.back().unwrap().1.sent_bytes_per_sec,
            Some(1024.0)
        );
        assert_eq!(
            sampler.history.back().unwrap().1.received_bytes_per_sec,
            Some(0.0)
        );
        drop(stream);
        assert_eq!(sampler.network.playing_devices(), 1);
        sampler
            .network
            .report_playback_presence("alice", "session", "desktop", 2, false)
            .unwrap();
        sampler.network.record_received(100_000);
        sampler.refresh(start + Duration::from_secs(20), 20000, || reading(0));
        assert_eq!(
            sampler.history.back().unwrap().1.received_bytes_per_sec,
            None
        );
        assert_eq!(sampler.history.back().unwrap().1.active_streams, 0);
        sampler.refresh(start + Duration::from_secs(22), 22000, || reading(0));
        assert_eq!(
            sampler.history.back().unwrap().1.received_bytes_per_sec,
            Some(0.0)
        );
    }

    #[test]
    fn shared_snapshots_keep_totals_and_stream_counts_current_between_samples() {
        let metrics = ProcessMetrics::new();
        let first = metrics.snapshot();
        let shared = metrics.clone();
        let stream = shared.network().stream_started();
        for client in ["desktop", "phone"] {
            shared
                .network()
                .report_playback_presence("alice", "session", client, 1, true)
                .unwrap();
        }
        shared.network().record_sent(1234);
        shared.network().record_received(4321);
        let current = metrics.snapshot();
        assert_eq!(current.sent_bytes, 1234);
        assert_eq!(current.received_bytes, 4321);
        assert_eq!(current.active_streams, 2);
        assert_eq!(current.history.len(), first.history.len());
        drop(stream);
        assert_eq!(metrics.snapshot().active_streams, 2);
        shared
            .network()
            .report_playback_presence("alice", "session", "desktop", 2, false)
            .unwrap();
        assert_eq!(metrics.snapshot().active_streams, 1);
        shared
            .network()
            .report_playback_presence("alice", "session", "phone", 2, false)
            .unwrap();
        assert_eq!(metrics.snapshot().active_streams, 0);
    }

    #[test]
    fn linux_resident_pages_use_the_current_resident_column_and_check_overflow() {
        assert_eq!(
            parse_resident_pages("9000 1024 128 4 0 600 0\n", 4_096),
            Some(4_194_304)
        );
        assert_eq!(parse_resident_pages("9000 0 0", 4_096), Some(0));
        assert_eq!(parse_resident_pages("9000", 4_096), None);
        assert_eq!(parse_resident_pages("9000 bad 0", 4_096), None);
        assert_eq!(
            parse_resident_pages("9000 18446744073709551615", 4_096),
            None
        );
    }
}
