//! Zero-dependency persistence: a tiny `key=value` file at
//! `%LOCALAPPDATA%\kyouko-player\state.cfg` (fallback `./state.cfg` when the
//! variable is missing, e.g. portable runs).
//!
//! Format — human-editable, forward-compatible:
//!
//! ```text
//! # kyouko-player state — rewritten automatically
//! volume=0.050
//! eq_gains=6.00,0.00,0.00,0.00,0.00,0.00,0.00,0.00,0.00,0.00
//! last_track=C:\My Music\some file.mp3
//! last_folder=C:\My Music
//! ```
//!
//! Rules: unknown keys are ignored on read (so future versions round-trip
//! through older builds harmlessly); a value that fails to parse falls back
//! to its default per-key, never aborting the load. Writes are atomic — a
//! temp file + rename, so a crash mid-write can never corrupt the real file.
//! The instance lock is held for the application lifetime. A dedicated writer
//! coalesces snapshots and parks between saves; its shutdown drains the last one.

use std::fs::{self, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use crate::broker::{EQ_BANDS, EQ_MAX_GAIN_DB};
use crate::log_warn;

/// The state we persist. Everything else in the player is runtime-only.
#[derive(Clone, Debug, PartialEq)]
pub struct PersistedState {
    /// Linear gain, 0.0..=1.0.
    pub volume: f32,
    /// Band gains in dB, 31 Hz → 16 kHz.
    pub eq_gains: [f32; EQ_BANDS],
    /// EQ bypass state; legacy configs default to enabled.
    pub eq_enabled: bool,
    /// Raw last track: a local file path or a YouTube URL.
    pub last_track: Option<String>,
    /// Absolute path of the active local folder, rescanned on startup.
    pub last_folder: Option<String>,
    /// One-based entry within a streaming playlist, paired with last_track.
    pub last_playlist_index: Option<usize>,
    /// Track repeat (mpv `loop-file` style).
    pub loop_enabled: bool,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            volume: 0.8,
            eq_gains: [0.0; EQ_BANDS],
            eq_enabled: true,
            last_track: None,
            last_folder: None,
            last_playlist_index: None,
            loop_enabled: false,
        }
    }
}

/// Where state lives. Computed per call — cheap, and honest if the
/// environment changes under us.
pub fn path() -> PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(dir) => PathBuf::from(dir).join("kyouko-player").join("state.cfg"),
        None => PathBuf::from("state.cfg"),
    }
}

/// An OS file lock, released even after a crash. Never delete the lock file:
/// replacing its inode would let another process lock a different file.
pub struct InstanceGuard {
    _file: fs::File,
}

