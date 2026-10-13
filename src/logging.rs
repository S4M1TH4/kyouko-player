//! Structured debug-console logging and bounded asynchronous release-file logging.
//!
//! `tracing` was rejected on purpose: it drags in a subscriber ecosystem for
//! needs a music player does not have. Level filter: `KYOUKO_LOG=debug|info|warn|error`
//! (default `info`).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

pub const LVL_ERROR: u8 = 0;
pub const LVL_WARN: u8 = 1;
pub const LVL_INFO: u8 = 2;
pub const LVL_DEBUG: u8 = 3;

static START: OnceLock<Instant> = OnceLock::new();
static MAX: AtomicU8 = AtomicU8::new(LVL_INFO);

pub struct LogGuard {
    #[cfg(all(windows, not(debug_assertions)))]
    _writer: Option<file::Guard>,
}

#[cfg(all(windows, not(debug_assertions)))]
static FILE_LOG: OnceLock<file::Sink> = OnceLock::new();

pub fn init() -> LogGuard {
    START.set(Instant::now()).ok();
    let lvl = match std::env::var("KYOUKO_LOG").as_deref() {
        Ok("error") => LVL_ERROR,
        Ok("warn") => LVL_WARN,
        Ok("debug") => LVL_DEBUG,
        _ => LVL_INFO,
    };
    MAX.store(lvl, Ordering::Relaxed);
    #[cfg(all(windows, not(debug_assertions)))]
    {
        match file::start(crate::config::path().with_file_name("player.log"), 1024 * 1024, 256) {
            Ok((sink, writer)) => { let _ = FILE_LOG.set(sink); LogGuard { _writer: Some(writer) } }
            Err(error) => { eprintln!("cannot open player log: {error}"); LogGuard { _writer: None } }
        }
    }
    #[cfg(not(all(windows, not(debug_assertions))))]
    { LogGuard {} }
}

pub fn emit(level: u8, tag: &str, message: std::fmt::Arguments<'_>) {
    #[cfg(all(windows, not(debug_assertions)))]
    if let Some(sink) = FILE_LOG.get() { sink.publish(level, tag, message); }
    #[cfg(not(all(windows, not(debug_assertions))))]
    {
        println!("[{} {} {:>7}] {}", stamp(), level_name(level), tag, message);
    }
}

#[inline]
pub fn enabled(lvl: u8) -> bool {
    lvl <= MAX.load(Ordering::Relaxed)
}

/// Monotonic seconds since startup — enough to reason about event timing.
pub fn stamp() -> String {
    let t = START.get().map(Instant::elapsed).unwrap_or_default();
    format!("{:7.3}s", t.as_secs_f32())
}

pub fn level_name(lvl: u8) -> &'static str {
    match lvl {
        LVL_ERROR => "ERROR",
        LVL_WARN => "WARN ",
        LVL_INFO => "INFO ",
        _ => "DEBUG",
    }
}

#[macro_export]
macro_rules! log_at {
    ($lvl:expr, $tag:expr, $($arg:tt)+) => {
        if $crate::logging::enabled($lvl) {
            $crate::logging::emit($lvl, $tag, format_args!($($arg)+));
        }
    };
}

#[macro_export]
macro_rules! log_error {
    ($tag:expr, $($arg:tt)+) => { $crate::log_at!($crate::logging::LVL_ERROR, $tag, $($arg)+) };
}

#[macro_export]
macro_rules! log_warn {
    ($tag:expr, $($arg:tt)+) => { $crate::log_at!($crate::logging::LVL_WARN, $tag, $($arg)+) };
}

#[macro_export]
macro_rules! log_info {
    ($tag:expr, $($arg:tt)+) => { $crate::log_at!($crate::logging::LVL_INFO, $tag, $($arg)+) };
}

#[macro_export]
macro_rules! log_debug {
    ($tag:expr, $($arg:tt)+) => { $crate::log_at!($crate::logging::LVL_DEBUG, $tag, $($arg)+) };
}

#[cfg(any(test, all(windows, not(debug_assertions))))]
mod file {
    use crossbeam_channel::{Sender, bounded};
    use std::fmt::{self, Write as _};
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread::{self, JoinHandle};

    const MAX_RECORD: usize = 4096;
    enum Record { Line(String), Shutdown }
    pub struct Sink { tx: Sender<Record>, dropped: Arc<AtomicU64> }
    pub struct Guard { tx: Sender<Record>, worker: Option<JoinHandle<()>> }

