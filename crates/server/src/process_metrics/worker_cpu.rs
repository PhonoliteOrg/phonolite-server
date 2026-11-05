//! Cumulative CPU for explicitly registered internal workers. Sampling is on
//! demand; completed jobs are retained in the total even between console polls.
//! Unix reaping belongs here so an exited worker cannot disappear before its
//! final rusage has been collected. Windows retains a duplicate process handle.

use std::collections::HashMap;
use std::process::{Child, ExitStatus};
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::Mutex;

struct Usage {
    cpu: Duration,
    exited: bool,
}

struct Ledger<T> {
    next: u64,
    completed: Duration,
    incomplete: bool,
    processes: HashMap<u64, T>,
}

impl<T> Default for Ledger<T> {
    fn default() -> Self {
        Self {
            next: 0,
            completed: Duration::ZERO,
            incomplete: false,
            processes: HashMap::new(),
        }
    }
}

impl<T> Ledger<T> {
    fn insert(&mut self, process: T) -> u64 {
        let id = self.next;
        self.next = self.next.checked_add(1).expect("worker CPU ID exhausted");
        self.processes.insert(id, process);
        id
    }

    fn finish(&mut self, id: u64, cpu: Duration) {
        if self.processes.remove(&id).is_some() {
            if let Some(total) = self.completed.checked_add(cpu) {
                self.completed = total;
            } else {
                self.incomplete = true;
            }
        }
    }

    fn sample(&mut self, mut read: impl FnMut(&mut T) -> Option<Usage>) -> Option<Duration> {
        let mut live = Duration::ZERO;
        let mut failed = self.incomplete;
        let mut finished = Vec::new();
        for (&id, process) in &mut self.processes {
            match read(process) {
                Some(Usage { cpu, exited: true }) => finished.push((id, cpu)),
                Some(Usage { cpu, exited: false }) => match live.checked_add(cpu) {
                    Some(total) => live = total,
                    None => failed = true,
                },
                None => failed = true,
            }
        }
        for (id, cpu) in finished {
            self.finish(id, cpu);
        }
        if failed || self.incomplete {
            None
        } else {
            self.completed.checked_add(live)
        }
    }
}

fn ledger() -> &'static Mutex<Ledger<PlatformProcess>> {
    static LEDGER: OnceLock<Mutex<Ledger<PlatformProcess>>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(Ledger::default()))
}

pub(crate) struct WorkerCpu {
    id: Option<u64>,
    #[cfg(unix)]
    exit: std::sync::Arc<Mutex<Option<ExitStatus>>>,
}

/// All internal worker waits go through the same owner, including watcher and
/// blocking tag jobs. The cleanup thread only waits for a killed child; it does
/// not collect periodic telemetry or retain application/database state.
pub(crate) struct TrackedChild {
    child: Option<Child>,
    cpu: Option<WorkerCpu>,
}

impl TrackedChild {
    pub(crate) fn new(child: Child) -> Self {
        let cpu = WorkerCpu::track(&child);
        Self {
            child: Some(child),
            cpu: Some(cpu),
        }
    }

    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.cpu
            .as_ref()
            .unwrap()
            .try_wait(self.child.as_mut().unwrap())
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        self.cpu
            .as_ref()
            .unwrap()
            .kill(self.child.as_mut().unwrap())
    }
}

impl std::ops::Deref for TrackedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        self.child.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for TrackedChild {
    fn deref_mut(&mut self) -> &mut Child {
        self.child.as_mut().unwrap()
    }
}

