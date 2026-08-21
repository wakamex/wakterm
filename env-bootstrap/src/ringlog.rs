//! This module sets up a logger that captures recent log entries
//! into an in-memory ring-buffer, as well as passed them on to
//! a pretty logger on stderr.
//! This allows other code to collect the ring buffer and display it
//! within the application.
use chrono::prelude::*;
use env_logger::filter::{Builder as FilterBuilder, Filter};
use log::{Level, LevelFilter, Log, Record};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use termwiz::istty::IsTty;

const LOG_SEGMENT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_RETAINED_LOG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RETAINED_LOG_FILES: usize = 8;
const LOG_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const LOG_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_LOG_MESSAGE_BYTES: usize = 64 * 1024;

lazy_static::lazy_static! {
    static ref RINGS: Mutex<Rings> = Mutex::new(Rings::new());
}

#[derive(Debug, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub struct Entry {
    pub then: DateTime<Local>,
    pub level: Level,
    pub target: String,
    pub msg: String,
}

struct LevelRing {
    entries: Vec<Entry>,
    first: usize,
    last: usize,
}

impl LevelRing {
    fn new(level: Level) -> Self {
        let mut entries = vec![];
        let now = Local::now();
        for _ in 0..16 {
            entries.push(Entry {
                then: now,
                level,
                target: String::new(),
                msg: String::new(),
            });
        }
        Self {
            entries,
            first: 0,
            last: 0,
        }
    }

    // Returns the number of entries in the ring
    fn len(&self) -> usize {
        if self.last >= self.first {
            self.last - self.first
        } else {
            // Wrapped around.
            (self.entries.len() - self.first) + self.last
        }
    }

    fn rolling_inc(&self, value: usize) -> usize {
        let incremented = value + 1;
        if incremented >= self.entries.len() {
            0
        } else {
            incremented
        }
    }

    fn push(&mut self, entry: Entry) {
        if self.len() == self.entries.len() {
            // We are full; effectively pop the first entry to
            // make room
            self.entries[self.first] = entry;
            self.first = self.rolling_inc(self.first);
        } else {
            self.entries[self.last] = entry;
        }
        self.last = self.rolling_inc(self.last);
    }

    fn append_to_vec(&self, target: &mut Vec<Entry>) {
        if self.last >= self.first {
            target.extend_from_slice(&self.entries[self.first..self.last]);
        } else {
            target.extend_from_slice(&self.entries[self.first..]);
            target.extend_from_slice(&self.entries[..self.last]);
        }
    }
}

struct Rings {
    rings: HashMap<Level, LevelRing>,
}

impl Rings {
    fn new() -> Self {
        let mut rings = HashMap::new();
        for level in &[
            Level::Error,
            Level::Warn,
            Level::Info,
            Level::Debug,
            Level::Trace,
        ] {
            rings.insert(*level, LevelRing::new(*level));
        }
        Self { rings }
    }

    fn get_entries(&self) -> Vec<Entry> {
        let mut results = vec![];
        for ring in self.rings.values() {
            ring.append_to_vec(&mut results);
        }
        results
    }

    fn log(&mut self, record: &Record) {
        if let Some(ring) = self.rings.get_mut(&record.level()) {
            ring.push(Entry {
                then: Local::now(),
                level: record.level(),
                target: record.target().to_string(),
                msg: record.args().to_string(),
            });
        }
    }
}

struct OpenLogFile {
    writer: BufWriter<File>,
    bytes_written: u64,
    last_flush: Instant,
}

struct BoundedLogFile {
    file_name: PathBuf,
    rotated_file_name: PathBuf,
    open: Option<OpenLogFile>,
    max_segment_bytes: u64,
    flush_interval: Duration,
}

impl BoundedLogFile {
    fn new(file_name: PathBuf, max_segment_bytes: u64, flush_interval: Duration) -> Self {
        let mut rotated = file_name.as_os_str().to_os_string();
        rotated.push(".1");
        Self {
            file_name,
            rotated_file_name: PathBuf::from(rotated),
            open: None,
            max_segment_bytes: max_segment_bytes.max(1),
            flush_interval,
        }
    }