    struct Line(String);
    impl fmt::Write for Line {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            let mut count = text.len().min(MAX_RECORD - 1 - self.0.len());
            while !text.is_char_boundary(count) { count -= 1; }
            self.0.push_str(&text[..count]);
            if count < text.len() { Err(fmt::Error) } else { Ok(()) }
        }
    }

    impl Sink {
        pub fn publish(&self, level: u8, tag: &str, message: fmt::Arguments<'_>) {
            let dropped = self.dropped.swap(0, Ordering::Relaxed);
            let mut line = Line(String::with_capacity(MAX_RECORD));
            if dropped > 0 { let _ = write!(line, "[LOGGER] dropped {dropped} records; "); }
            let _ = write!(line, "[{} {} {:>7}] {}", super::stamp(), super::level_name(level), tag, message);
            line.0.push('\n');
            if self.tx.try_send(Record::Line(line.0)).is_err() { self.dropped.fetch_add(dropped + 1, Ordering::Relaxed); }
        }
    }

    fn open(path: &PathBuf) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Permit log readers/rotation, but prevent concurrent file writers.
            options.share_mode(1 | 4);
        }
        options.open(path)
    }

    pub fn start(path: PathBuf, limit: u64, capacity: usize) -> io::Result<(Sink, Guard)> {
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) { std::fs::create_dir_all(parent)?; }
        let mut output = open(&path)?;
        let mut size = output.metadata()?.len();
        if size > limit { output.set_len(0)?; size = 0; }
        let (tx, rx) = bounded(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let final_drops = dropped.clone();
        let worker = thread::Builder::new().name("kyouko-log".into()).spawn(move || {
            let result = (|| -> io::Result<()> {
                while let Ok(record) = rx.recv() {
                    let shutting_down = matches!(record, Record::Shutdown);
                    let line = match record {
                        Record::Line(line) => line,
                        Record::Shutdown => {
                            let count = final_drops.swap(0, Ordering::Relaxed);
                            if count == 0 { break; }
                            format!("[LOGGER] dropped {count} records before shutdown\n")
                        }
                    };
                    // One current and one previous file, each at most limit.
                    let mut end = line.len().min(limit as usize);
                    while !line.is_char_boundary(end) { end -= 1; }
                    let line = &line.as_bytes()[..end];
                    if size + line.len() as u64 > limit {
                        output.flush()?;
                        let previous = path.with_extension("previous.log");
                        match std::fs::remove_file(&previous) { Ok(()) => {}, Err(error) if error.kind() == io::ErrorKind::NotFound => {}, Err(error) => return Err(error) }
                        std::fs::rename(&path, &previous)?;
                        output = open(&path)?;
                        size = 0;
                    }
                    output.write_all(line)?;
                    size += line.len() as u64;
                    output.flush()?;
                    // Shutdown with dropped records is the last queued message.
                    if shutting_down { break; }
                }
                output.sync_all()
            })();
            if let Err(error) = result { eprintln!("player log writer stopped: {error}"); }
        })?;
        Ok((Sink { tx: tx.clone(), dropped }, Guard { tx, worker: Some(worker) }))
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.tx.send(Record::Shutdown); // only shutdown may wait for disk
            if let Some(worker) = self.worker.take() { let _ = worker.join(); }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn records_are_utf8_bounded_and_full_queue_never_blocks() {
            let (tx, rx) = bounded(1);
            let sink = Sink { tx, dropped: Arc::new(AtomicU64::new(0)) };
            sink.publish(2, "TEST", format_args!("{}", "日本語".repeat(2000)));
            sink.publish(2, "TEST", format_args!("queue full"));
            assert_eq!(sink.dropped.load(Ordering::Relaxed), 1);
            let Record::Line(line) = rx.recv().unwrap() else { panic!("missing line") };
            assert!(line.len() <= MAX_RECORD);
            assert!(line.ends_with('\n'));
            sink.publish(2, "TEST", format_args!("recovered"));
            let Record::Line(line) = rx.recv().unwrap() else { panic!("missing line") };
            assert!(line.contains("dropped 1 records"));
        }

        #[test]
        fn shutdown_flushes_and_rotation_bounds_both_files() {
            let dir = std::env::temp_dir().join(format!("kyouko-log-{}-rotation", std::process::id()));
            let path = dir.join("player.log");
            let (sink, guard) = start(path.clone(), 256, 128).unwrap();
            for index in 0..50 { sink.publish(2, "TEST", format_args!("record {index}: {}", "x".repeat(70))); }
            drop(guard);
            assert!(std::fs::metadata(&path).unwrap().len() <= 256);
            assert!(std::fs::metadata(path.with_extension("previous.log")).unwrap().len() <= 256);
            assert!(std::fs::read_to_string(&path).unwrap().contains("record 49"));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

#[cfg(all(test, windows, not(debug_assertions)))]
#[test]
fn release_logger_flushes_on_shutdown() {
    let guard = init();
    let marker = format!("release logging verified, process {}", std::process::id());
    emit(LVL_ERROR, "SELFTEST", format_args!("{marker}"));
    drop(guard);
    let path = crate::config::path().with_file_name("player.log");
    assert!(std::fs::read_to_string(path).unwrap().contains(&marker));
}