impl Drop for TrackedChild {
    fn drop(&mut self) {
        if matches!(self.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.kill();
        if matches!(self.try_wait(), Ok(Some(_))) {
            return;
        }
        let mut child = self.child.take().unwrap();
        let cpu = self.cpu.take().unwrap();
        let _ = std::thread::Builder::new()
            .name("worker-cpu-reaper".into())
            .spawn(move || loop {
                match cpu.try_wait(&mut child) {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                }
            });
    }
}

impl WorkerCpu {
    /// Register immediately after spawn, before any owner may wait/reap it.
    pub(crate) fn track(child: &Child) -> Self {
        #[cfg(unix)]
        let exit = std::sync::Arc::new(Mutex::new(None));
        let mut ledger = ledger().lock();
        #[cfg(windows)]
        let process = PlatformProcess::new(child);
        #[cfg(unix)]
        let process = PlatformProcess::new(child, exit.clone());
        #[cfg(not(any(unix, windows)))]
        let process = PlatformProcess::new(child);
        let id = match process {
            Some(process) => Some(ledger.insert(process)),
            None => {
                // Registration failures must never produce a plausible partial
                // total. The missing worker's final CPU cannot be recovered.
                ledger.incomplete = true;
                None
            }
        };
        Self {
            id,
            #[cfg(unix)]
            exit,
        }
    }

    pub(crate) fn try_wait(&self, child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
        #[cfg(unix)]
        {
            let _ = child;
            let mut ledger = ledger().lock();
            if let Some(status) = *self.exit.lock() {
                return Ok(Some(status));
            }
            let id = self
                .id
                .ok_or_else(|| std::io::Error::other("worker CPU registration failed"))?;
            let process = ledger
                .processes
                .get_mut(&id)
                .ok_or_else(|| std::io::Error::other("worker CPU registration missing"))?;
            match process.reap()? {
                Some(cpu) => {
                    let status = *self.exit.lock();
                    ledger.finish(id, cpu);
                    Ok(status)
                }
                None => Ok(None),
            }
        }
        #[cfg(not(unix))]
        {
            let result = child.try_wait();
            if matches!(result, Ok(Some(_))) {
                self.fold_finished();
            }
            result
        }
    }

    fn kill(&self, child: &mut Child) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let _ = child;
            self.signal_unix(|pid| {
                // The registry lock prevents a concurrent wait4 between the
                // cached-exit check and signal. An unreaped PID cannot be reused.
                if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            })
        }
        #[cfg(not(unix))]
        {
            child.kill()
        }
    }

    #[cfg(unix)]
    fn signal_unix(
        &self,
        signal: impl FnOnce(libc::pid_t) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let mut ledger = ledger().lock();
        if self.exit.lock().is_some() {
            return Ok(());
        }
        let id = self
            .id
            .ok_or_else(|| std::io::Error::other("worker CPU registration failed"))?;
        let process = ledger
            .processes
            .get_mut(&id)
            .ok_or_else(|| std::io::Error::other("worker CPU registration missing"))?;
        if let Some(cpu) = process.reap()? {
            ledger.finish(id, cpu);
            Ok(())
        } else {
            signal(process.pid)
        }
    }

    #[cfg(not(unix))]
    fn fold_finished(&self) {
        let Some(id) = self.id else { return };
        let mut ledger = ledger().lock();
        if let Some(Usage { cpu, exited: true }) = ledger
            .processes
            .get_mut(&id)
            .and_then(PlatformProcess::read)
        {
            ledger.finish(id, cpu);
        }
    }
}

pub(super) fn cumulative_cpu() -> Option<Duration> {
    ledger().lock().sample(PlatformProcess::read)
}