    fn open(&mut self) -> io::Result<()> {
        if self.open.is_some() {
            return Ok(());
        }
        if let Some(parent) = self.file_name.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.file_name)?;
        let bytes_written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        let now = Instant::now();
        self.open = Some(OpenLogFile {
            writer: BufWriter::new(file),
            bytes_written,
            // Persist an isolated first message immediately. Subsequent DEBUG
            // and TRACE messages are batched for the configured interval.
            last_flush: now.checked_sub(self.flush_interval).unwrap_or(now),
        });
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut open) = self.open.take() {
            open.writer.flush()?;
        }
        let _ = std::fs::remove_file(&self.rotated_file_name);
        if self.file_name.exists()
            && std::fs::rename(&self.file_name, &self.rotated_file_name).is_err()
        {
            // The log is diagnostic data. If a platform cannot rename it after
            // the writer is closed, discard that segment rather than allowing
            // an unbounded append-only file.
            let _ = std::fs::remove_file(&self.file_name);
        }
        self.open()
    }

    fn write_line(&mut self, line: &[u8], level: Level) -> io::Result<()> {
        self.open()?;
        let should_rotate = self.open.as_ref().is_some_and(|open| {
            open.bytes_written > 0
                && open.bytes_written.saturating_add(line.len() as u64) > self.max_segment_bytes
        });
        if should_rotate {
            self.rotate()?;
            if let Some(parent) = self.file_name.parent() {
                prune_log_dir(
                    parent,
                    &[self.file_name.as_path(), self.rotated_file_name.as_path()],
                    MAX_RETAINED_LOG_FILES,
                    MAX_RETAINED_LOG_BYTES,
                    LOG_RETENTION,
                );
            }
        }

        let open = self.open.as_mut().expect("log file should be open");
        open.writer.write_all(line)?;
        open.bytes_written = open.bytes_written.saturating_add(line.len() as u64);
        if matches!(level, Level::Error | Level::Warn | Level::Info)
            || open.last_flush.elapsed() >= self.flush_interval
        {
            open.writer.flush()?;
            open.last_flush = Instant::now();
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(open) = self.open.as_mut() {
            open.writer.flush()?;
            open.last_flush = Instant::now();
        }
        Ok(())
    }
}

struct Logger {
    file: Mutex<BoundedLogFile>,
    filter: Filter,
    padding: AtomicUsize,
    is_tty: bool,
}

impl Drop for Logger {
    fn drop(&mut self) {
        self.flush();
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.filter.enabled(metadata)
    }

    fn flush(&self) {
        let _ = self.file.lock().unwrap().flush();
        let _ = std::io::stderr().flush();
    }

    fn log(&self, record: &Record) {
        if self.filter.matches(record) {
            RINGS.lock().unwrap().log(record);
            let ts = Local::now().format("%H:%M:%S%.3f").to_string();
            let level = record.level().as_str();
            let target = record.target().to_string();
            let msg = truncate_message(record.args().to_string());

            let padding = self.padding.fetch_max(target.len(), Ordering::SeqCst);

            let level_color = if self.is_tty {
                match record.level() {
                    Level::Error => "\u{1b}[31m",
                    Level::Warn => "\u{1b}[33m",
                    Level::Info => "\u{1b}[32m",
                    Level::Debug => "\u{1b}[36m",
                    Level::Trace => "\u{1b}[35m",
                }
            } else {
                ""
            };

            let reset = if self.is_tty { "\u{1b}[0m" } else { "" };
            let target_color = if self.is_tty { "\u{1b}[1m" } else { "" };

            {
                // We use writeln! here rather than eprintln! so that we can ignore
                // a failed log write in the case that stderr has been redirected
                // to a device that is out of disk space.
                // <https://github.com/wakamex/wakterm/issues/1839>
                let mut stderr = std::io::stderr();
                // Direct `write!` will `write()` every single padding space as individual syscall
                // which makes terminal with tracing logs enabled unusably slow.
                let logline = format!(
                    "{}  {level_color}{:6}{reset} {target_color}{:padding$}{reset} > {}\n",
                    ts,
                    level,
                    target,
                    msg,
                    padding = padding,
                    level_color = level_color,
                    reset = reset,
                    target_color = target_color
                );
                let _ = stderr.write_all(logline.as_bytes());
            }

            let logline = format!(
                "{}  {:6} {:padding$} > {}\n",
                ts,
                level,
                target,
                msg,
                padding = padding
            );
            let _ = self
                .file
                .lock()
                .unwrap()
                .write_line(logline.as_bytes(), record.level());
        }
    }
}

fn truncate_message(mut message: String) -> String {
    if message.len() <= MAX_LOG_MESSAGE_BYTES {
        return message;
    }
    const SUFFIX: &str = "... [log message truncated]";
    let mut end = MAX_LOG_MESSAGE_BYTES.saturating_sub(SUFFIX.len());
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str(SUFFIX);
    message
}

/// Returns the current set of log information, sorted by time
pub fn get_entries() -> Vec<Entry> {
    let mut entries = RINGS.lock().unwrap().get_entries();
    entries.sort();
    entries
}

struct RetainedLog {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

fn is_wakterm_log(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("-log-"))
}

fn is_protected(path: &Path, protected: &[&Path]) -> bool {
    protected.contains(&path)
}

fn prune_log_dir(
    dir: &Path,
    protected: &[&Path],
    max_files: usize,
    max_bytes: u64,
    max_age: Duration,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_wakterm_log(&path) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let expired = modified.elapsed().is_ok_and(|elapsed| elapsed > max_age);
        if expired && !is_protected(&path, protected) && std::fs::remove_file(&path).is_ok() {
            continue;
        }
        logs.push(RetainedLog {
            path,
            bytes: metadata.len(),
            modified,
        });
    }

    logs.sort_by_key(|log| log.modified);
    let mut file_count = logs.len();
    let mut total_bytes = logs
        .iter()
        .fold(0u64, |total, log| total.saturating_add(log.bytes));
    for log in logs {
        if file_count <= max_files && total_bytes <= max_bytes {
            break;
        }
        if is_protected(&log.path, protected) {
            continue;
        }
        if std::fs::remove_file(&log.path).is_ok() {
            file_count = file_count.saturating_sub(1);
            total_bytes = total_bytes.saturating_sub(log.bytes);
        }
    }
}

