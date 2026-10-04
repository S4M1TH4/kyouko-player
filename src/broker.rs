//! The pure core: message vocabulary, lock-free shared state, and the broker
//! state machine. No platform code, no I/O — this module is the reason a
//! future Linux port is a presentation-layer change only.
//!
//! Threading contract:
//! * `Broker` lives on the main thread only (single-owner state machine).
//! * `SharedState` is the only cross-thread data: independent atomics, each an
//!   isolated scalar, `Relaxed` unless a flag carries a payload (then
//!   Release/Acquire). No locks, no false-sharing-critical writes.
//! * Every transition goes through the broker and gets logged.

use std::fmt;
use std::path::Path;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering,
};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;

use crate::config::PersistedState;
use crate::{log_debug, log_error, log_info, log_warn};

// ── Tuning constants (consumed by the audio module in step 3) ────────────────

/// Decode granularity: ~85 ms at 48 kHz stereo. Small enough that skip/stop
/// feel instant, large enough that per-chunk overhead vanishes.
#[allow(dead_code)] // consumed in step 3
pub const CHUNK_FRAMES: usize = 4096;

/// ~1.4 s of buffered audio. Backpressure mechanism: when full, the decoder
/// parks in `select!` until either the callback drains space or a command
/// arrives. That parking is the *only* waiting the decoder ever does.
#[allow(dead_code)] // consumed in step 3
pub const CHUNK_CHANNEL_DEPTH: usize = 16;

pub const EQ_BANDS: usize = 10;
pub const EQ_BAND_HZ: [u32; EQ_BANDS] =
    [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
pub const EQ_BAND_LABELS: [&str; EQ_BANDS] =
    ["31", "62", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"];
pub const EQ_MAX_GAIN_DB: f32 = 12.0;

/// Extensions a folder scan queues. Deliberately matches the symphonia
/// feature set this binary was built with (.opus waits for symphonia 0.6).
const MEDIA_EXTENSIONS: [&str; 10] =
    ["mp3", "flac", "wav", "m4a", "aac", "mp4", "mkv", "webm", "ogg", "aiff"];

// ── Message vocabulary ───────────────────────────────────────────────────────

/// Something to play. One enum so the decoder owns *all* source handling.
#[derive(Clone, Debug)]
pub enum Source {
    /// Path as typed on the terminal (wide-char conversion happens at open).
    File(String),
    /// `format` is a yt-dlp format id; default "140" = m4a 128 kbps.
    /// `playlist_index: Some(n)` marks a playlist-shaped URL playing entry n
    /// — at EOF the broker advances to n+1 (see Finished handling).
    Youtube { url: String, format: String, playlist_index: Option<usize> },
}

impl Source {
    /// Short name for the panel: file stem-ish or the URL.
    pub fn display_name(&self) -> &str {
        match self {
            Source::File(p) => p.rsplit(['/', '\\']).next().unwrap_or(p),
            Source::Youtube { url, .. } => url,
        }
    }

    /// Raw string form (path or URL) — exactly what persistence stores and
    /// what `from_raw` turns back into a `Source`.
    pub fn raw(&self) -> &str {
        match self {
            Source::File(p) => p,
            Source::Youtube { url, .. } => url,
        }
    }

    /// Inverse of `raw`: URL-shaped strings stream via yt-dlp, everything
    /// else opens as a local file. Playlist-shaped URLs (a `list=` parameter
    /// without an explicit video id) become advancing sources at entry 1.
    pub fn from_raw(raw: &str) -> Source {
        if raw.starts_with("http://") || raw.starts_with("https://") {
            Source::Youtube {
                url: raw.to_string(),
                format: "140".to_string(),
                playlist_index: playlist_index_of(raw),
            }
        } else {
            Source::File(raw.to_string())
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::File(p) => write!(f, "file({p})"),
            Source::Youtube { url, format, .. } => {
                write!(f, "youtube({url} [fmt {format}])")
            }
        }
    }
}

/// A URL is playlist-shaped when it carries a `list=` parameter but no
/// explicit video id (`watch?v=` / `youtu.be/`) — the latter stays a single
/// video even if it happens to sit inside a playlist.
pub fn playlist_index_of(url: &str) -> Option<usize> {
    let has_list = url.contains("list=");
    let is_video = url.contains("watch?v=") || url.contains("youtu.be/");
    (has_list && !is_video).then_some(1)
}

/// The terminal queue listing: 1-based numbering with an arrow marker on
/// the currently selected track. Pure so the format is unit-testable; the
/// broker's ShowQueue arm prints what this returns.
fn format_queue(queue: &[Source], current: Option<usize>) -> String {
    if queue.is_empty() {
        return "no local queue loaded (load a folder with `p <folder>`)".to_string();
    }
    let mut out = format!("local queue ({} tracks):
", queue.len());
    for (i, source) in queue.iter().enumerate() {
        let marker = if current == Some(i) { "->" } else { "  " };
        out.push_str(&format!("{} [{:>2}] {}
", marker, i + 1, source.display_name()));
    }
    out
}

/// Shallow scan of one folder: files with a recognized media extension,
/// sorted newest-first (files without a mtime sort last). Single-level by
/// design — no recursion, so the one-off scan cannot block the broker.
fn scan_folder(dir: &str) -> Vec<Source> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(Option<std::time::SystemTime>, Source)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                return None;
            }
            let ext = path.extension()?.to_str()?.to_ascii_lowercase();
            if !MEDIA_EXTENSIONS.contains(&ext.as_str()) {
                return None;
            }
            let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
            Some((modified, Source::File(path.to_string_lossy().into_owned())))
        })
        .collect();
    // Newest first; Option ordering puts undated entries at the end.
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().map(|(_, source)| source).collect()
}

/// Next source in a playlist run: only playlist-shaped YouTube sources
/// advance (index n -> n+1). Everything else has no "next".
fn advance_playlist(source: &Source) -> Option<Source> {
    match source {
        Source::Youtube { url, format, playlist_index: Some(n) } => Some(Source::Youtube {
            url: url.clone(),
            format: format.clone(),
            playlist_index: Some(n + 1),
        }),
        _ => None,
    }
}

/// Input side: terminal now, tray menu / global hotkeys later. All of them
/// converge here — the broker is the single decision point.
#[derive(Clone, Debug)]
pub enum Command {
    /// `paused: true` stages the source in the decoder (buffered, resume is
    /// instant) while the output stays stopped — used for last-track restore.
    Load { source: Source, paused: bool },
    /// Flip between Playing and Paused (tray menu + terminal `r`).
    TogglePause,
    /// Jump the playhead by a signed offset in seconds (terminal `.` / `,`).
    SeekRelative(f64),
    /// Next / previous track: playlist sources step their entry index
    /// (clamped at the start); everything else restarts from 00:00.
    NextTrack,
    PrevTrack,
    /// Print the local folder queue to the terminal.
    ShowQueue,
    /// Jump straight to a queue track. Carries the user's 1-BASED number —
    /// the broker converts and validates it against the queue bounds.
    JumpToTrack(usize),
    Stop,
    /// 0.0..=1.0 linear gain, applied in the decoder (callback stays a memcpy).
    /// Terminal `vol`, tray vol row (+/-10% steps) all land here.
    SetVolume(f32),
    /// Band gain in dB, clamped to ±12. `None` = all bands.
    EqGain { band: Option<usize>, gain_db: f32 },
    EqEnabled(bool),
    /// Flip EQ bypass (off = biquads skipped, saved band gains untouched).
    ToggleEq,
    /// Toggle track repeat (mpv `loop-file`): Finished replays the source.
    ToggleLoop,
    /// Show/hide the echo window (handled by the presentation layer).
    ToggleWindow,
    DumpState,
    Quit,
}

/// Broker → decoder. Kept deliberately tiny: the decoder is a state machine
/// parked on this channel, not a service with a rich API.
#[derive(Debug)]
pub enum DecoderCmd {
    Load(Source),
    /// Jump the playhead by a signed offset in seconds. Executed on the
    /// decoder thread; ignored while no track is open.
    SeekRelative(f64),
    /// Drop the current source and go back to park. Wakes the decoder even
    /// when it is parked waiting for chunk-channel space (select! on both).
    Stop,
    /// Same as Stop, then exit the thread. Kills any yt-dlp child first.
    Shutdown,
}

/// Decoder → broker. Rare events; the bounded channel cannot deadlock because
/// the broker selects on both inputs every iteration.
#[allow(dead_code)] // Opened/Finished are constructed by the audio module in step 3
#[derive(Debug)]
pub enum Status {
    Opened {
        source: Source,
        sample_rate: u32,
        channels: u16,
        /// Streams may not know their name; files usually do.
        title: Option<String>,
        duration: Option<Duration>,
    },
    /// The playhead moved (seek); the presentation layer refreshes its view.
    /// The new position is already in SharedState.
    Seeked,
    /// Natural end of stream (not Stop).
    Finished,
    Failed { source: Source, reason: String },
}

/// Playback phase. Stored as `u8` in `SharedState`.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Stopped = 0,
    Loading = 1,
    Playing = 2,
    Paused = 3,
}