#[cfg(windows)]
struct PlatformProcess(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl PlatformProcess {
    fn new(child: &Child) -> Option<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        unsafe {
            let process = GetCurrentProcess();
            let mut copy = std::ptr::null_mut();
            if DuplicateHandle(
                process,
                child.as_raw_handle(),
                process,
                &mut copy,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            ) == 0
            {
                return None;
            }
            Some(Self(OwnedHandle::from_raw_handle(copy)))
        }
    }

    fn read(&mut self) -> Option<Usage> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{FILETIME, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{GetProcessTimes, WaitForSingleObject};
        unsafe {
            // A retained handle remains valid after wait/reap and cannot refer
            // to a reused PID. Read terminal CPU after observing process exit.
            let exited = match WaitForSingleObject(self.0.as_raw_handle(), 0) {
                WAIT_OBJECT_0 => true,
                WAIT_TIMEOUT => false,
                _ => return None,
            };
            let mut creation: FILETIME = std::mem::zeroed();
            let mut exit: FILETIME = std::mem::zeroed();
            let mut kernel: FILETIME = std::mem::zeroed();
            let mut user: FILETIME = std::mem::zeroed();
            if GetProcessTimes(
                self.0.as_raw_handle(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            ) == 0
            {
                return None;
            }
            let ticks = |time: FILETIME| {
                (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
            };
            let ticks = ticks(kernel).checked_add(ticks(user))?;
            Some(Usage {
                cpu: Duration::new(ticks / 10_000_000, ((ticks % 10_000_000) * 100) as u32),
                exited,
            })
        }
    }
}

#[cfg(unix)]
struct PlatformProcess {
    pid: libc::pid_t,
    exit: std::sync::Arc<Mutex<Option<ExitStatus>>>,
}

#[cfg(unix)]
impl PlatformProcess {
    fn new(child: &Child, exit: std::sync::Arc<Mutex<Option<ExitStatus>>>) -> Option<Self> {
        Some(Self {
            pid: libc::pid_t::try_from(child.id()).ok()?,
            exit,
        })
    }

    fn reap(&mut self) -> std::io::Result<Option<Duration>> {
        use std::os::unix::process::ExitStatusExt;
        loop {
            let mut status = 0;
            let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
            // Only this registry reaps workers. The same lock covers sampling,
            // normal completion, cancellation and the existing deferred reaper.
            let result = unsafe { libc::wait4(self.pid, &mut status, libc::WNOHANG, &mut usage) };
            if result == 0 {
                return Ok(None);
            }
            if result == self.pid {
                *self.exit.lock() = Some(ExitStatus::from_raw(status));
                return timeval_duration(usage.ru_utime)
                    .and_then(|user| user.checked_add(timeval_duration(usage.ru_stime)?))
                    .map(Some)
                    .ok_or_else(|| std::io::Error::other("invalid worker CPU time"));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn read(&mut self) -> Option<Usage> {
        if let Some(cpu) = self.reap().ok()? {
            return Some(Usage { cpu, exited: true });
        }
        match live_cpu(self.pid) {
            Some(cpu) => Some(Usage { cpu, exited: false }),
            // On platforms that cannot inspect a zombie, retry wait4 once in
            // case the process exited between the nonblocking wait and read.
            None => self.reap().ok()?.map(|cpu| Usage { cpu, exited: true }),
        }
    }
}

#[cfg(unix)]
fn timeval_duration(time: libc::timeval) -> Option<Duration> {
    let seconds = u64::try_from(time.tv_sec).ok()?;
    let micros = u32::try_from(time.tv_usec).ok()?;
    (micros < 1_000_000).then(|| Duration::new(seconds, micros * 1_000))
}

#[cfg(target_os = "linux")]
fn live_cpu(pid: libc::pid_t) -> Option<Duration> {
    let ticks_per_second = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
    parse_linux_cpu(
        &std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?,
        ticks_per_second,
    )
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_cpu(stat: &str, ticks_per_second: u64) -> Option<Duration> {
    if ticks_per_second == 0 {
        return None;
    }
    // comm may contain spaces and ')' characters. Fields after its final ')'
    // start at field 3; utime/stime are fields 14 and 15, excluding descendants.
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let user = fields.nth(11)?.parse::<u64>().ok()?;
    let system = fields.next()?.parse::<u64>().ok()?;
    let ticks = user.checked_add(system)?;
    Some(Duration::new(
        ticks / ticks_per_second,
        ((u128::from(ticks % ticks_per_second) * 1_000_000_000) / u128::from(ticks_per_second))
            as u32,
    ))
}

#[cfg(target_os = "macos")]
fn live_cpu(pid: libc::pid_t) -> Option<Duration> {
    unsafe {
        let mut usage: libc::rusage_info_v2 = std::mem::zeroed();
        if libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V2,
            (&mut usage as *mut libc::rusage_info_v2).cast(),
        ) != 0
        {
            return None;
        }
        Some(Duration::from_nanos(
            usage.ri_user_time.checked_add(usage.ri_system_time)?,
        ))
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn live_cpu(_pid: libc::pid_t) -> Option<Duration> {
    None
}

#[cfg(not(any(unix, windows)))]
struct PlatformProcess;
#[cfg(not(any(unix, windows)))]
impl PlatformProcess {
    fn new(_child: &Child) -> Option<Self> {
        None
    }
    fn read(&mut self) -> Option<Usage> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct Fake {
        millis: u64,
        exited: bool,
        failed: bool,
    }
    fn read(fake: &mut Fake) -> Option<Usage> {
        (!fake.failed).then_some(Usage {
            cpu: Duration::from_millis(fake.millis),
            exited: fake.exited,
        })
    }
    fn process(millis: u64, exited: bool) -> Fake {
        Fake {
            millis,
            exited,
            failed: false,
        }
    }

    #[test]
    fn jobs_completed_between_polls_are_retained_exactly_once() {
        let mut ledger = Ledger::default();
        let first = ledger.insert(process(35, true));
        ledger.finish(first, Duration::from_millis(35));
        ledger.finish(first, Duration::from_millis(35));
        ledger.insert(process(65, true));
        assert_eq!(ledger.sample(read), Some(Duration::from_millis(100)));
        assert!(ledger.processes.is_empty());
        assert_eq!(ledger.sample(read), Some(Duration::from_millis(100)));
    }

    #[test]
    fn overlapping_live_completed_and_cancelled_jobs_have_monotonic_totals() {
        let mut ledger = Ledger::default();
        let active = ledger.insert(process(20, false));
        let killed = ledger.insert(process(30, false));
        assert_eq!(ledger.sample(read), Some(Duration::from_millis(50)));
        *ledger.processes.get_mut(&killed).unwrap() = process(45, true);
        ledger.processes.get_mut(&active).unwrap().millis = 50;
        assert_eq!(ledger.sample(read), Some(Duration::from_millis(95)));
        *ledger.processes.get_mut(&active).unwrap() = process(70, true);
        assert_eq!(ledger.sample(read), Some(Duration::from_millis(115)));
        assert_eq!(ledger.sample(read), Some(Duration::from_millis(115)));
    }

    #[test]
    fn failed_reads_never_return_partial_cpu_and_can_recover() {
        let mut ledger = Ledger::default();
        let failed = ledger.insert(Fake {
            millis: 70,
            exited: false,
            failed: true,
        });
        ledger.insert(process(30, true));
        assert_eq!(ledger.sample(read), None);
        assert_eq!(ledger.completed, Duration::from_millis(30));
        ledger.processes.get_mut(&failed).unwrap().failed = false;
        assert_eq!(ledger.sample(read), Some(Duration::from_millis(100)));
        ledger.incomplete = true;
        assert_eq!(ledger.sample(read), None);
    }

    #[test]
    fn linux_cpu_parser_handles_complex_names_and_ignores_child_cpu() {
        let stat = "42 (worker name ) suffix) S 1 2 3 4 5 6 7 8 9 10 125 25 9999 9999";
        assert_eq!(
            parse_linux_cpu(stat, 100),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(parse_linux_cpu(stat, 0), None);
        assert_eq!(parse_linux_cpu("truncated", 100), None);
        assert_eq!(parse_linux_cpu("42 (x) S 1 2", 100), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_reaped_worker_cannot_signal_a_reused_pid() {
        use std::os::unix::process::ExitStatusExt;
        let worker = WorkerCpu {
            id: None,
            exit: std::sync::Arc::new(Mutex::new(Some(ExitStatus::from_raw(0)))),
        };
        // Simulate sampling having already reaped/removed this worker. No child
        // is launched; a late kill must return before looking up or signalling PID.
        worker
            .signal_unix(|_| panic!("signalled an already reaped worker"))
            .unwrap();
    }
}