impl InstanceGuard {
    pub fn acquire(state_path: &Path) -> std::io::Result<Option<Self>> {
        if let Some(parent) = state_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_SHARE_READ | FILE_SHARE_WRITE: another launch may open/try
            // the lock, but nobody may delete or rename its identity while held.
            options.share_mode(1 | 2);
        }
        let file = options.open(state_path.with_file_name("instance.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
}

#[derive(Default)]
struct PendingSave {
    latest: Option<(u64, PersistedState)>,
    requested: u64,
    completed: u64,
    stopping: bool,
    finished: bool,
}

struct WriterExit(Arc<(Mutex<PendingSave>, Condvar)>);
impl Drop for WriterExit {
    fn drop(&mut self) {
        self.0
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .finished = true;
        self.0.1.notify_all(); // wake a final flush even after a worker panic
    }
}

pub struct SaveWriter {
    mailbox: Arc<(Mutex<PendingSave>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl SaveWriter {
    pub fn start(path: PathBuf) -> std::io::Result<Self> {
        Self::with_store(move |snapshot| store_to(&path, &snapshot))
    }

    fn with_store(store: impl Fn(PersistedState) + Send + 'static) -> std::io::Result<Self> {
        let mailbox = Arc::new((Mutex::new(PendingSave::default()), Condvar::new()));
        let pending = mailbox.clone();
        let worker = thread::Builder::new()
            .name("kyouko-state".into())
            .spawn(move || {
                let _exit = WriterExit(pending.clone());
                loop {
                    let (revision, snapshot) = {
                        let state = pending.0.lock().unwrap_or_else(|error| error.into_inner());
                        let mut state = pending
                            .1
                            .wait_while(state, |state| state.latest.is_none() && !state.stopping)
                            .unwrap_or_else(|error| error.into_inner());
                        match state.latest.take() {
                            Some(snapshot) => snapshot,
                            None => break,
                        }
                    };
                    store(snapshot); // no disk I/O under the mailbox lock or on the UI
                    pending
                        .0
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .completed = revision;
                    pending.1.notify_all();
                }
            })?;
        Ok(Self {
            mailbox,
            worker: Some(worker),
        })
    }

    pub fn saver(&self) -> Arc<dyn Fn(PersistedState) + Send + Sync> {
        let mailbox = self.mailbox.clone();
        Arc::new(move |snapshot| {
            let mut pending = mailbox.0.lock().unwrap_or_else(|error| error.into_inner());
            if !pending.stopping && !pending.finished {
                pending.requested += 1;
                pending.latest = Some((pending.requested, snapshot));
                mailbox.1.notify_all();
            }
        })
    }

    /// Shutdown-only barrier. It must also complete inside WM_ENDSESSION's
    /// broker Quit handler, before Windows is allowed to terminate the process.
    pub fn flusher(&self) -> Arc<dyn Fn() + Send + Sync> {
        let mailbox = self.mailbox.clone();
        Arc::new(move || {
            let state = mailbox.0.lock().unwrap_or_else(|error| error.into_inner());
            let target = state.requested;
            let state = mailbox
                .1
                .wait_while(state, |state| state.completed < target && !state.finished)
                .unwrap_or_else(|error| error.into_inner());
            let incomplete = state.completed < target;
            drop(state);
            if incomplete {
                log_warn!(
                    "CFG",
                    "state writer stopped before the final snapshot completed"
                );
            }
        })
    }
}

impl Drop for SaveWriter {
    fn drop(&mut self) {
        self.mailbox
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stopping = true;
        self.mailbox.1.notify_all();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            log_warn!("CFG", "state writer panicked during shutdown");
        }
    }
}

/// Read the persisted state. A missing file is a first run: defaults.
pub fn load() -> PersistedState {
    load_from(&path())
}

/// Persist the state. Failures are logged, never fatal.
pub fn store(state: &PersistedState) {
    store_to(&path(), state);
}

pub fn load_from(path: &Path) -> PersistedState {
    match fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(_) => PersistedState::default(),
    }
}

pub fn store_to(path: &Path, state: &PersistedState) {
    if let Err(error) = save_atomic(path, state) {
        log_warn!("CFG", "save failed: {error}");
    }
}

fn save_atomic(path: &Path, state: &PersistedState) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut eq = String::with_capacity(EQ_BANDS * 7);
    for (i, gain) in state.eq_gains.iter().enumerate() {
        if i > 0 {
            eq.push(',');
        }
        eq.push_str(&format!("{gain:.2}"));
    }
    let body = format!(
        "# kyouko-player state — rewritten automatically, safe to edit\nvolume={:.3}\neq_gains={eq}\neq_enabled={}\nlast_track={}\nlast_folder={}\nlast_playlist_index={}\nloop={}\n",
        state.volume,
        state.eq_enabled,
        state.last_track.as_deref().unwrap_or(""),
        state.last_folder.as_deref().unwrap_or(""),
        state
            .last_playlist_index
            .map(|index| index.to_string())
            .unwrap_or_default(),
        if state.loop_enabled { "true" } else { "false" },
    );
    let staged = stage_file(path, body.as_bytes())?;
    fs::rename(&staged.path, path)?;
    // On Unix, persist the directory entry as well as the file contents.
    #[cfg(unix)]
    fs::File::open(
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()?;
    Ok(())
}

struct StagedFile {
    path: PathBuf,
}
impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn stage_file(path: &Path, bytes: &[u8]) -> std::io::Result<StagedFile> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    // create_new cannot truncate a stale or concurrently staged snapshot.
    loop {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(
            ".{}-{}.tmp",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let tmp = path.with_file_name(name);
        let mut output = match OpenOptions::new().create_new(true).write(true).open(&tmp) {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let staged = StagedFile { path: tmp };
        output.write_all(bytes)?;
        output.sync_all()?;
        drop(output); // Windows rename must not retain an open staging handle
        return Ok(staged);
    }
}

fn parse(text: &str) -> PersistedState {
    let mut state = PersistedState::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "volume" => {
                if let Ok(v) = value.parse::<f32>()
                    && v.is_finite()
                {
                    state.volume = v.clamp(0.0, 1.0);
                }
            }
            "eq_gains" => {
                for (band, part) in value.split(',').enumerate() {
                    if band >= EQ_BANDS {
                        break;
                    }
                    if let Ok(g) = part.trim().parse::<f32>()
                        && g.is_finite()
                    {
                        state.eq_gains[band] = g.clamp(-EQ_MAX_GAIN_DB, EQ_MAX_GAIN_DB);
                    }
                }
            }
            "eq_enabled" => {
                if value.eq_ignore_ascii_case("true") {
                    state.eq_enabled = true;
                } else if value.eq_ignore_ascii_case("false") {
                    state.eq_enabled = false;
                }
            }
            "last_track" => {
                state.last_track = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                };
            }
            "last_folder" => {
                state.last_folder = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                };
            }
            "last_playlist_index" => {
                state.last_playlist_index = value.parse::<usize>().ok().filter(|&index| index > 0);
            }
            "loop" => {
                state.loop_enabled = value.eq_ignore_ascii_case("true");
            }
            _ => {} // unknown key — ignore, forward compatibility
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_waits_for_the_final_snapshot_without_blocking_normal_broker_controls() {
        use crate::broker::{Broker, Command, DecoderLink, Flow, SharedState};
        use crossbeam_channel::bounded;
        let path = temp_path("quit-flush");
        let target = path.clone();
        let (started_tx, started_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let writer = SaveWriter::with_store(move |snapshot| {
            if snapshot.volume == 0.0 {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            save_atomic(&target, &snapshot).unwrap();
        })
        .unwrap();
        let (tx, rx) = crate::decoder_wake::command_channel(8);
        let mut broker = Broker::new(
            SharedState::new(),
            DecoderLink::with_interrupt(tx, rx, Arc::new(|| {})),
        )
        .with_saver(writer.saver())
        .with_flush(writer.flusher());
        broker.handle_command(Command::SetVolume(0.0));
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        broker.handle_command(Command::SetVolume(0.73));
        let (quit_tx, quit_rx) = bounded(1);
        let quit = thread::spawn(move || {
            quit_tx
                .send(broker.handle_command(Command::Quit) == Flow::Exit)
                .unwrap();
        });
        let premature = quit_rx.recv_timeout(std::time::Duration::from_millis(30));
        release_tx.send(()).unwrap();
        let result = if let Ok(result) = premature {
            Ok(result)
        } else {
            quit_rx.recv_timeout(std::time::Duration::from_secs(2))
        };
        quit.join().unwrap();
        drop(writer);
        assert!(
            premature.is_err(),
            "Quit must wait for the committed final snapshot"
        );
        assert!(result.unwrap());
        assert_eq!(load_from(&path).volume, 0.73);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_panicked_writer_unblocks_the_shutdown_barrier() {
        let writer = SaveWriter::with_store(|_| panic!("test-only writer fault")).unwrap();
        writer.saver()(PersistedState::default());
        let flush = writer.flusher();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let waiter = thread::spawn(move || {
            flush();
            tx.send(()).unwrap();
        });
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        waiter.join().unwrap();
        drop(writer);
    }

    #[test]
    fn save_writer_coalesces_pending_updates_and_flushes_latest_on_drop() {
        use crossbeam_channel::bounded;
        let path = temp_path("async-flush");
        let target = path.clone();
        let (started_tx, started_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let (written_tx, written_rx) = bounded(3);
        let writer = SaveWriter::with_store(move |snapshot| {
            if snapshot.volume == 0.0 {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap(); // slow disk, outside the mailbox lock
            }
            save_atomic(&target, &snapshot).unwrap();
            written_tx.send(snapshot.volume).unwrap();
        })
        .unwrap();
        let saver = writer.saver();
        saver(PersistedState {
            volume: 0.0,
            ..Default::default()
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        for step in 1..=99 {
            saver(PersistedState {
                volume: step as f32 / 100.0,
                ..Default::default()
            });
        }
        release_tx.send(()).unwrap();
        drop(writer); // joins the first write and the last pending snapshot
        assert_eq!(written_rx.try_iter().collect::<Vec<_>>(), [0.0, 0.99]);
        assert_eq!(load_from(&path).volume, 0.99);
        saver(PersistedState::default()); // a retained callback cannot reopen a stopped writer
        assert_eq!(load_from(&path).volume, 0.99);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn atomic_snapshots_use_distinct_files_and_failed_commit_preserves_previous_state() {
        let dir =
            std::env::temp_dir().join(format!("kyouko-cfg-{}-failed-commit", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.cfg");
        let original = PersistedState {
            volume: 0.37,
            last_folder: Some("C:\\Music".into()),
            ..Default::default()
        };
        save_atomic(&path, &original).unwrap();
        let a = stage_file(&path, b"partial snapshot A").unwrap();
        let b = stage_file(&path, b"partial snapshot B").unwrap();
        assert_ne!(a.path, b.path);
        assert_eq!(load_from(&path), original);
        drop(a);
        drop(b);
        let blocked = dir.join("not-a-file.cfg");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("sentinel"), b"keep").unwrap();
        assert!(save_atomic(&blocked, &PersistedState::default()).is_err());
        assert_eq!(fs::read(blocked.join("sentinel")).unwrap(), b"keep");
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            2,
            "failed save must remove its staging file"
        );
        assert_eq!(load_from(&path), original);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "subprocess fixture, invoked only by the instance/crash tests"]
    fn instance_lock_fixture() {
        let path = PathBuf::from(std::env::var_os("KYOUKO_INSTANCE_TEST_STATE").unwrap());
        let _guard = InstanceGuard::acquire(&path)
            .unwrap()
            .expect("fixture owns the state directory");
        let _staged = if std::env::var("KYOUKO_INSTANCE_TEST_MODE").unwrap() == "interrupt" {
            Some(stage_file(&path, b"# incomplete new snapshot\nvolume=").unwrap())
        } else {
            None
        };
        println!("INSTANCE_HELD");
        std::io::stdout().flush().unwrap();
        loop {
            thread::park();
        } // killed by the parent; no UI or polling
    }

    fn kill_held_fixture(path: &Path, mode: &str) {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "config::tests::instance_lock_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("KYOUKO_INSTANCE_TEST_STATE", path)
            .env("KYOUKO_INSTANCE_TEST_MODE", mode)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut owner = crate::process::SourceChild::track(command.spawn().unwrap()).unwrap();
        let stdout = owner.stdout.take().unwrap();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line.is_ok_and(|line| line == "INSTANCE_HELD") {
                    let _ = tx.send(());
                    return;
                }
            }
        });
        let ready = rx.recv_timeout(std::time::Duration::from_secs(5));
        let duplicate = InstanceGuard::acquire(path);
        let _ = owner.kill();
        owner.wait().unwrap();
        reader.join().unwrap(); // clean up the fixture before asserting
        ready.unwrap();
        assert!(
            duplicate.unwrap().is_none(),
            "another process must not own the same state directory"
        );
        assert!(
            InstanceGuard::acquire(path).unwrap().is_some(),
            "OS releases the lock after forced process termination"
        );
    }

    #[test]
    #[cfg(windows)]
    #[ignore = "requires KYOUKO_INSTANCE_VERIFY_EXE pointing to the release binary"]
    fn duplicate_release_launch_exits_without_touching_state_or_log() {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        use std::process::{Command, Stdio};
        let dir = std::env::temp_dir().join(format!(
            "kyouko-cfg-{}-release-instance",
            std::process::id()
        ));
        let data = dir.join("kyouko-player");
        fs::create_dir_all(&data).unwrap();
        let path = data.join("state.cfg");
        let expected = PersistedState {
            volume: 0.43,
            last_track: Some("old.mp4".into()),
            ..Default::default()
        };
        save_atomic(&path, &expected).unwrap();
        let original = fs::read(&path).unwrap();
        let log = data.join("player.log");
        fs::write(&log, b"existing instance log\n").unwrap();
        let instance = InstanceGuard::acquire(&path).unwrap().unwrap();
        let exe = std::env::var_os("KYOUKO_INSTANCE_VERIFY_EXE").unwrap();
        let mut command = Command::new(exe);
        command
            .env("LOCALAPPDATA", &dir)
            .arg("new.mp4")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut duplicate = crate::process::SourceChild::track(command.spawn().unwrap()).unwrap();
        let process = duplicate.as_handle().try_clone_to_owned().unwrap();
        let deadline =
            crate::deadline::Deadline::start(std::time::Duration::from_secs(5), move || unsafe {
                let _ = windows::Win32::System::Threading::TerminateProcess(
                    windows::Win32::Foundation::HANDLE(process.as_raw_handle()),
                    1,
                );
            })
            .unwrap();
        let status = duplicate.wait().unwrap();
        drop(duplicate);
        drop(deadline);
        drop(instance);
        let after = fs::read(&path).unwrap();
        let log_after = fs::read(&log).unwrap();
        fs::remove_dir_all(dir).unwrap();
        assert!(
            status.success(),
            "second launch must exit promptly before device/window initialization"
        );
        assert_eq!(after, original);
        assert_eq!(log_after, b"existing instance log\n");
    }

    #[test]
    #[cfg(windows)]
    fn active_instance_lock_cannot_be_deleted_or_renamed() {
        let dir =
            std::env::temp_dir().join(format!("kyouko-cfg-{}-lock-identity", std::process::id()));
        let guard = InstanceGuard::acquire(&dir.join("state.cfg"))
            .unwrap()
            .unwrap();
        let path = dir.join("instance.lock");
        let removed = fs::remove_file(&path);
        let renamed = fs::rename(&path, dir.join("different.lock"));
        drop(guard);
        fs::remove_dir_all(dir).unwrap();
        assert!(
            removed.is_err(),
            "deletion must not permit a new lock inode"
        );
        assert!(renamed.is_err(), "rename must not permit another instance");
    }

    #[test]
    fn instance_lock_excludes_other_processes_and_is_released_after_a_crash() {
        let dir = std::env::temp_dir().join(format!("kyouko-cfg-{}-instance", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        kill_held_fixture(&dir.join("state.cfg"), "hold");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interrupted_staged_save_retains_valid_state_and_restart_can_save_again() {
        let dir =
            std::env::temp_dir().join(format!("kyouko-cfg-{}-interrupted", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.cfg");
        let original = PersistedState {
            last_track: Some("old.mp4".into()),
            volume: 0.37,
            ..Default::default()
        };
        save_atomic(&path, &original).unwrap();
        kill_held_fixture(&path, "interrupt");
        assert_eq!(load_from(&path), original);
        let restarted = PersistedState {
            last_track: Some("new.mp4".into()),
            volume: 0.75,
            ..Default::default()
        };
        save_atomic(&path, &restarted).unwrap();
        assert_eq!(load_from(&path), restarted);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nonfinite_numbers_fall_back_per_key_without_poisoning_other_settings() {
        for volume in ["NaN", "inf", "-inf"] {
            let state = parse(&format!(
                "volume={volume}\neq_gains=NaN,inf,-inf,3.5\neq_enabled=false\nloop=true\n"
            ));
            assert_eq!(state.volume, PersistedState::default().volume);
            assert_eq!(&state.eq_gains[..4], &[0.0, 0.0, 0.0, 3.5]);
            assert!(!state.eq_enabled);
            assert!(state.loop_enabled);
        }
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kyouko-cfg-test-{}-{tag}.cfg", std::process::id()))
    }

    #[test]
    fn round_trip_preserves_values_and_spaces() {
        let path = temp_path("roundtrip");
        let state = PersistedState {
            volume: 0.05,
            eq_gains: [6.0, 0.0, 0.0, 0.0, 0.0, 0.0, -1.5, 0.0, 8.0, 0.0],
            eq_enabled: false,
            last_track: Some(r"C:\My Music\kyouko test file.mp3".into()),
            last_folder: Some(r"C:\My Music".into()),
            last_playlist_index: None,
            loop_enabled: true,
        };
        store_to(&path, &state);
        let loaded = load_from(&path);
        assert_eq!(loaded, state);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn folder_state_round_trip_and_legacy_defaults() {
        let path = temp_path("folder");
        let state = PersistedState {
            last_folder: Some(r"C:\音楽\Album = Live".into()),
            ..Default::default()
        };
        store_to(&path, &state);
        assert_eq!(load_from(&path), state);
        store_to(&path, &PersistedState::default());
        assert_eq!(load_from(&path).last_folder, None);
        fs::write(&path, "last_track=C:\\Music\\old.mp3\nvolume=0.5\n").unwrap();
        let legacy = load_from(&path);
        assert_eq!(legacy.last_folder, None);
        assert_eq!(legacy.last_track.as_deref(), Some(r"C:\Music\old.mp3"));
        assert_eq!(legacy.volume, 0.5);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn playlist_selection_round_trip_and_invalid_defaults() {
        let path = temp_path("playlist-index");
        let state = PersistedState {
            last_track: Some("https://youtube.com/playlist?list=test".into()),
            last_playlist_index: Some(317),
            ..Default::default()
        };
        store_to(&path, &state);
        assert_eq!(load_from(&path), state);
        for value in ["", "0", "-1", "invalid"] {
            assert_eq!(
                parse(&format!("last_playlist_index={value}\n")).last_playlist_index,
                None
            );
        }
        assert_eq!(parse("last_track=legacy.mp3\n").last_playlist_index, None);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn garbage_file_degrades_to_defaults_per_key() {
        let path = temp_path("garbage");
        fs::write(
            &path,
            "volume=notanumber\neq_gains=x,y,z\nrandom=1\nlast_track=\n",
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded, PersistedState::default());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn values_are_clamped_on_load() {
        let path = temp_path("clamp");
        fs::write(&path, "volume=2.5\neq_gains=99,-99,0,0,0,0,0,0,0,0\n").unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.volume, 1.0);
        assert_eq!(loaded.eq_gains[0], EQ_MAX_GAIN_DB);
        assert_eq!(loaded.eq_gains[1], -EQ_MAX_GAIN_DB);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn loop_flag_round_trip_and_garbage() {
        let path = temp_path("loop");
        store_to(
            &path,
            &PersistedState {
                loop_enabled: true,
                ..Default::default()
            },
        );
        assert!(load_from(&path).loop_enabled);
        store_to(
            &path,
            &PersistedState {
                loop_enabled: false,
                ..Default::default()
            },
        );
        assert!(!load_from(&path).loop_enabled);
        fs::write(
            &path,
            "loop=banana
",
        )
        .unwrap();
        assert!(!load_from(&path).loop_enabled); // strict parse → default
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn partial_eq_gains_fill_flat() {
        let path = temp_path("partial");
        fs::write(&path, "eq_gains=3.5\n").unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.eq_gains[0], 3.5);
        assert!(loaded.eq_gains[1..].iter().all(|g| *g == 0.0));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn eq_enabled_round_trip_and_legacy_or_invalid_defaults() {
        let path = temp_path("eq-enabled");
        for enabled in [false, true] {
            let state = PersistedState {
                eq_enabled: enabled,
                ..Default::default()
            };
            store_to(&path, &state);
            assert_eq!(load_from(&path), state);
        }
        for text in ["volume=0.5\n", "eq_enabled=\n", "eq_enabled=banana\n"] {
            assert!(
                parse(text).eq_enabled,
                "legacy/invalid state defaults to EQ ON"
            );
        }
        assert!(!parse("eq_enabled=FALSE\n").eq_enabled);
        assert!(parse("eq_enabled=TRUE\n").eq_enabled);
        fs::remove_file(path).unwrap();
    }
}