impl Phase {
    pub fn from_u8(v: u8) -> Option<Phase> {
        match v {
            0 => Some(Phase::Stopped),
            1 => Some(Phase::Loading),
            2 => Some(Phase::Playing),
            3 => Some(Phase::Paused),
            _ => None,
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Phase::Stopped => "STOPPED",
            Phase::Loading => "LOADING",
            Phase::Playing => "PLAYING",
            Phase::Paused => "PAUSED ",
        };
        f.write_str(s)
    }
}

// ── Cross-thread shared state ────────────────────────────────────────────────

/// The only data shared across threads. Every field is an independent scalar;
/// the audio callback reads volume/eq per chunk, writes `frames_played`, and
/// nothing here ever needs a lock or a CAS.
pub struct SharedState {
    phase: AtomicU8,
    /// Volume as raw f32 bits (lock-free 32-bit store).
    volume_bits: AtomicU32,
    /// Written by the WASAPI callback, read by the broker's 1 Hz timer.
    frames_played: AtomicU64,
    /// The OUTPUT device's rate — makes `frames_played` meaningful as time.
    /// Set once when the stream opens; chunks are already resampled to it.
    sample_rate: AtomicU32,
    /// The output device's channel count, for decoder-side layout mixing.
    device_channels: AtomicU32,
    /// Track repeat (mpv `loop-file` style): Finished replays the same source.
    loop_enabled: AtomicBool,
    /// Bumped on every Load. Chunks from a dead generation are discarded.
    generation: AtomicU64,
    eq_enabled: AtomicBool,
    /// Set (Release) when a gain changes; decoder rebuilds biquad coefficients
    /// once (Acquire), then clears it. Coeff math only ever runs on change.
    eq_dirty: AtomicBool,
    /// Band gains in centi-dB: integer, lock-free, 0.1 dB resolution.
    eq_gains_cdb: [AtomicI32; EQ_BANDS],
}

