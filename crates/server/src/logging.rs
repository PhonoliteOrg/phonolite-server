use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tracing::{Event, Level, Metadata};
use tracing_subscriber::fmt::{
    format::{FormatEvent, FormatFields, Writer},
    time::{FormatTime, SystemTime},
    FmtContext, MakeWriter,
};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::config::{resolve_path, ServerConfig};

pub const LOG_MAX_LINES: usize = 10_000;
pub const LOG_TRIM_TO: usize = 5_000;

pub const LOG_ALL_FILE: &str = "all.log";
pub const LOG_INFO_FILE: &str = "info.log";
pub const LOG_WARN_FILE: &str = "warnings.log";
pub const LOG_ERROR_FILE: &str = "errors.log";
pub const LOG_ISSUE_FILE: &str = "issues.log";
pub const LOG_ACTIVITY_FILE: &str = "activities.log";
pub const LOG_DEBUG_FILE: &str = "debug.log";

#[derive(Clone)]
pub struct LogControl {
    reload: Arc<dyn Fn(EnvFilter) -> Result<(), String> + Send + Sync>,
    debug_enabled: Arc<AtomicBool>,
    clear_all: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}

impl LogControl {
    pub fn debug_enabled(&self) -> bool {
        self.debug_enabled.load(Ordering::Relaxed)
    }

    pub fn set_debug_enabled(&self, enabled: bool) -> Result<(), String> {
        self.debug_enabled.store(enabled, Ordering::Relaxed);
        (self.reload)(make_filter(enabled))
    }

    pub fn clear_all(&self) -> Result<(), String> {
        (self.clear_all)()
    }
}

#[derive(Clone)]
pub struct LogWriter {
    inner: Arc<Mutex<LogFileState>>,
}

struct LogFileState {
    path: PathBuf,
    file: File,
    line_count: usize,
}

impl LogWriter {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let file = open_log_file(&path)?;
        let mut line_count = count_lines(&path).unwrap_or(0);
        if line_count > LOG_MAX_LINES {
            let _ = trim_log_file(&path, LOG_TRIM_TO);
            line_count = count_lines(&path).unwrap_or(LOG_TRIM_TO);
            let reopened = open_log_file(&path)?;
            return Ok(Self {
                inner: Arc::new(Mutex::new(LogFileState {
                    path,
                    file: reopened,
                    line_count,
                })),
            });
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(LogFileState {
                path,
                file,
                line_count,
            })),
        })
    }

    fn clear(&self) -> io::Result<()> {
        let mut guard = self.inner.lock();
        guard.file.flush()?;
        guard.file = truncate_log_file(&guard.path)?;
        guard.line_count = 0;
        Ok(())
    }
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self.inner.lock();
        guard.file.write_all(buf)?;
        let newlines = buf.iter().filter(|byte| **byte == b'\n').count();
        if newlines > 0 {
            guard.line_count = guard.line_count.saturating_add(newlines);
            if guard.line_count > LOG_MAX_LINES {
                trim_log_file(guard.path.as_path(), LOG_TRIM_TO)?;
                guard.line_count = count_lines(guard.path.as_path()).unwrap_or(LOG_TRIM_TO);
                guard.file = open_log_file(&guard.path)?;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self.inner.lock();
        guard.file.flush()
    }
}

#[derive(Clone)]
struct LogOutputs {
    all: LogWriter,
    info: LogWriter,
    warnings: LogWriter,
    errors: LogWriter,
    issues: LogWriter,
    activities: LogWriter,
    debug: LogWriter,
}

impl LogOutputs {
    fn new(log_dir: &Path) -> io::Result<Self> {
        Ok(Self {
            all: LogWriter::new(log_dir.join(LOG_ALL_FILE))?,
            info: LogWriter::new(log_dir.join(LOG_INFO_FILE))?,
            warnings: LogWriter::new(log_dir.join(LOG_WARN_FILE))?,
            errors: LogWriter::new(log_dir.join(LOG_ERROR_FILE))?,
            issues: LogWriter::new(log_dir.join(LOG_ISSUE_FILE))?,
            activities: LogWriter::new(log_dir.join(LOG_ACTIVITY_FILE))?,
            debug: LogWriter::new(log_dir.join(LOG_DEBUG_FILE))?,
        })
    }

    fn clear_all(&self) -> io::Result<()> {
        self.all.clear()?;
        self.info.clear()?;
        self.warnings.clear()?;
        self.errors.clear()?;
        self.issues.clear()?;
        self.activities.clear()?;
        self.debug.clear()?;
        Ok(())
    }