fn setup_pretty() -> (LevelFilter, Logger) {
    let base_name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|| "wakterm".to_string());

    let log_file_name = config::RUNTIME_DIR.join(format!("{}-log-{}.txt", base_name, unsafe {
        libc::getpid()
    }));
    let bounded_log = BoundedLogFile::new(log_file_name, LOG_SEGMENT_BYTES, LOG_FLUSH_INTERVAL);
    prune_log_dir(
        &config::RUNTIME_DIR,
        &[],
        MAX_RETAINED_LOG_FILES,
        MAX_RETAINED_LOG_BYTES,
        LOG_RETENTION,
    );

    let mut filters = FilterBuilder::new();
    for (module, level) in [
        ("wgpu_core", LevelFilter::Error),
        ("wgpu_hal", LevelFilter::Error),
        ("gfx_backend_metal", LevelFilter::Error),
        ("tracing", LevelFilter::Error),
        ("zbus", LevelFilter::Error),
    ] {
        filters.filter_module(module, level);
    }

    if let Ok(s) = std::env::var("WAKTERM_LOG") {
        filters.parse(&s);
    } else {
        filters.filter_level(LevelFilter::Info);
    }
    let filter = filters.build();
    let max_level = filter.filter();

    (
        max_level,
        Logger {
            file: Mutex::new(bounded_log),
            filter,
            padding: AtomicUsize::new(0),
            is_tty: std::io::stderr().is_tty(),
        },
    )
}

pub fn setup_logger() {
    let (max_level, logger) = setup_pretty();
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(max_level);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn bounded_log_rotates_and_keeps_one_previous_segment() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("wakterm-gui-log-1.txt");
        let mut log = BoundedLogFile::new(path.clone(), 64, Duration::ZERO);
        let first = vec![b'a'; 40];
        let second = vec![b'b'; 40];
        let third = vec![b'c'; 40];

        log.write_line(&first, Level::Info).unwrap();
        log.write_line(&second, Level::Info).unwrap();
        log.write_line(&third, Level::Info).unwrap();
        log.flush().unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), third);
        assert_eq!(std::fs::read(path.with_extension("txt.1")).unwrap(), second);
        assert!(!path.with_extension("txt.2").exists());
    }

    #[test]
    fn pruning_bounds_log_count_and_bytes_without_touching_other_files() {
        let temp = tempfile::tempdir().unwrap();
        let protected = temp.path().join("wakterm-gui-log-current.txt");
        for idx in 0..6 {
            let path = if idx == 0 {
                protected.clone()
            } else {
                temp.path().join(format!("wakterm-gui-log-{idx}.txt"))
            };
            let file = File::create(path).unwrap();
            file.set_len(10).unwrap();
        }
        let unrelated = temp.path().join("keep-me.txt");
        File::create(&unrelated).unwrap().set_len(100).unwrap();

        prune_log_dir(
            temp.path(),
            &[protected.as_path()],
            3,
            25,
            Duration::from_secs(u64::MAX),
        );

        let retained = std::fs::read_dir(temp.path())
            .unwrap()
            .flatten()
            .filter(|entry| is_wakterm_log(&entry.path()))
            .collect::<Vec<_>>();
        let retained_bytes = retained
            .iter()
            .fold(0u64, |total, entry| total + entry.metadata().unwrap().len());
        assert!(retained.len() <= 3);
        assert!(retained_bytes <= 25);
        assert!(protected.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn warnings_are_flushed_without_an_explicit_flush_call() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("wakterm-gui-log-1.txt");
        let mut log = BoundedLogFile::new(path.clone(), 1024, Duration::from_secs(60));

        log.write_line(b"context\n", Level::Info).unwrap();
        log.write_line(b"warning\n", Level::Warn).unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "context\nwarning\n");
    }

    #[test]
    fn info_messages_are_flushed_without_an_explicit_flush_call() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("wakterm-gui-log-1.txt");
        let mut log = BoundedLogFile::new(path.clone(), 1024, Duration::from_secs(60));

        log.write_line(b"startup context\n", Level::Info).unwrap();
        log.write_line(b"runtime context\n", Level::Info).unwrap();

        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "startup context\nruntime context\n"
        );
    }

    #[test]
    fn oversized_messages_are_truncated_on_a_character_boundary() {
        let message = "🙂".repeat(MAX_LOG_MESSAGE_BYTES);
        let truncated = truncate_message(message);
        assert!(truncated.len() <= MAX_LOG_MESSAGE_BYTES);
        assert!(truncated.ends_with("... [log message truncated]"));
    }
}