impl SharedState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            phase: AtomicU8::new(Phase::Stopped as u8),
            volume_bits: AtomicU32::new(0.8f32.to_bits()),
            frames_played: AtomicU64::new(0),
            sample_rate: AtomicU32::new(0),
            device_channels: AtomicU32::new(0),
            loop_enabled: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            eq_enabled: AtomicBool::new(true),
            eq_dirty: AtomicBool::new(true),
            eq_gains_cdb: std::array::from_fn(|_| AtomicI32::new(0)),
        })
    }

    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Relaxed)).unwrap_or(Phase::Stopped)
    }
    pub fn set_phase(&self, p: Phase) {
        self.phase.store(p as u8, Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }
    pub fn set_volume(&self, v: f32) {
        self.volume_bits
            .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn eq_enabled(&self) -> bool {
        self.eq_enabled.load(Ordering::Relaxed)
    }
    pub fn eq_gain_db(&self, band: usize) -> f32 {
        self.eq_gains_cdb[band].load(Ordering::Relaxed) as f32 / 100.0
    }
    /// Returns the clamped value actually stored.
    pub fn set_eq_gain(&self, band: usize, gain_db: f32) -> f32 {
        let clamped = gain_db.clamp(-EQ_MAX_GAIN_DB, EQ_MAX_GAIN_DB);
        self.eq_gains_cdb[band]
            .store((clamped * 100.0).round() as i32, Ordering::Relaxed);
        self.eq_dirty.store(true, Ordering::Release);
        clamped
    }
    pub fn set_eq_enabled(&self, on: bool) {
        self.eq_enabled.store(on, Ordering::Relaxed);
        self.eq_dirty.store(true, Ordering::Release);
    }

    pub fn position(&self) -> Duration {
        let rate = self.sample_rate.load(Ordering::Relaxed);
        let frames = self.frames_played.load(Ordering::Relaxed);
        if rate > 0 {
            Duration::from_secs_f64(frames as f64 / rate as f64)
        } else {
            Duration::ZERO
        }
    }
    pub fn reset_playhead(&self) {
        self.frames_played.store(0, Ordering::Relaxed);
    }
    /// Called by the output callback only (realtime context): playhead advance.
    pub fn add_frames_played(&self, frames: u64) {
        self.frames_played.fetch_add(frames, Ordering::Relaxed);
    }
    /// Overwrite the playhead (device-rate frames) — used by seeks.
    pub fn set_playhead_frames(&self, frames: u64) {
        self.frames_played.store(frames, Ordering::Relaxed);
    }
    /// Consumes the dirty flag (decoder rebuilds biquad coefficients when set).
    pub fn take_eq_dirty(&self) -> bool {
        self.eq_dirty.swap(false, Ordering::Acquire)
    }
    /// Device rate — set once by the output stream, read by the decoder as
    /// its resampling target and by the broker for time math.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Relaxed)
    }
    pub fn set_sample_rate(&self, rate: u32) {
        self.sample_rate.store(rate, Ordering::Relaxed);
    }
    pub fn device_channels(&self) -> u32 {
        self.device_channels.load(Ordering::Relaxed)
    }
    pub fn set_device_channels(&self, n: u32) {
        self.device_channels.store(n, Ordering::Relaxed);
    }
    pub fn loop_enabled(&self) -> bool {
        self.loop_enabled.load(Ordering::Relaxed)
    }
    pub fn set_loop_enabled(&self, on: bool) {
        self.loop_enabled.store(on, Ordering::Relaxed);
    }
    /// Snapshot of all band gains, for the decoder's coefficient rebuild.
    pub fn eq_gains(&self) -> [f32; EQ_BANDS] {
        let mut out = [0.0; EQ_BANDS];
        for (i, g) in self.eq_gains_cdb.iter().enumerate() {
            out[i] = g.load(Ordering::Relaxed) as f32 / 100.0;
        }
        out
    }
    pub fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

// ── Broker ───────────────────────────────────────────────────────────────────

/// What the current track looks like to the main thread (main-thread owned,
/// never shared — that is why it can be plain data).
pub struct TrackMeta {
    pub source: Source,
    pub title: Option<String>,
    pub duration: Option<Duration>,
    #[allow(dead_code)] // read by the stream (re)configuration in step 3
    pub sample_rate: u32,
    #[allow(dead_code)] // read by the stream (re)configuration in step 3
    pub channels: u16,
}

#[derive(PartialEq)]
pub enum Flow {
    Continue,
    Exit,
}

/// Broker → decoder link: the command channel plus an optional kill switch.
/// Before every Stop/Load/Shutdown, `interrupt` tears down any yt-dlp child
/// the decoder is reading from — otherwise a network-stalled pipe read would
/// defer the command indefinitely. This stays audio-agnostic: the audio
/// module installs the closure, the broker just calls it.
#[derive(Clone)]
pub struct DecoderLink {
    tx: Sender<DecoderCmd>,
    interrupt: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl DecoderLink {
    pub fn with_interrupt(tx: Sender<DecoderCmd>, interrupt: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { tx, interrupt: Some(interrupt) }
    }

    pub fn send(&self, cmd: DecoderCmd) {
        if matches!(cmd, DecoderCmd::Stop | DecoderCmd::Shutdown | DecoderCmd::Load(_)) {
            if let Some(f) = &self.interrupt {
                f();
            }
        }
        if let Err(e) = self.tx.send(cmd) {
            log_error!("BROKER", "decoder channel dead: {e}");
        }
    }
}

/// The decision point. Owns the decoder link; everything else is derived
/// state. The presentation layer (headless loop, Win32 pump) calls
/// `handle_command` / `handle_status` and renders `view`.
pub struct Broker {
    shared: Arc<SharedState>,
    decoder: DecoderLink,
    track: Option<TrackMeta>,
    view: String,
    view_dirty: bool,
    /// Load asked for a staged (paused) start: the next Opened flips to
    /// Paused instead of Playing. Consumed there, cleared on failure.
    stage_paused: bool,
    /// Raw form of the most recently OPENED source — survives Stop/Finished
    /// (where `track` is cleared) so a volume tweak never erases last_track.
    last_source_raw: Option<String>,
    /// The source a LOADING load will open (no track exists yet) — lets
    /// navigation work while metadata/probe is still in flight.
    pending_source: Option<Source>,
    /// Local folder queue (shallow scan, newest first) + position. A single
    /// file load becomes a one-entry queue; a YouTube load clears it.
    local_queue: Vec<Source>,
    queue_index: Option<usize>,

    /// Installed by main; writes PersistedState to disk. A closure keeps the
    /// broker I/O-free (and testable).
    saver: Option<Arc<dyn Fn(PersistedState) + Send + Sync>>,
}

impl Broker {
    pub fn new(shared: Arc<SharedState>, decoder: DecoderLink) -> Self {
        let mut broker = Self {
            shared,
            decoder,
            track: None,
            view: String::new(),
            view_dirty: false,
            stage_paused: false,
            last_source_raw: None,
            pending_source: None,
            local_queue: Vec::new(),
            queue_index: None,
            saver: None,
        };
        broker.refresh();
        broker
    }

    /// Install the persistence sink. Fired on the save triggers: volume,
    /// EQ gains, opened track, Quit.
    pub fn with_saver(mut self, saver: Arc<dyn Fn(PersistedState) + Send + Sync>) -> Self {
        self.saver = Some(saver);
        self
    }

    /// Current persistent-worthy state.
    pub fn persist_snapshot(&self) -> PersistedState {
        PersistedState {
            volume: self.shared.volume(),
            eq_gains: self.shared.eq_gains(),
            last_track: self.last_source_raw.clone(),
            loop_enabled: self.shared.loop_enabled(),
        }
    }

    fn persist(&self) {
        if let Some(saver) = &self.saver {
            saver(self.persist_snapshot());
            log_debug!("BROKER", "state persisted to disk");
        }
    }

    /// The common load tail - everything a Load does once queue management
    /// has settled which source to open.
    fn begin_load(&mut self, source: Source, paused: bool) {
        log_info!(
            "BROKER",
            "load{}: {source}",
            if paused { " [staged paused]" } else { "" }
        );
        self.stage_paused = paused;
        self.track = None;
        self.pending_source = Some(source.clone());
        self.shared.reset_playhead();
        self.shared.bump_generation();
        self.set_phase(Phase::Loading, "load requested");
        self.send_decoder(DecoderCmd::Load(source));
    }

    /// Load a track straight from the local folder queue WITHOUT re-running
    /// queue management (which would clobber the folder queue on every
    /// auto-advance). Used by navigation and EOF advancement.
    fn load_queue_track(&mut self, index: usize) {
        self.queue_index = Some(index);
        let source = self.local_queue[index].clone();
        self.begin_load(source, false);
    }

    /// Track navigation: playlist-shaped YouTube sources step their entry
    /// index (previous clamps at entry 1, where the reload acts as a restart
    /// from 00:00); sources without a playlist (or a queue — none exists)
    /// simply restart the current track. Every navigation is a normal Load,
    /// so the single-process lifecycle and orphan-proofing apply untouched.
    fn navigate(&mut self, dir: i32) {
        // Local folder queue first: it navigates purely by index, even while
        // stopped or while a load is still in flight.
        if !self.local_queue.is_empty() {
            let idx = self.queue_index.unwrap_or(0);
            let last = self.local_queue.len() - 1;
            let next_i = (idx as i32 + dir).clamp(0, last as i32) as usize;
            if dir > 0 && next_i == idx && idx == last {
                log_info!("BROKER", "navigate: end of folder queue — stopping");
                self.handle_command(Command::Stop);
                return;
            }
            log_info!(
                "BROKER",
                "navigate {}: folder track {} -> {} (of {})",
                if dir > 0 { "next" } else { "prev" },
                idx,
                next_i,
                last
            );
            self.load_queue_track(next_i);
            return;
        }
        let current = self
            .track
            .as_ref()
            .map(|t| t.source.clone())
            .or_else(|| self.pending_source.clone());
        let Some(current) = current else {
            log_warn!("BROKER", "navigate ignored — nothing loaded");
            return;
        };
        // YouTube playlist: step the entry index.
        if let Source::Youtube { url, format, playlist_index: Some(n) } = &current {
            let next = if dir > 0 { n + 1 } else { (*n).saturating_sub(1).max(1) };
            log_info!(
                "BROKER",
                "navigate {}: playlist entry {} -> {}",
                if dir > 0 { "next" } else { "prev" },
                n,
                next
            );
            let target = Source::Youtube {
                url: url.clone(),
                format: format.clone(),
                playlist_index: Some(next),
            };
            self.handle_command(Command::Load { source: target, paused: false });
            return;
        }
        // Local folder queue: step the file index; Next past the end stops
        // gracefully, Prev at the start restarts the first file.
        if !self.local_queue.is_empty() {
            let idx = self.queue_index.unwrap_or(0);
            let last = self.local_queue.len() - 1;
            let next_i = (idx as i32 + dir).clamp(0, last as i32) as usize;
            if dir > 0 && next_i == idx && idx == last {
                log_info!("BROKER", "navigate: end of folder queue — stopping");
                self.handle_command(Command::Stop);
                return;
            }
            log_info!(
                "BROKER",
                "navigate {}: folder track {} -> {} (of {})",
                if dir > 0 { "next" } else { "prev" },
                idx,
                next_i,
                last
            );
            self.load_queue_track(next_i);
            return;
        }
        // No playlist, no queue: restart the current track.
        log_info!("BROKER", "navigate: no queue — restarting current track");
        self.handle_command(Command::Load { source: current, paused: false });
    }

    /// Panel changed since last `take_refresh`? (Presentation pulls; the
    /// broker never draws.)
    pub fn take_refresh(&mut self) -> Option<String> {
        if self.view_dirty {
            self.view_dirty = false;
            Some(self.view.clone())
        } else {
            None
        }
    }

    /// Handle to the cross-thread state. The Win32 layer uses it to watch the
    /// phase for timer management without touching broker internals.
    pub fn shared(&self) -> Arc<SharedState> {
        Arc::clone(&self.shared)
    }

    /// 1 Hz playback tick, delivered by the presentation timer (which only
    /// exists while `Playing`). Advances the elapsed-time display.
    pub fn tick(&mut self) {
        if self.shared.phase() == Phase::Playing {
            self.refresh();
        }
    }

    /// Current panel text — read by the Win32 renderer in step 2 (e.g. when
    /// it needs to repaint from scratch after `TaskbarCreated` or a resize).
    #[allow(dead_code)] // consumed in step 2
    pub fn view(&self) -> &str {
        &self.view
    }

    pub fn handle_command(&mut self, cmd: Command) -> Flow {
        log_debug!("BROKER", "command: {cmd:?}");
        match cmd {
            Command::Load { source, paused } => {
                // Folder expansion: `p <dir>` queues the folder's media files
                // (shallow scan, newest first) and plays the first one. A
                // folder with no supported media leaves the player untouched.
                let mut source = source;
                let mut queue_managed = false;
                if let Source::File(path) = &source {
                    if Path::new(path).is_dir() {
                        let files = scan_folder(path);
                        if files.is_empty() {
                            log_error!(
                                "BROKER",
                                "folder {path} contains no supported media files — playback state unchanged"
                            );
                            self.refresh();
                            return Flow::Continue;
                        }
                        log_info!("BROKER", "folder queue: {} tracks from {path}", files.len());
                        self.local_queue = files.clone();
                        self.queue_index = Some(0);
                        source = files[0].clone();
                        queue_managed = true;
                    }
                }
                if !queue_managed {
                    // A direct file load becomes a one-entry queue; a YouTube
                    // load supersedes any local queue.
                    match &source {
                        Source::File(_) => {
                            self.local_queue = vec![source.clone()];
                            self.queue_index = Some(0);
                        }
                        Source::Youtube { .. } => {
                            self.local_queue.clear();
                            self.queue_index = None;
                        }
                    }
                }
                log_info!(
                    "BROKER",
                    "load{}: {source}",
                    if paused { " [staged paused]" } else { "" }
                );
                self.stage_paused = paused;
                self.track = None;
                self.pending_source = Some(source.clone());
                self.shared.reset_playhead();
                self.shared.bump_generation();
                self.set_phase(Phase::Loading, "load requested");
                self.send_decoder(DecoderCmd::Load(source));
            }
            Command::SeekRelative(delta) => {
                log_info!("BROKER", "seek {delta:+.1}s");
                self.send_decoder(DecoderCmd::SeekRelative(delta));
            }
            Command::NextTrack => self.navigate(1),
            Command::PrevTrack => self.navigate(-1),
            Command::ShowQueue => {
                // The queue is broker-owned: the listing prints from this
                // event loop (the spec's single-owner requirement).
                let current = self.queue_index;
                println!("{}", format_queue(&self.local_queue, current));
            }
            Command::JumpToTrack(num) => {
                if self.local_queue.is_empty() {
                    log_warn!("BROKER", "jump ignored — no local queue loaded");
                } else {
                    // 1-based user number -> 0-based index, bounds-checked.
                    match num.checked_sub(1).filter(|idx| *idx < self.local_queue.len()) {
                        Some(idx) => {
                            log_info!("BROKER", "jump: track {num} of {}", self.local_queue.len());
                            self.load_queue_track(idx);
                        }
                        None => {
                            log_warn!(
                                "BROKER",
                                "jump: track {num} out of range (1-{})",
                                self.local_queue.len()
                            );
                        }
                    }
                }
            }
            Command::TogglePause => match self.shared.phase() {
                Phase::Playing => self.set_phase(Phase::Paused, "toggle"),
                Phase::Paused => self.set_phase(Phase::Playing, "toggle"),
                p => log_warn!("BROKER", "toggle ignored while {p}"),
            },
            Command::Stop => {
                log_info!("BROKER", "stop");
                self.send_decoder(DecoderCmd::Stop);
                self.pending_source = None;
                self.track = None;
                self.shared.reset_playhead();
                self.set_phase(Phase::Stopped, "stop");
            }
            Command::SetVolume(v) => {
                let v = v.clamp(0.0, 1.0);
                self.shared.set_volume(v);
                log_info!("BROKER", "volume: {:.0}%", v * 100.0);
                self.persist();
            }
            Command::EqGain { band, gain_db } => {
                match band {
                    Some(b) if b < EQ_BANDS => {
                        let stored = self.shared.set_eq_gain(b, gain_db);
                        log_info!(
                            "BROKER",
                            "eq[{}Hz]: {gain_db:+.1} -> {stored:+.1} dB",
                            EQ_BAND_HZ[b]
                        );
                    }
                    Some(b) => log_warn!("BROKER", "eq band {b} out of range (0..={})", EQ_BANDS - 1),
                    None => {
                        let stored = self.shared.set_eq_gain(0, gain_db);
                        for b in 1..EQ_BANDS {
                            self.shared.set_eq_gain(b, gain_db);
                        }
                        log_info!("BROKER", "eq[all]: {gain_db:+.1} -> {stored:+.1} dB");
                    }
                }
                self.persist();
            }
            Command::EqEnabled(on) => {
                self.shared.set_eq_enabled(on);
                log_info!("BROKER", "eq: {}", if on { "ON" } else { "OFF" });
            }
            // Bypass only: eq_enabled gates the biquad pass in the decoder;
            // the saved band gains are untouched, so nothing to persist.
            Command::ToggleEq => {
                let on = !self.shared.eq_enabled();
                self.shared.set_eq_enabled(on);
                log_info!("BROKER", "eq: {}", if on { "ON" } else { "OFF" });
            }
            Command::ToggleLoop => {
                let on = !self.shared.loop_enabled();
                self.shared.set_loop_enabled(on);
                log_info!("LOOP", "repeat: {}", if on { "ON" } else { "OFF" });
                self.persist();
            }
            // Presentation-side command: the Win32 drain intercepts this and
            // toggles the window; it never reaches here in normal flow.
            Command::ToggleWindow => {
                log_debug!("BROKER", "window toggle is presentation-side");
            }
            Command::DumpState => {
                let s = &self.shared;
                log_info!(
                    "BROKER",
                    "state dump: phase={} generation={} rate={} frames={} pos={} vol={:.2} eq_on={} loop={} gains_cdb={:?}",
                    s.phase(),
                    s.generation(),
                    s.sample_rate.load(Ordering::Relaxed),
                    s.frames_played.load(Ordering::Relaxed),
                    fmt_mmss(s.position()),
                    s.volume(),
                    s.eq_enabled(),
                    s.loop_enabled(),
                    s.eq_gains_cdb.each_ref().map(|g| g.load(Ordering::Relaxed)),
                );
                self.refresh(); // re-print the panel with the dump
            }
            Command::Quit => {
                log_info!("BROKER", "quit — shutting decoder down (kills yt-dlp child)");
                self.persist(); // final flush; Ctrl+C / tray / terminal all land here
                self.send_decoder(DecoderCmd::Shutdown);
                self.set_phase(Phase::Stopped, "quit");
                return Flow::Exit;
            }
        }
        self.refresh();
        Flow::Continue
    }

    pub fn handle_status(&mut self, st: Status) {
        log_debug!("BROKER", "status: {st:?}");
        match st {
            Status::Opened { source, sample_rate, channels, title, duration } => {
                log_info!(
                    "BROKER",
                    "opened: {source} — {} Hz, {channels} ch{}",
                    sample_rate,
                    duration
                        .map(|d| format!(", {}", fmt_mmss(d)))
                        .unwrap_or_default()
                );
                self.last_source_raw = Some(source.raw().to_string());
                self.pending_source = None; // the track now carries it
                self.track = Some(TrackMeta { source, title, duration, sample_rate, channels });
                self.shared.reset_playhead();
                // A staged load resolves to Paused instead of Playing: the
                // track is buffered and resume is instant, but the output
                // stays stopped — 0% CPU until the user asks for sound.
                let target = if self.stage_paused {
                    self.stage_paused = false;
                    Phase::Paused
                } else {
                    Phase::Playing
                };
                self.set_phase(
                    target,
                    if target == Phase::Paused {
                        "staged — resume to play"
                    } else {
                        "stream opened"
                    },
                );
                self.persist();
            }
            Status::Finished => {
                // 1. mpv loop-file: replay the exact source (preserving a
                //    custom YouTube format id when the track carries one).
                let replay = self.shared.loop_enabled().then(|| {
                    self.track
                        .as_ref()
                        .map(|t| t.source.clone())
                        .or_else(|| self.last_source_raw.as_deref().map(Source::from_raw))
                });
                if let Some(Some(source)) = replay {
                    log_info!("LOOP", "repeat: replaying {source}");
                    self.handle_command(Command::Load { source, paused: false });
                    return; // handle_command already refreshed the panel
                }
                // 2. Playlist progression: a finished playlist entry advances
                //    to the next index via a fresh (sequential, single-process)
                //    load — title prefetch included.
                if let Some(source) =
                    self.track.as_ref().map(|t| t.source.clone()).and_then(|src| advance_playlist(&src))
                {
                    log_info!("BROKER", "playlist: advancing to entry {:?}", source);
                    self.handle_command(Command::Load { source, paused: false });
                    return;
                }
                // Local folder queue: play the next file, if any.
                let idx = self.queue_index.unwrap_or(0);
                if idx + 1 < self.local_queue.len() {
                    log_info!(
                        "BROKER",
                        "folder queue: advancing to {}/{}",
                        idx + 2,
                        self.local_queue.len()
                    );
                    self.load_queue_track(idx + 1);
                    return;
                }
                log_info!("BROKER", "track finished");
                self.track = None;
                self.shared.reset_playhead();
                self.set_phase(Phase::Stopped, "track finished");
            }
            Status::Seeked => {
                // The decoder already moved SharedState's playhead; this only
                // repaints the panel (matters while paused — the 1 Hz timer
                // is not running).
                self.refresh();
            }
            Status::Failed { source, reason } => {
                log_error!("BROKER", "failed to open {source}: {reason}");
                self.stage_paused = false;
                self.pending_source = None;
                self.track = None;
                self.shared.reset_playhead();
                self.set_phase(Phase::Stopped, "load failed");
            }
        }
        self.refresh();
    }

    fn set_phase(&mut self, to: Phase, why: &str) {
        let from = self.shared.phase();
        if from == to {
            return;
        }
        self.shared.set_phase(to);
        log_info!("BROKER", "phase: {from} -> {to} ({why})");
    }

    fn send_decoder(&mut self, cmd: DecoderCmd) {
        self.decoder.send(cmd);
    }

    fn refresh(&mut self) {
        self.view = render_panel(&self.shared, self.track.as_ref());
        self.view_dirty = true;
    }
}

// ── Panel: the exact text the 200 px window will draw (ASCII only) ──────────

fn fmt_mmss(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// Borderless: no frame, no gutters — just the text, truncated at 28
/// columns so the layered window keeps a stable width.
fn row(content: &str) -> String {
    content.chars().take(28).collect()
}

fn band_row(labels: &[&str]) -> String {
    labels.iter().map(|l| format!("{l:>4}")).collect()
}

fn gain_row(shared: &SharedState, start: usize) -> String {
    (start..start + 5)
        .map(|b| format!("{:+4.0}", shared.eq_gain_db(b)))
        .collect()
}

/// Borderless panel — plain text lines only (the frame was removed by
/// design); the layered window's own edges provide the boundary.
pub fn render_panel(shared: &SharedState, track: Option<&TrackMeta>) -> String {
    let timeline = match track.and_then(|t| t.duration) {
        Some(d) => format!("{} / {}", fmt_mmss(shared.position()), fmt_mmss(d)),
        None => format!("{} / --:--", fmt_mmss(shared.position())),
    };
    let title = match track {
        Some(t) => t.title.clone().unwrap_or_else(|| t.source.display_name().to_string()),
        None => "(none)".into(),
    };
    let v = shared.volume();
    let cells = ((v * 10.0).round() as usize).min(10);
    let bar = format!("{}{}", "|".repeat(cells), "-".repeat(10 - cells));

    let mut p = String::with_capacity(400);
    p.push_str(&row(&format!("KYOUKO    {}", shared.phase())));
    p.push('\n');
    p.push_str(&row(&format!("time : {timeline}")));
    p.push('\n');
    p.push_str(&row(&format!("track: {title}")));
    p.push('\n');
    p.push_str(&row(&format!("vol  : {bar} {:>3}%", (v * 100.0).round() as u32)));
    p.push('\n');
    p.push_str(&row(&format!(
        "eq   : {}",
        if shared.eq_enabled() { "ON" } else { "OFF" }
    )));
    p.push('\n');
    p.push_str(&row(&band_row(&EQ_BAND_LABELS[0..5])));
    p.push('\n');
    p.push_str(&row(&gain_row(shared, 0)));
    p.push('\n');
    p.push_str(&row(&band_row(&EQ_BAND_LABELS[5..])));
    p.push('\n');
    p.push_str(&row(&gain_row(shared, 5)));
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A broker wired to an in-memory sink — no disk I/O in tests. Returns
    /// the decoder command receiver so tests can observe what the broker
    /// asked the decoder to do.
    fn broker_with_sink() -> (
        Broker,
        Arc<Mutex<Vec<PersistedState>>>,
        crossbeam_channel::Receiver<DecoderCmd>,
    ) {
        let shared = SharedState::new();
        let (tx, rx) = crossbeam_channel::bounded::<DecoderCmd>(8);
        let sink: Arc<Mutex<Vec<PersistedState>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_saver = Arc::clone(&sink);
        let broker = Broker::new(shared, DecoderLink::with_interrupt(
            tx,
            Arc::new(|| {}),
        ))
        .with_saver(Arc::new(move |st| {
            sink_for_saver.lock().unwrap().push(st);
        }));
        (broker, sink, rx)
    }

    fn open_youtube(broker: &mut Broker) {
        broker.handle_status(Status::Opened {
            source: Source::Youtube {
                url: "https://youtu.be/x".into(),
                format: "140".into(),
                playlist_index: None,
            },
            sample_rate: 44_100,
            channels: 2,
            title: None,
            duration: None,
        });
    }

    #[test]
    fn staged_load_opens_paused_and_clears_on_failure() {
        let (mut broker, _, _rx) = broker_with_sink();
        broker.handle_command(Command::Load {
            source: Source::File("nowhere.mp3".into()),
            paused: true,
        });
        // Staged flag is set by Load...
        assert!(broker.stage_paused);
        // ...and a FAILED open must not leave it armed for a future track.
        broker.handle_status(Status::Failed {
            source: Source::File("nowhere.mp3".into()),
            reason: "test".into(),
        });
        assert!(!broker.stage_paused);
        assert_eq!(broker.shared.phase(), Phase::Stopped);
    }

    #[test]
    fn opened_consumes_stage_flag_once() {
        let (mut broker, _, _rx) = broker_with_sink();
        broker.handle_command(Command::Load {
            source: Source::File("a.mp3".into()),
            paused: true,
        });
        broker.handle_status(Status::Opened {
            source: Source::File("a.mp3".into()),
            sample_rate: 48_000,
            channels: 2,
            title: None,
            duration: None,
        });
        assert_eq!(broker.shared.phase(), Phase::Paused);
        assert!(!broker.stage_paused);
        // A follow-up normal load must play, not inherit the staged pause.
        broker.handle_command(Command::Load {
            source: Source::File("b.mp3".into()),
            paused: false,
        });
        broker.handle_status(Status::Opened {
            source: Source::File("b.mp3".into()),
            sample_rate: 48_000,
            channels: 2,
            title: None,
            duration: None,
        });
        assert_eq!(broker.shared.phase(), Phase::Playing);
    }

    #[test]
    fn save_triggers_fire_and_snapshot_matches() {
        let (mut broker, sink, _rx) = broker_with_sink();
        broker.handle_command(Command::SetVolume(0.25));
        broker.handle_command(Command::EqGain { band: Some(2), gain_db: 4.0 });
        broker.handle_status(Status::Opened {
            source: Source::Youtube {
                url: "https://youtu.be/x".into(),
                format: "140".into(),
                playlist_index: None,
            },
            sample_rate: 44_100,
            channels: 2,
            title: None,
            duration: None,
        });
        broker.handle_command(Command::SetVolume(0.5));
        let snaps = sink.lock().unwrap();
        // Volume, EqGain, Opened, Volume → 4 saves.
        assert_eq!(snaps.len(), 4);
        let last = snaps.last().unwrap();
        assert_eq!(last.volume, 0.5);
        assert_eq!(last.eq_gains[2], 4.0);
        assert_eq!(last.last_track.as_deref(), Some("https://youtu.be/x"));
    }

    #[test]
    fn loop_on_replays_track_at_eof() {
        let (mut broker, sink, rx) = broker_with_sink();
        broker.handle_command(Command::ToggleLoop);
        assert!(broker.shared.loop_enabled());
        open_youtube(&mut broker);
        assert_eq!(broker.shared.phase(), Phase::Playing);
        broker.handle_status(Status::Finished);
        // Replayed: a fresh autoplay Load went to the decoder, phase is
        // LOADING (never dipped through Stopped), and the toggle itself
        // plus the Opened persist produced exactly two sink writes.
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Load(Source::Youtube { .. }))));
        assert_eq!(broker.shared.phase(), Phase::Loading);
        assert_eq!(sink.lock().unwrap().len(), 2);
        assert!(sink.lock().unwrap().last().unwrap().loop_enabled);
    }

    #[test]
    fn loop_off_stops_at_eof() {
        let (mut broker, sink, rx) = broker_with_sink();
        open_youtube(&mut broker);
        broker.handle_status(Status::Finished);
        assert_eq!(broker.shared.phase(), Phase::Stopped);
        assert!(rx.try_recv().is_err(), "no replay command without loop");
        assert_eq!(sink.lock().unwrap().len(), 1); // only the Opened save
    }

    #[test]
    fn loop_replay_prefers_custom_format_id() {
        let (mut broker, _, rx) = broker_with_sink();
        broker.handle_command(Command::ToggleLoop);
        broker.handle_status(Status::Opened {
            source: Source::Youtube {
                url: "https://youtu.be/x".into(),
                format: "251".into(),
                playlist_index: None,
            },
            sample_rate: 48_000,
            channels: 2,
            title: None,
            duration: None,
        });
        broker.handle_status(Status::Finished);
        match rx.try_recv() {
            Ok(DecoderCmd::Load(Source::Youtube { format, .. })) => {
                assert_eq!(format, "251", "replay must keep the track's format id");
            }
            other => panic!("expected replay load, got {other:?}"),
        }
    }

    #[test]
    fn toggle_pause_resumes_and_pauses() {
        let (mut broker, _, _rx) = broker_with_sink();
        broker.handle_command(Command::Load {
            source: Source::File("a.mp3".into()),
            paused: true,
        });
        broker.handle_status(Status::Opened {
            source: Source::File("a.mp3".into()),
            sample_rate: 48_000,
            channels: 2,
            title: None,
            duration: None,
        });
        assert_eq!(broker.shared.phase(), Phase::Paused);
        broker.handle_command(Command::TogglePause);
        assert_eq!(broker.shared.phase(), Phase::Playing);
        broker.handle_command(Command::TogglePause);
        assert_eq!(broker.shared.phase(), Phase::Paused);
    }

    #[test]
    fn toggle_eq_bypasses_without_touching_gains_or_persistence() {
        let (mut broker, sink, _rx) = broker_with_sink();
        broker.handle_command(Command::EqGain { band: Some(0), gain_db: 6.0 });
        let saves = sink.lock().unwrap().len();
        broker.handle_command(Command::ToggleEq);
        assert!(!broker.shared.eq_enabled(), "first toggle bypasses EQ");
        assert_eq!(broker.shared.eq_gains()[0], 6.0, "bypass must not touch gains");
        broker.handle_command(Command::ToggleEq);
        assert!(broker.shared.eq_enabled());
        assert_eq!(
            sink.lock().unwrap().len(),
            saves,
            "eq bypass is runtime-only: no save trigger"
        );
    }

    #[test]
    fn finished_playlist_entry_advances_to_next() {
        let (mut broker, _, rx) = broker_with_sink();
        let pl = |n: usize| Source::Youtube {
            url: "https://youtube.com/playlist?list=PLtest".into(),
            format: "140".into(),
            playlist_index: Some(n),
        };
        broker.handle_command(Command::Load { source: pl(1), paused: false });
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Load(Source::Youtube { .. }))));
        broker.handle_status(Status::Opened {
            source: pl(1),
            sample_rate: 44_100,
            channels: 2,
            title: Some("Entry One".into()),
            duration: None,
        });
        assert_eq!(broker.shared.phase(), Phase::Playing);
        broker.handle_status(Status::Finished);
        // Advanced: a Load for entry 2 is issued and the phase is LOADING —
        // the playlist never dips through Stopped.
        match rx.try_recv() {
            Ok(DecoderCmd::Load(Source::Youtube { url, playlist_index: Some(2), .. })) => {
                assert_eq!(url, "https://youtube.com/playlist?list=PLtest");
            }
            other => panic!("expected playlist advance, got {other:?}"),
        }
        assert_eq!(broker.shared.phase(), Phase::Loading);
    }

    #[test]
    fn folder_load_queues_and_plays_newest_first() {
        // Real temp dir: two media files (different mtimes) + noise.
        let dir = std::env::temp_dir().join(format!("kyouko-queue-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("01 - Old.mp3");
        let new = dir.join("02 - New.flac");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&new, b"x").unwrap();
        // set_modified needs a write handle on Windows.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&new)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir(dir.join("nested")).unwrap();

        let (mut broker, _, rx) = broker_with_sink();
        let dir_path = dir.to_string_lossy().into_owned();
        broker.handle_command(Command::Load {
            source: Source::File(dir_path.clone()),
            paused: false,
        });
        // The folder scan queued both media files (newest first) and the
        // decoder was handed the newest one.
        assert_eq!(broker.local_queue.len(), 2);
        assert_eq!(broker.queue_index, Some(0));
        assert!(broker.local_queue[0].display_name().contains("New"));
        assert!(broker.local_queue[1].display_name().contains("Old"));
        let Ok(DecoderCmd::Load(Source::File(p))) = rx.try_recv() else {
            panic!("expected first folder track load")
        };
        assert!(p.contains("02 - New"));

        // NextTrack walks the queue (and does not clobber it).
        broker.handle_command(Command::NextTrack);
        assert_eq!(broker.queue_index, Some(1));
        assert_eq!(broker.local_queue.len(), 2, "navigation must preserve the queue");
        let Ok(DecoderCmd::Load(Source::File(p1))) = rx.try_recv() else {
            panic!("expected queue advance to the oldest file")
        };
        assert!(p1.contains("01"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn next_track_at_queue_end_stops_gracefully() {
        let (mut broker, _, rx) = broker_with_sink();
        broker.local_queue = vec![Source::File("C:\\album\\only.mp3".into())];
        broker.queue_index = Some(0);
        broker.handle_command(Command::NextTrack);
        // End of queue: a graceful Stop, no Load, no restart.
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Stop)));
        assert!(rx.try_recv().is_err());
        assert_eq!(broker.shared.phase(), Phase::Stopped);
    }

    #[test]
    fn prev_track_at_queue_start_reloads_first_file() {
        let (mut broker, _, rx) = broker_with_sink();
        broker.local_queue = vec![Source::File("C:\\album\\only.mp3".into())];
        broker.queue_index = Some(0);
        broker.handle_command(Command::PrevTrack);
        match rx.try_recv() {
            Ok(DecoderCmd::Load(Source::File(p))) => assert!(p.contains("only.mp3")),
            other => panic!("expected restart of first file, got {other:?}"),
        }
        assert_eq!(broker.queue_index, Some(0));
    }

    #[test]
    fn youtube_load_clears_the_local_queue() {
        let (mut broker, _, _rx) = broker_with_sink();
        broker.local_queue = vec![Source::File("C:\\album\\only.mp3".into())];
        broker.queue_index = Some(0);
        broker.handle_command(Command::Load {
            source: Source::Youtube {
                url: "https://youtu.be/x".into(),
                format: "140".into(),
                playlist_index: None,
            },
            paused: false,
        });
        assert!(broker.local_queue.is_empty());
        assert_eq!(broker.queue_index, None);
    }

    #[test]
    fn queue_listing_marks_the_current_track() {
        let queue = vec![
            Source::File("C:\\album\\01 First.wav".into()),
            Source::File("C:\\album\\02 Second.wav".into()),
            Source::File("C:\\album\\03 Third.wav".into()),
        ];
        let listing = format_queue(&queue, Some(1));
        assert!(listing.contains("local queue (3 tracks)"));
        assert!(listing.contains("  [ 1] 01 First.wav"));
        assert!(listing.contains("-> [ 2] 02 Second.wav"), "arrow marks current");
        assert!(listing.contains("  [ 3] 03 Third.wav"));
        // No selection: no arrows anywhere.
        assert!(!format_queue(&queue, None).contains("->"));
    }

    #[test]
    fn queue_listing_empty_message() {
        let listing = format_queue(&[], None);
        assert!(listing.contains("no local queue loaded"));
    }

    #[test]
    fn jump_to_track_validates_and_loads() {
        let (mut broker, _, rx) = broker_with_sink();
        broker.local_queue = vec![
            Source::File("C:\\album\\01.wav".into()),
            Source::File("C:\\album\\02.wav".into()),
        ];
        broker.queue_index = Some(0);
        // 1-based jump to track 2 -> 0-based index 1.
        broker.handle_command(Command::JumpToTrack(2));
        match rx.try_recv() {
            Ok(DecoderCmd::Load(Source::File(p))) => assert!(p.contains("02")),
            other => panic!("expected jump load, got {other:?}"),
        }
        assert_eq!(broker.queue_index, Some(1));
        // Out of bounds (and zero) are rejected cleanly: nothing dispatched.
        broker.handle_command(Command::JumpToTrack(9));
        broker.handle_command(Command::JumpToTrack(0));
        assert!(rx.try_recv().is_err(), "out-of-range jumps dispatch nothing");
        assert_eq!(broker.queue_index, Some(1), "state unchanged on bad jumps");
    }

    #[test]
    fn navigate_works_while_still_loading() {
        let (mut broker, _, rx) = broker_with_sink();
        let pl1 = Source::Youtube {
            url: "https://youtube.com/playlist?list=PLx".into(),
            format: "140".into(),
            playlist_index: Some(1),
        };
        // Load issued, Opened not yet landed (probe in flight).
        broker.handle_command(Command::Load { source: pl1.clone(), paused: false });
        assert!(broker.track.is_none());
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Load(..))), "initial load pending");
        broker.handle_command(Command::NextTrack);
        match rx.try_recv() {
            Ok(DecoderCmd::Load(Source::Youtube { playlist_index: Some(2), .. })) => {}
            other => panic!("expected advance during LOADING, got {other:?}"),
        }
        assert_eq!(broker.shared.phase(), Phase::Loading);
    }

    #[test]
    fn playlist_urls_gain_an_index_videos_do_not() {
        match Source::from_raw("https://youtube.com/playlist?list=PLx") {
            Source::Youtube {
                playlist_index: Some(1),
                ..
            } => {}
            other => panic!("playlist URL must advance: {other:?}"),
        }
        match Source::from_raw("https://www.youtube.com/watch?v=abc&list=PLx") {
            Source::Youtube {
                playlist_index: None,
                ..
            } => {}
            other => panic!("watch URL must stay a single video: {other:?}"),
        }
    }

    #[test]
    fn source_raw_round_trip() {
        assert_eq!(Source::from_raw("C:\\m\\a.flac").raw(), "C:\\m\\a.flac");
        assert!(matches!(
            Source::from_raw("https://youtu.be/x"),
            Source::Youtube { url, format, .. } if url == "https://youtu.be/x" && format == "140"
        ));
        assert!(matches!(Source::from_raw("song.mp3"), Source::File(_)));
    }

    #[test]
    fn panel_reflects_eq_and_volume_changes() {
        let shared = SharedState::new();
        let before = render_panel(&shared, None);
        assert!(before.contains("+0  +0  +0  +0  +0"));
        shared.set_eq_gain(7, 8.0);
        shared.set_volume(0.3);
        let after = render_panel(&shared, None);
        assert!(after.contains("+8"), "panel should show +8: {after}");
        assert!(after.contains(" 30%"), "panel should show 30%: {after}");
        assert!(!after.contains(" 80%"));
    }
}