    fn writer_for(&self, meta: &Metadata<'_>) -> MultiWriter {
        let mut writers = Vec::with_capacity(2);
        writers.push(self.all.clone());
        let target = meta.target();
        if target == "activity" {
            writers.push(self.activities.clone());
            return MultiWriter { writers };
        }
        if target == "issue" {
            writers.push(self.issues.clone());
            return MultiWriter { writers };
        }
        match *meta.level() {
            Level::ERROR => writers.push(self.errors.clone()),
            Level::WARN => writers.push(self.warnings.clone()),
            Level::INFO => writers.push(self.info.clone()),
            Level::DEBUG | Level::TRACE => writers.push(self.debug.clone()),
        }
        MultiWriter { writers }
    }
}

struct MultiWriter {
    writers: Vec<LogWriter>,
}

impl Write for MultiWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for writer in &mut self.writers {
            writer.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        for writer in &mut self.writers {
            writer.flush()?;
        }
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogOutputs {
    type Writer = MultiWriter;

    fn make_writer(&'a self) -> Self::Writer {
        MultiWriter {
            writers: vec![self.all.clone()],
        }
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        self.writer_for(meta)
    }
}

pub fn init_logging(
    config_path: &Path,
    config: &ServerConfig,
) -> Result<(PathBuf, LogControl), String> {
    let log_dir_value = config.log_dir.trim();
    let log_dir_value = if log_dir_value.is_empty() {
        "logs"
    } else {
        log_dir_value
    };
    let log_dir = resolve_path(config_path, log_dir_value);
    if let Err(err) = fs::create_dir_all(&log_dir) {
        return Err(err.to_string());
    }
    let outputs = LogOutputs::new(&log_dir).map_err(|err| err.to_string())?;

    let debug_enabled = config.log_debug_enabled;
    let filter = make_filter(debug_enabled);
    let (filter_layer, handle) = reload::Layer::new(filter);
    let clear_outputs = outputs.clone();

    let file_layer = tracing_subscriber::fmt::layer()
        .event_format(LogFormatter::new())
        .with_writer(outputs.clone())
        .with_ansi(false);

    let subscriber = tracing_subscriber::registry()
        .with(file_layer)
        .with(filter_layer);
    #[cfg(debug_assertions)]
    {
        let console_layer = tracing_subscriber::fmt::layer()
            .event_format(LogFormatter::new())
            .with_ansi(true);
        subscriber.with(console_layer).init();
    }
    #[cfg(not(debug_assertions))]
    {
        subscriber.init();
    }

    Ok((
        log_dir,
        LogControl {
            reload: Arc::new(move |filter| handle.reload(filter).map_err(|err| err.to_string())),
            debug_enabled: Arc::new(AtomicBool::new(debug_enabled)),
            clear_all: Arc::new(move || clear_outputs.clear_all().map_err(|err| err.to_string())),
        },
    ))
}

struct LogFormatter {
    timer: SystemTime,
}

impl LogFormatter {
    fn new() -> Self {
        Self {
            timer: SystemTime::default(),
        }
    }
}

impl<S, N> FormatEvent<S, N> for LogFormatter
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        self.timer.format_time(&mut writer)?;
        write!(writer, " ")?;
        let meta = event.metadata();
        let level = if meta.target() == "activity" {
            "ACTIVITY"
        } else if meta.target() == "issue" {
            "ISSUE"
        } else {
            meta.level().as_str()
        };
        write!(writer, "{:<8} ", level)?;
        if meta.target() != "activity" && meta.target() != "issue" && !meta.target().is_empty() {
            write!(writer, "{}: ", meta.target())?;
        }
        let mut visitor = LogFieldVisitor::new();
        event.record(&mut visitor);
        write!(writer, "{}", visitor.out)?;
        writeln!(writer)
    }
}

struct LogFieldVisitor {
    out: String,
    first: bool,
}

impl LogFieldVisitor {
    fn new() -> Self {
        Self {
            out: String::new(),
            first: true,
        }
    }
}

impl tracing::field::Visit for LogFieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if !self.first {
            self.out.push(' ');
        }
        self.first = false;
        self.out.push_str(field.name());
        self.out.push('=');
        self.out.push_str(&format!("{:?}", value));
    }
}

fn count_lines(path: &Path) -> io::Result<usize> {
    let contents = fs::read_to_string(path)?;
    Ok(contents.lines().count())
}

fn open_log_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .write(true)
        .open(path)
}

fn truncate_log_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    open_log_file(path)
}

fn trim_log_file(path: &Path, keep_lines: usize) -> io::Result<()> {
    let contents = fs::read_to_string(path)?;
    let mut lines: Vec<&str> = contents.lines().collect();
    if lines.len() > keep_lines {
        let start = lines.len().saturating_sub(keep_lines);
        lines = lines.split_off(start);
    }
    let mut trimmed = lines.join("\n");
    if !trimmed.is_empty() {
        trimmed.push('\n');
    }
    fs::write(path, trimmed)
}

fn make_filter(debug_enabled: bool) -> EnvFilter {
    if debug_enabled {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("info")
    }
}
