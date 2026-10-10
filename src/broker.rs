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
use crossbeam_channel::Sender;
use std::process::{Command as StdCommand, Stdio};
use std::sync::Arc;
use std::thread::self;
use std::time::Duration;

use crate::config::PersistedState;
use crate::ui::CMD_TX;
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
pub const EQ_MAX_GAIN_DB: f32 = 12.0;

/// Extensions a folder scan queues. Deliberately matches the symphonia
/// feature set this binary was built with (.opus waits for symphonia 0.6).
const MEDIA_EXTENSIONS: [&str; 10] =
    ["mp3", "flac", "wav", "m4a", "aac", "mp4", "mkv", "webm", "ogg", "aiff"];

// ── Message vocabulary ───────────────────────────────────────────────────────

/// Something to play. One enum so the decoder owns *all* source handling.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// else opens as a local file. The format id is per-site (YouTube 140,
    /// Bilibili 30232). Playlist-shaped URLs (a `list=` parameter without
    /// an explicit video id) become advancing sources at entry 1.
    pub fn from_raw(raw: &str) -> Source {
        if raw.starts_with("http://") || raw.starts_with("https://") {
            Source::Youtube {
                url: raw.to_string(),
                format: default_format_for(raw).to_string(),
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

/// The terminal listing for a resolved YouTube playlist: 1-based numbering
/// with an arrow marker on the playing entry (1-based playlist index).
fn format_yt_queue(titles: &[String], current: Option<usize>) -> String {
    let mut out = format!("youtube playlist ({} videos):
", titles.len());
    for (i, title) in titles.iter().enumerate() {
        let n = i + 1;
        let marker = if current == Some(n) { "->" } else { "  " };
        out.push_str(&format!("{} [{:>2}] {}
", marker, n, title));
    }
    out
}

/// Does this path carry a supported media extension?
fn is_media_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| MEDIA_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Modification time, if the file system reports one.
fn mtime_of(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Parse a Windows `.url` Internet Shortcut file for its target web
/// address (the `URL=` line of the INI structure, case-insensitive, first
/// match wins). Malformed/missing files and missing targets -> None.
fn parse_url_shortcut(path: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("URL") {
            let target = value.trim();
            if target.starts_with("http://") || target.starts_with("https://") {
                return Some(target.to_string());
            }
            return None; // a URL= line with a non-web target: not streamable
        }
    }
    None
}

/// Does this address stream through the YouTube pathway?
fn is_youtube_url(url: &str) -> bool {
    url.contains("youtube.com/") || url.contains("youtu.be/")
}

/// Does this address stream through the Bilibili pathway? `b23.tv` is
/// Bilibili's shortlink host (it redirects to a bilibili.com watch URL and
/// yt-dlp resolves it directly).
fn is_bilibili_url(url: &str) -> bool {
    url.contains("bilibili.com/") || url.contains("b23.tv/")
}

/// Does this address stream through the Niconico pathway? `nico.ms` is
/// Niconico's shortlink host (redirects to a nicovideo.jp watch URL).
fn is_niconico_url(url: &str) -> bool {
    url.contains("nicovideo.jp/") || url.contains("nico.ms/")
}

/// Any host kyouko routes through the yt-dlp pathway.
fn is_stream_url(url: &str) -> bool {
    is_youtube_url(url) || is_bilibili_url(url) || is_niconico_url(url)
}

/// yt-dlp format id a site's audio streams under. YouTube's default `140`
/// (m4a 128k) does not exist on Bilibili — requesting it there yields an
/// empty stream that fails symphonia's format probe — so Bilibili links
/// select Bilibili's own DASH audio stream `30232` instead. Niconico
/// likewise only resolves under its named format `audio-aac-128kbps`.
fn default_format_for(url: &str) -> &'static str {
    if is_bilibili_url(url) {
        "30232"
    } else if is_niconico_url(url) {
        "audio-aac-128kbps"
    } else {
        "140"
    }
}

/// What a dropped batch of paths resolves to. A `.url` shortcut for a
/// yt-dlp site (YouTube, Bilibili) wins over everything else in the same
/// drop (spec prioritization); otherwise the surviving media files form
/// the queue.
enum DroppedBatch {
    YouTube(String),
    Media(LocalBatch),
}

/// Queue contents plus the last successfully scanned folder in this batch.
#[derive(Default)]
struct LocalBatch {
    files: Vec<Source>,
    last_folder: Option<String>,
}

/// Classify a dropped batch. `.url` shortcuts for streamable sites are
/// parsed and prioritized (the first one wins over everything else in the
/// drop); shortcuts for other hosts are skipped with a warning; every other
/// path is delegated to `collect_media_from_paths` (folder expansion, media
/// filtering, newest-first sort).
fn classify_dropped(paths: &[String]) -> DroppedBatch {
    let mut rest: Vec<String> = Vec::new();
    for path in paths {
        let is_url_file = Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("url"))
            .unwrap_or(false);
        if !is_url_file {
            rest.push(path.clone());
            continue;
        }
        match parse_url_shortcut(path) {
            Some(target) if is_stream_url(&target) => {
                log_info!("BROKER", "dropped .url: stream link {target}");
                return DroppedBatch::YouTube(target);
            }
            Some(target) => {
                log_warn!(
                    "BROKER",
                    "dropped .url is not a streamable link ({target}) - skipping"
                );
            }
            None => {
                log_warn!("BROKER", "dropped .url could not be parsed - skipping");
            }
        }
    }
    match collect_media_from_paths(&rest) {
        Some(batch) => DroppedBatch::Media(batch),
        None => DroppedBatch::Media(LocalBatch::default()),
    }
}

/// Expand a batch of dropped paths (files and/or folders) into the merged,
/// media-filtered, newest-first queue. Folders use the existing shallow
/// scan; loose files are filtered by extension; the merged list is re-sorted
/// by mtime so drops and scans interleave predictably. `None` when nothing
/// in the batch is playable.
fn collect_media_from_paths(paths: &[String]) -> Option<LocalBatch> {
    let mut files: Vec<Source> = Vec::new();
    let mut last_folder = None;
    for path in paths {
        let p = Path::new(path);
        if p.is_dir() {
            let Ok(absolute) = std::path::absolute(p) else {
                log_warn!("BROKER", "cannot resolve folder path: {path}");
                continue;
            };
            let absolute = absolute.to_string_lossy().into_owned();
            let folder_files = scan_folder(&absolute);
            if !folder_files.is_empty() {
                files.extend(folder_files);
                last_folder = Some(absolute);
            }
        } else if p.is_file() && is_media_path(p) {
            files.push(Source::File(path.clone()));
        } else {
            log_debug!("BROKER", "dropped path skipped (not media): {path}");
        }
    }
    if files.is_empty() {
        return None;
    }
    files.sort_by(|a, b| mtime_of(source_path(b)).cmp(&mtime_of(source_path(a))));
    Some(LocalBatch { files, last_folder })
}

/// The path behind a file source (queue entries are always `Source::File`).
fn source_path(source: &Source) -> &Path {
    match source {
        Source::File(p) => Path::new(p),
        _ => Path::new(""),
    }
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

/// Input side: terminal, floating window, and tray icon. All of them
/// converge here — the broker is the single decision point.
#[derive(Clone, Debug)]
pub enum Command {
    /// `paused: true` stages the source in the decoder (buffered, resume is
    /// instant) while the output stays stopped — used for last-track restore.
    Load { source: Source, paused: bool },
    /// Startup-only history restore: rescan a valid folder, then stage its
    /// saved track (or the first entry) paused. Fall back to last_track.
    RestoreState { last_folder: Option<String>, last_track: Option<String> },
    /// Flip between Playing and Paused; restart the retained track when
    /// Stopped (window/tray clicks + terminal `r`).
    TogglePause,
    /// Jump the playhead by a signed offset in seconds (terminal `.` / `,`).
    SeekRelative(f64),
    /// Next / previous track: playlist sources step their entry index
    /// (clamped at the start); everything else restarts from 00:00.
    NextTrack,
    PrevTrack,
    /// Print the active queue (local folder or YouTube playlist) to the
    /// terminal.
    ShowQueue,
    /// Load a batch of dropped paths (files and/or folders). The broker
    /// expands folders, filters to supported media, merges and sorts
    /// newest-first into the local queue.
    LoadDropped(Vec<String>),
    /// A detached `--flat-playlist` fetch resolved a playlist's titles.
    /// Carries the playlist URL so a stale fetch can be dropped.
    YtQueueResolved { url: String, titles: Vec<String> },
    /// Jump straight to a queue track. Carries the user's 1-BASED number —
    /// the broker converts and validates it against the queue bounds.
    JumpToTrack(usize),
    Stop,
    /// 0.0..=1.0 linear gain, applied in the decoder (callback stays a memcpy).
    /// Terminal `vol` and window wheel (+/-10% steps) all land here.
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
    /// Generation identifies the failed attempt, so obsolete failures after
    /// Stop/Skip/retry cannot restart playback or consume the retry budget.
    Failed { source: Source, reason: String, generation: u64 },
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
    /// Most recently OPENED source — survives Stop/Finished so playback can
    /// restart with its exact format/playlist index and retain track history.
    last_opened_source: Option<Source>,
    /// Active folder context survives queue navigation, Stop, and repeat.
    last_folder: Option<String>,
    /// The source a LOADING load will open (no track exists yet) — lets
    /// navigation work while metadata/probe is still in flight.
    pending_source: Option<Source>,
    /// One automatic retry for the current load, including failures after
    /// Opened. Only a fresh track load resets this budget.
    retry_used: bool,
    /// When streaming playlist length is unavailable, allow one following
    /// entry after double failure, then stop if that entry also fails twice.
    failed_playlist_probe: bool,
    /// Local folder queue (shallow scan, newest first) + position. A single
    /// file load becomes a one-entry queue; a YouTube load clears it.
    local_queue: Vec<Source>,
    queue_index: Option<usize>,
    /// YouTube playlist titles resolved by a detached `--flat-playlist`
    /// fetch, plus the playlist URL they belong to. `Some(url)` with an
    /// empty vec = the fetch is still in flight.
    yt_queue: Vec<String>,
    yt_queue_url: Option<String>,

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
            last_opened_source: None,
            last_folder: None,
            pending_source: None,
            retry_used: false,
            failed_playlist_probe: false,
            local_queue: Vec::new(),
            queue_index: None,
            yt_queue: Vec::new(),
            yt_queue_url: None,
            saver: None,
        };
        broker.refresh();
        broker
    }

    /// Install the persistence sink. Fired on the save triggers: volume,
    /// EQ gains, folder ingestion, opened track, Quit.
    pub fn with_saver(mut self, saver: Arc<dyn Fn(PersistedState) + Send + Sync>) -> Self {
        self.saver = Some(saver);
        self
    }

    /// Current persistent-worthy state.
    pub fn persist_snapshot(&self) -> PersistedState {
        PersistedState {
            volume: self.shared.volume(),
            eq_gains: self.shared.eq_gains(),
            last_track: self.last_opened_source.as_ref().map(|source| source.raw().to_string()),
            last_folder: self.last_folder.clone(),
            loop_enabled: self.shared.loop_enabled(),
        }
    }

    fn persist(&self) {
        if let Some(saver) = &self.saver {
            saver(self.persist_snapshot());
            log_debug!("BROKER", "state persisted to disk");
        }
    }

    fn restore_startup(&mut self, last_folder: Option<String>, last_track: Option<String>) {
        self.last_opened_source = last_track.as_deref().map(Source::from_raw);
        if let Some(folder) = last_folder {
            let batch = Path::new(&folder).is_dir()
                .then(|| collect_media_from_paths(std::slice::from_ref(&folder)))
                .flatten();
            if let Some(batch) = batch {
                // Match resolved paths so relative paths, case,
                // and alternate spellings do not lose the saved selection.
                let saved_path = last_track.as_deref()
                    .and_then(|p| std::fs::canonicalize(p).ok());
                let index = saved_path.as_ref().and_then(|saved| {
                    batch.files.iter().position(|source| {
                        std::fs::canonicalize(source_path(source)).ok().as_ref() == Some(saved)
                    })
                }).unwrap_or(0);
                self.last_folder = batch.last_folder;
                self.local_queue = batch.files;
                self.queue_index = Some(index);
                self.yt_queue.clear();
                self.yt_queue_url = None;
                log_info!(
                    "BROKER",
                    "restored folder queue: {} tracks from {folder}, selected {}",
                    self.local_queue.len(), index + 1
                );
                self.begin_load(self.local_queue[index].clone(), true);
                return;
            }
            log_warn!("BROKER", "saved folder unavailable or contains no supported media: {folder}");
        }
        self.last_folder = None;
        if let Some(raw) = last_track {
            self.handle_command(Command::Load { source: Source::from_raw(&raw), paused: true });
        }
    }

    /// The common load tail - everything a Load does once queue management
    /// has settled which source to open.
    fn begin_load(&mut self, source: Source, paused: bool) {
        self.retry_used = false;
        self.start_attempt(source, paused);
    }

    /// Retry uses the same load tail without resetting its per-track budget
    /// or re-running queue ingestion.
    fn start_attempt(&mut self, source: Source, paused: bool) {
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

    fn playback_failed(&mut self, source: Source, reason: String, generation: u64) {
        let current = self.pending_source.as_ref()
            .or_else(|| self.track.as_ref().map(|track| &track.source));
        if generation != self.shared.generation() || current != Some(&source)
            || self.shared.phase() == Phase::Stopped
        {
            log_debug!("BROKER", "obsolete failure ignored: {source} (generation {generation})");
            return;
        }
        let paused = self.stage_paused || self.shared.phase() == Phase::Paused;
        if !self.retry_used {
            log_warn!("BROKER", "playback failed: {source}: {reason}; retrying (attempt 2/2)");
            self.retry_used = true;
            self.start_attempt(source, paused);
            return;
        }
        log_error!("BROKER", "playback failed after 2 attempts: {source}: {reason}; skipping track");
        // Keep the failed source pending until NextTrack has selected its
        // successor; clearing it first loses streaming playlist position.
        self.track = None;
        self.pending_source = Some(source.clone());
        self.stage_paused = false;
        self.shared.reset_playhead();
        let has_next = if let Some(index) = self.queue_index {
            index + 1 < self.local_queue.len()
        } else if let Source::Youtube { playlist_index: Some(index), .. } = &source {
            if !self.yt_queue.is_empty() {
                *index < self.yt_queue.len()
            } else if !self.failed_playlist_probe {
                self.failed_playlist_probe = true;
                true
            } else {
                false
            }
        } else {
            false
        };
        if has_next {
            self.handle_command(Command::NextTrack);
            // A restored or manually paused track must not autoplay its
            // successor when failure handling advances the queue.
            self.stage_paused = paused;
        } else {
            log_info!("BROKER", "no next playable queue entry — stopping after failure");
            self.handle_command(Command::Stop);
        }
    }

    /// Load a track straight from the local folder queue WITHOUT re-running
    /// queue management (which would clobber the folder queue on every
    /// auto-advance). Used by navigation and EOF advancement.
    fn load_queue_track(&mut self, index: usize) {
        self.queue_index = Some(index);
        let source = self.local_queue[index].clone();
        self.begin_load(source, false);
    }

    /// Make sure a flat-playlist metadata fetch is in flight for `url`.
    /// Spawns a detached, simulate-only yt-dlp (no stream resolution, so it
    /// self-exits after printing the index — no crawler) that reports back
    /// through `Command::YtQueueResolved`. Skipped when the fetch for this
    /// exact playlist is already in flight or resolved.
    fn ensure_yt_queue_fetch(&mut self, url: &str) {
        if self.yt_queue_url.as_deref() == Some(url) {
            return;
        }
        self.yt_queue.clear();
        self.yt_queue_url = Some(url.to_string());
        let Some(tx) = CMD_TX.get().cloned() else {
            log_warn!("BROKER", "no command channel - playlist listing unavailable");
            return;
        };
        let url = url.to_string();
        let spawned = thread::Builder::new()
            .name("kyouko-ytlist".into())
            .spawn(move || {
                let output = StdCommand::new("yt-dlp")
                    .args(["--flat-playlist", "--print", "title", &url])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .output();
                match output {
                    Ok(out) if out.status.success() => {
                        let titles: Vec<String> = String::from_utf8_lossy(&out.stdout)
                            .lines()
                            .map(str::trim)
                            .filter(|line| !line.is_empty())
                            .map(String::from)
                            .collect();
                        if titles.is_empty() {
                            log_warn!("TITLE", "flat-playlist fetch resolved no titles");
                        } else {
                            log_info!("TITLE", "playlist resolved: {} videos", titles.len());
                            let _ = tx.send(Command::YtQueueResolved { url, titles });
                        }
                    }
                    Ok(_) => log_warn!("TITLE", "flat-playlist fetch exited nonzero"),
                    Err(e) => log_warn!("TITLE", "flat-playlist fetch failed: {e}"),
                }
            });
        if let Err(e) = spawned {
            log_warn!("BROKER", "playlist fetch thread failed to spawn: {e}");
        }
    }

    /// The 1-based playlist index of the playing/pending track, when a
    /// playlist-shaped YouTube source is active.
    fn current_yt_index(&self) -> Option<usize> {
        let current = self
            .track
            .as_ref()
            .map(|t| t.source.clone())
            .or_else(|| self.pending_source.clone());
        match current {
            Some(Source::Youtube { playlist_index: Some(n), .. }) => Some(n),
            _ => None,
        }
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
            if dir > 0 && !self.yt_queue.is_empty() && *n >= self.yt_queue.len() {
                log_info!("BROKER", "navigate: end of streaming playlist — stopping");
                self.handle_command(Command::Stop);
                return;
            }
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
            self.begin_load(target, false);
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
            Command::RestoreState { last_folder, last_track } => {
                self.restore_startup(last_folder, last_track);
            }
            Command::LoadDropped(paths) => {
                // Classify the batch first: a YouTube .url shortcut wins over
                // everything else in the same drop; otherwise the surviving
                // media files form the queue (newest first). An unplayable
                // result leaves the player completely untouched.
                match classify_dropped(&paths) {
                    DroppedBatch::YouTube(url) => {
                        // Identical to typing `y <url>`: the playlist flow
                        // (flat fetch, index jumps, EOF advance) works as-is.
                        self.handle_command(Command::Load {
                            source: Source::from_raw(&url),
                            paused: false,
                        });
                    }
                    DroppedBatch::Media(batch) if !batch.files.is_empty() => {
                        self.failed_playlist_probe = false;
                        log_info!(
                            "BROKER",
                            "drop queue: {} media files (newest first)",
                            batch.files.len()
                        );
                        self.last_folder = batch.last_folder;
                        self.local_queue = batch.files;
                        self.queue_index = Some(0);
                        self.yt_queue.clear();
                        self.yt_queue_url = None;
                        self.persist();
                        let first = self.local_queue[0].clone();
                        self.begin_load(first, false);
                    }
                    _ => {
                        log_error!(
                            "BROKER",
                            "drop contained no supported media files — playback state unchanged"
                        );
                        self.refresh();
                    }
                }
            }
            Command::Load { source, paused } => {
                // Folder expansion: `p <dir>` queues the folder's media files
                // (shallow scan, newest first) and plays the first one. A
                // folder with no supported media leaves the player untouched.
                let mut source = source;
                let mut queue_managed = false;
                if let Source::File(path) = &source {
                    // A typed/dropped .url shortcut: parse it; a YouTube
                    // target routes to the streaming flow, anything else is
                    // skipped without touching playback.
                    if Path::new(path)
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext.eq_ignore_ascii_case("url"))
                        .unwrap_or(false)
                    {
                        match parse_url_shortcut(path) {
                            Some(target) if is_youtube_url(&target) => {
                                log_info!("BROKER", ".url shortcut: YouTube link {target}");
                                self.handle_command(Command::Load {
                                    source: Source::from_raw(&target),
                                    paused,
                                });
                                return Flow::Continue;
                            }
                            Some(target) => {
                                log_warn!(
                                    "BROKER",
                                    ".url shortcut is not a YouTube link ({target}) - ignoring"
                                );
                                self.refresh();
                                return Flow::Continue;
                            }
                            None => {
                                log_warn!("BROKER", ".url shortcut could not be parsed - ignoring");
                                self.refresh();
                                return Flow::Continue;
                            }
                        }
                    }
                    if Path::new(path).is_dir() {
                        match collect_media_from_paths(std::slice::from_ref(path)) {
                            Some(batch) => {
                                log_info!(
                                    "BROKER",
                                    "folder queue: {} tracks from {path}",
                                    batch.files.len()
                                );
                                self.last_folder = batch.last_folder;
                                self.local_queue = batch.files;
                                self.queue_index = Some(0);
                                // A folder load supersedes any YouTube listing.
                                self.yt_queue.clear();
                                self.yt_queue_url = None;
                                self.persist();
                                source = self.local_queue[0].clone();
                                queue_managed = true;
                            }
                            None => {
                                log_error!(
                                    "BROKER",
                                    "folder {path} contains no supported media files — playback state unchanged"
                                );
                                self.refresh();
                                return Flow::Continue;
                            }
                        }
                    }
                }
                if !queue_managed {
                    self.last_folder = None;
                    // A direct file load becomes a one-entry queue; a YouTube
                    // load supersedes any local queue.
                    match &source {
                        Source::File(_) => {
                            self.local_queue = vec![source.clone()];
                            self.queue_index = Some(0);
                            self.yt_queue.clear();
                            self.yt_queue_url = None;
                        }
                        Source::Youtube { url, playlist_index: Some(_), .. } => {
                            self.local_queue.clear();
                            self.queue_index = None;
                            let url = url.clone();
                            self.ensure_yt_queue_fetch(&url);
                        }
                        Source::Youtube { .. } => {
                            // Non-playlist YouTube: clears both queues.
                            self.local_queue.clear();
                            self.queue_index = None;
                            self.yt_queue.clear();
                            self.yt_queue_url = None;
                        }
                    }
                }
                self.failed_playlist_probe = false;
                self.begin_load(source, paused);
            }
            Command::SeekRelative(delta) => {
                log_info!("BROKER", "seek {delta:+.1}s");
                self.send_decoder(DecoderCmd::SeekRelative(delta));
            }
            Command::NextTrack => self.navigate(1),
            Command::PrevTrack => self.navigate(-1),
            Command::YtQueueResolved { url, titles } => {
                // Only accept the fetch for the still-current playlist; a
                // stale fetch (superseded by another load) is dropped.
                if self.yt_queue_url.as_deref() == Some(url.as_str()) {
                    log_info!("BROKER", "youtube playlist resolved: {} videos", titles.len());
                    self.yt_queue = titles;
                }
            }
            Command::ShowQueue => {
                // The queue is broker-owned: the listing prints from this
                // event loop (the spec's single-owner requirement). An active
                // YouTube playlist takes precedence over the local queue.
                if self.yt_queue_url.is_some() {
                    if self.yt_queue.is_empty() {
                        println!("YouTube playlist metadata is still loading...");
                    } else {
                        let current = self.current_yt_index();
                        println!("{}", format_yt_queue(&self.yt_queue, current));
                    }
                } else {
                    let current = self.queue_index;
                    println!("{}", format_queue(&self.local_queue, current));
                }
            }
            Command::JumpToTrack(num) => {
                // A playlist-shaped YouTube current/pending source routes to
                // the YT listing; everything else uses the local folder queue.
                let yt_active = self.current_yt_index().is_some();
                if yt_active {
                    if self.yt_queue.is_empty() {
                        log_warn!("BROKER", "jump: YouTube playlist metadata is still loading");
                    } else {
                        match num.checked_sub(1).filter(|idx| *idx < self.yt_queue.len()) {
                            Some(i0) => {
                                log_info!(
                                    "BROKER",
                                    "jump: youtube track {num} of {} - {}",
                                    self.yt_queue.len(),
                                    self.yt_queue[i0]
                                );
                                // Rebuild the target from the playing source
                                // so a custom format id survives the jump.
                                if let Some(target) = self
                                    .track
                                    .as_ref()
                                    .map(|t| t.source.clone())
                                    .or_else(|| self.pending_source.clone())
                                    .and_then(|src| match src {
                                        Source::Youtube { url, format, .. } => {
                                            Some(Source::Youtube {
                                                url,
                                                format,
                                                playlist_index: Some(i0 + 1),
                                            })
                                        }
                                        _ => None,
                                    })
                                {
                                    self.handle_command(Command::Load {
                                        source: target,
                                        paused: false,
                                    });
                                }
                            }
                            None => {
                                log_warn!(
                                    "BROKER",
                                    "jump: track {num} out of range (1-{})",
                                    self.yt_queue.len()
                                );
                            }
                        }
                    }
                    return Flow::Continue;
                }
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
                Phase::Stopped => {
                    self.failed_playlist_probe = false;
                    if let Some(index) = self.queue_index.filter(|&i| i < self.local_queue.len()) {
                        log_info!("BROKER", "restart: selected local track {}", index + 1);
                        self.load_queue_track(index);
                    } else if let Some(source) = self.last_opened_source.clone() {
                        log_info!("BROKER", "restart: {source}");
                        self.begin_load(source, false);
                    } else {
                        log_warn!("BROKER", "toggle ignored — nothing loaded");
                    }
                }
                p => log_warn!("BROKER", "toggle ignored while {p}"),
            },
            Command::Stop => {
                log_info!("BROKER", "stop");
                self.send_decoder(DecoderCmd::Stop);
                self.shared.bump_generation();
                self.pending_source = None;
                self.stage_paused = false;
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
                self.refresh();
            }
            // Bypass only: eq_enabled gates the biquad pass in the decoder;
            // the saved band gains are untouched, so nothing to persist.
            Command::ToggleEq => {
                let on = !self.shared.eq_enabled();
                self.shared.set_eq_enabled(on);
                log_info!("BROKER", "eq: {}", if on { "ON" } else { "OFF" });
                self.refresh();
            }
            Command::ToggleLoop => {
                let on = !self.shared.loop_enabled();
                self.shared.set_loop_enabled(on);
                log_info!("LOOP", "repeat: {}", if on { "ON" } else { "OFF" });
                self.refresh();
                self.persist();
            }
            // Presentation-only: intercepted by the Win32 command drain.
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
                self.pending_source = None;
                self.track = None;
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
                self.last_opened_source = Some(source.clone());
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
                self.failed_playlist_probe = false;
                // 1. mpv loop-file: replay the exact source (preserving a
                //    custom YouTube format id when the track carries one).
                let replay = self.shared.loop_enabled().then(|| {
                    self.track
                        .as_ref()
                        .map(|t| t.source.clone())
                        .or_else(|| self.last_opened_source.clone())
                });
                if let Some(Some(source)) = replay {
                    log_info!("LOOP", "repeat: replaying {source}");
                    // Replaying a queue entry must retain its folder context.
                    self.begin_load(source, false);
                    self.refresh();
                    return;
                }
                // 2. Playlist progression: a finished playlist entry advances
                //    to the next index via a fresh (sequential, single-process)
                //    load — title prefetch included.
                if let Some(source) =
                    self.track.as_ref().map(|t| t.source.clone()).and_then(|src| advance_playlist(&src))
                {
                    log_info!("BROKER", "playlist: advancing to entry {:?}", source);
                    self.handle_command(Command::NextTrack);
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
            Status::Failed { source, reason, generation } => {
                self.playback_failed(source, reason, generation);
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

/// Borderless panel — plain text lines only (the frame was removed by
/// design); the layered window's own edges provide the boundary.
///
/// Compact 80 px layout: the window interior fits ~14 Consolas cells, so
/// every line is tuned to that budget and the title truncates. The dual
/// status row's column layout is fixed by ui/render.rs's STATUS_*_COLS
/// hit-test table — keep the two in sync.
pub fn render_panel(shared: &SharedState, track: Option<&TrackMeta>) -> String {
    let timeline = match track.and_then(|t| t.duration) {
        Some(d) => format!("{}/{}", fmt_mmss(shared.position()), fmt_mmss(d)),
        None => format!("{}/--:--", fmt_mmss(shared.position())),
    };
    let title = match track {
        Some(t) => t.title.clone().unwrap_or_else(|| t.source.display_name().to_string()),
        None => "(none)".into(),
    };
    let v = shared.volume();
    // Half-resolution bar: a 10-cell bar plus a label no longer fits 80 px.
    let cells = ((v * 5.0).round() as usize).min(5);
    let bar = format!("{}{}", "|".repeat(cells), "-".repeat(5 - cells));

    let mut p = String::with_capacity(200);
    p.push_str(&row(&format!("KYOUKO {}", shared.phase())));
    p.push('\n');
    p.push_str(&row(&timeline));
    p.push('\n');
    p.push_str(&row(&title.chars().take(14).collect::<String>()));
    p.push('\n');
    p.push_str(&row(&format!("vol {bar} {:>3}%", (v * 100.0).round() as u32)));
    p.push('\n');
    // Dual status row: eq state + repeat state side-by-side. The column
    // layout is fixed by ui/render.rs's STATUS_*_COLS hit-test table
    // (`eq:ON ` at columns 0..6, `rep:OFF` at 7..14) — keep the format in
    // sync. The eq state is padded to 3 cells so `rep:` starts at a fixed
    // column regardless of ON/OFF width.
    p.push_str(&row(&format!(
        "eq:{:<3} rep:{}",
        if shared.eq_enabled() { "ON" } else { "OFF" },
        if shared.loop_enabled() { "ON" } else { "OFF" },
    )));
    // Band gains are NOT text rows — the window draws them as the 10-column
    // Braille equalizer strip (ui/render.rs), tuned by hovering a column and
    // scrolling.
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

    fn fail(broker: &mut Broker, source: &Source) {
        broker.handle_status(Status::Failed {
            source: source.clone(), reason: "simulated playback failure".into(),
            generation: broker.shared.generation(),
        });
    }

    fn assert_load(rx: &crossbeam_channel::Receiver<DecoderCmd>, expected: &Source) {
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Load(source)) if source == *expected));
    }

    #[test]
    fn failure_retry_success_keeps_budget_until_next_track() {
        let (mut broker, _, rx) = broker_with_sink();
        let first = Source::File("first.mp3".into());
        let next = Source::File("next.mp3".into());
        broker.local_queue = vec![first.clone(), next.clone()];
        broker.last_folder = Some("folder context".into());
        broker.load_queue_track(0);
        assert_load(&rx, &first);
        fail(&mut broker, &first);
        assert_load(&rx, &first);
        assert!(broker.retry_used);
        assert_eq!(broker.queue_index, Some(0));
        broker.handle_status(Status::Opened {
            source: first.clone(), sample_rate: 48_000, channels: 2,
            title: None, duration: None,
        });
        assert_eq!(broker.shared.phase(), Phase::Playing);
        assert!(broker.retry_used, "Opened must not allow unlimited stream-drop retries");
        fail(&mut broker, &first);
        assert_load(&rx, &next);
        assert_eq!(broker.local_queue.len(), 2);
        assert_eq!(broker.queue_index, Some(1));
        assert_eq!(broker.last_folder.as_deref(), Some("folder context"));
        assert!(!broker.retry_used, "a different queue track receives a fresh budget");
        fail(&mut broker, &next);
        assert_load(&rx, &next);
        fail(&mut broker, &next);
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Stop)));
        assert_eq!(broker.shared.phase(), Phase::Stopped);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn streaming_double_failure_advances_with_exact_format_and_index() {
        let (mut broker, _, rx) = broker_with_sink();
        let first = Source::Youtube {
            url: "https://youtube.com/playlist?list=test".into(),
            format: "custom-format".into(), playlist_index: Some(2),
        };
        broker.yt_queue_url = Some(first.raw().into());
        broker.yt_queue = vec!["one".into(), "two".into(), "three".into()];
        broker.begin_load(first.clone(), false);
        assert_load(&rx, &first);
        fail(&mut broker, &first);
        assert_load(&rx, &first);
        fail(&mut broker, &first);
        let next = advance_playlist(&first).unwrap();
        assert_load(&rx, &next);
        assert_eq!(broker.current_yt_index(), Some(3));
        assert!(!broker.retry_used);
        assert_eq!(broker.yt_queue.len(), 3);
        fail(&mut broker, &next);
        assert_load(&rx, &next);
        fail(&mut broker, &next);
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Stop)));
        assert_eq!(broker.shared.phase(), Phase::Stopped);
        assert!(rx.try_recv().is_err(), "end of playlist must not request a nonexistent entry");
    }

    #[test]
    fn standalone_double_failure_stops_even_with_repeat_enabled() {
        for source in [Source::File("missing.mp3".into()), Source::from_raw("https://youtu.be/unavailable")] {
            let (mut broker, _, rx) = broker_with_sink();
            broker.shared.set_loop_enabled(true);
            broker.handle_command(Command::Load { source: source.clone(), paused: false });
            assert_load(&rx, &source);
            fail(&mut broker, &source);
            assert_load(&rx, &source);
            fail(&mut broker, &source);
            assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Stop)));
            assert_eq!(broker.shared.phase(), Phase::Stopped);
            // Repeated/delayed notifications cannot restart an exhausted load.
            fail(&mut broker, &source);
            assert!(rx.try_recv().is_err());
            broker.handle_command(Command::Load { source: source.clone(), paused: false });
            assert_load(&rx, &source);
            fail(&mut broker, &source);
            assert_load(&rx, &source);
        }
    }

    #[test]
    fn failure_advancement_preserves_paused_playback_intent() {
        let (mut broker, _, rx) = broker_with_sink();
        let first = Source::File("first.mp3".into());
        let next = Source::File("next.mp3".into());
        broker.local_queue = vec![first.clone(), next.clone()];
        broker.queue_index = Some(0);
        broker.begin_load(first.clone(), true);
        assert_load(&rx, &first);
        fail(&mut broker, &first);
        assert_load(&rx, &first);
        assert!(broker.stage_paused);
        broker.handle_status(Status::Opened {
            source: first.clone(), sample_rate: 48_000, channels: 2,
            title: None, duration: None,
        });
        assert_eq!(broker.shared.phase(), Phase::Paused);
        fail(&mut broker, &first);
        assert_load(&rx, &next);
        assert!(broker.stage_paused);
        broker.handle_status(Status::Opened {
            source: next, sample_rate: 48_000, channels: 2,
            title: None, duration: None,
        });
        assert_eq!(broker.shared.phase(), Phase::Paused);
    }

    #[test]
    fn obsolete_failures_after_retry_stop_or_new_load_are_ignored() {
        let (mut broker, _, rx) = broker_with_sink();
        let source = Source::File("same.mp3".into());
        broker.handle_command(Command::Load { source: source.clone(), paused: false });
        assert_load(&rx, &source);
        let obsolete_generation = broker.shared.generation();
        fail(&mut broker, &source);
        assert_load(&rx, &source);
        broker.handle_status(Status::Failed {
            source: source.clone(), reason: "late first-attempt error".into(), generation: obsolete_generation,
        });
        assert!(rx.try_recv().is_err());
        assert!(broker.retry_used);
        broker.handle_command(Command::Stop);
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Stop)));
        fail(&mut broker, &source);
        assert!(rx.try_recv().is_err());
        broker.handle_command(Command::Load { source: source.clone(), paused: false });
        assert_load(&rx, &source);
        broker.handle_status(Status::Failed {
            source: source.clone(), reason: "late older load".into(), generation: obsolete_generation,
        });
        assert!(rx.try_recv().is_err());
        assert!(!broker.retry_used);
        // Even a current-generation error for another source is obsolete.
        fail(&mut broker, &Source::File("other.mp3".into()));
        assert!(!broker.retry_used);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn unknown_streaming_playlist_bounds_failure_advancement() {
        let (mut broker, _, rx) = broker_with_sink();
        let source = Source::Youtube {
            url: "https://youtube.com/playlist?list=unresolved".into(),
            format: "140".into(), playlist_index: Some(1),
        };
        broker.begin_load(source.clone(), false);
        assert_load(&rx, &source);
        fail(&mut broker, &source);
        assert_load(&rx, &source);
        fail(&mut broker, &source);
        let next = advance_playlist(&source).unwrap();
        assert_load(&rx, &next);
        fail(&mut broker, &next);
        assert_load(&rx, &next);
        fail(&mut broker, &next);
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Stop)));
        assert_eq!(broker.shared.phase(), Phase::Stopped);
        assert!(rx.try_recv().is_err(), "unavailable metadata must not enumerate forever");
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
            generation: broker.shared.generation(),
        });
        assert!(broker.stage_paused, "retry retains startup's paused intent");
        fail(&mut broker, &Source::File("nowhere.mp3".into()));
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
    fn stopped_toggle_restarts_selected_local_track_after_eof_or_stop() {
        for count in [1, 3] {
            for explicit_stop in [false, true] {
                let (mut broker, _, rx) = broker_with_sink();
                broker.local_queue = (0..count)
                    .map(|i| Source::File(format!("C:\\album\\track-{i}.mp3")))
                    .collect();
                let index = count - 1;
                broker.queue_index = Some(index);
                broker.last_folder = (count > 1).then(|| r"C:\album".to_string());
                let selected = broker.local_queue[index].clone();
                broker.handle_status(Status::Opened {
                    source: selected.clone(), sample_rate: 48_000, channels: 2,
                    title: None, duration: Some(Duration::from_secs(10)),
                });
                let saved = broker.persist_snapshot();
                if explicit_stop {
                    broker.handle_command(Command::Stop);
                    assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Stop)));
                } else {
                    broker.handle_status(Status::Finished);
                }
                assert_eq!(broker.shared.phase(), Phase::Stopped);
                assert!(broker.track.is_none());
                assert!(rx.try_recv().is_err(), "EOF must stay idle until restart is requested");
                broker.handle_command(Command::TogglePause);
                assert_eq!(broker.shared.phase(), Phase::Loading);
                assert!(!broker.stage_paused);
                assert_eq!(broker.shared.position(), Duration::ZERO);
                assert_eq!(broker.local_queue.len(), count);
                assert_eq!(broker.queue_index, Some(index));
                assert_eq!(broker.persist_snapshot(), saved);
                assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Load(source)) if source.raw() == selected.raw()));
                broker.handle_status(Status::Opened {
                    source: selected, sample_rate: 48_000, channels: 2,
                    title: None, duration: Some(Duration::from_secs(10)),
                });
                assert_eq!(broker.shared.phase(), Phase::Playing);
                broker.handle_command(Command::TogglePause);
                assert_eq!(broker.shared.phase(), Phase::Paused);
                broker.handle_command(Command::TogglePause);
                assert_eq!(broker.shared.phase(), Phase::Playing);
                assert!(rx.try_recv().is_err(), "pause/resume must not reload");
            }
        }
    }

    #[test]
    fn stopped_toggle_preserves_stream_format_and_last_played_playlist_entry() {
        for playlist_index in [None, Some(3)] {
            let (mut broker, _, rx) = broker_with_sink();
            let source = Source::Youtube {
                url: "https://youtube.com/playlist?list=PLtest".into(),
                format: "custom-format".into(), playlist_index,
            };
            broker.handle_status(Status::Opened {
                source: source.clone(), sample_rate: 48_000, channels: 2,
                title: None, duration: None,
            });
            if playlist_index.is_some() {
                broker.yt_queue = vec!["one".into(), "two".into(), "three".into()];
            }
            broker.handle_status(Status::Finished);
            if playlist_index.is_some() {
                assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Stop)));
            }
            assert_eq!(broker.shared.phase(), Phase::Stopped);
            broker.handle_command(Command::TogglePause);
            assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Load(Source::Youtube {
                format, playlist_index: actual_index, ..
            })) if format == "custom-format" && actual_index == playlist_index));
            assert_eq!(broker.shared.phase(), Phase::Loading);
        }
    }

    #[test]
    fn toggle_without_track_or_during_loading_does_not_issue_a_load() {
        let (mut broker, _, rx) = broker_with_sink();
        broker.handle_command(Command::TogglePause);
        assert_eq!(broker.shared.phase(), Phase::Stopped);
        assert!(rx.try_recv().is_err());
        broker.handle_command(Command::Load { source: Source::File("pending.mp3".into()), paused: false });
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Load(_))));
        broker.handle_command(Command::TogglePause);
        assert_eq!(broker.shared.phase(), Phase::Loading);
        assert!(rx.try_recv().is_err());
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

        let (mut broker, sink, rx) = broker_with_sink();
        let dir_path = dir.to_string_lossy().into_owned();
        broker.handle_command(Command::Load {
            source: Source::File(dir_path.clone()),
            paused: false,
        });
        // The folder scan queued both media files (newest first) and the
        // decoder was handed the newest one.
        assert_eq!(broker.local_queue.len(), 2);
        assert_eq!(sink.lock().unwrap().last().unwrap().last_folder.as_deref(), Some(dir_path.as_str()),
            "folder saved before the decoder opens a track");
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

        // Persist the selected track, then restart after adding a newer file.
        broker.handle_status(Status::Opened {
            source: Source::File(p1.clone()), sample_rate: 48_000, channels: 2,
            title: None, duration: None,
        });
        broker.shared.set_loop_enabled(true);
        broker.handle_status(Status::Finished);
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Load(_))));
        assert_eq!(broker.local_queue.len(), 2, "repeat retains the folder queue");
        broker.handle_command(Command::Stop);
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Stop)));
        let saved = broker.persist_snapshot();
        assert_eq!(saved.last_folder.as_deref(), Some(dir_path.as_str()));
        let latest = dir.join("03 - Latest.wav");
        std::fs::write(&latest, b"x").unwrap();
        std::fs::OpenOptions::new().write(true).open(&latest).unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(120)).unwrap();
        let (mut restarted, _, restarted_rx) = broker_with_sink();
        restarted.handle_command(Command::RestoreState {
            last_folder: saved.last_folder, last_track: saved.last_track,
        });
        assert_eq!(restarted.local_queue.len(), 3, "restart rescans current contents");
        assert_eq!(restarted.local_queue[0].raw(), latest.to_string_lossy());
        assert_eq!(restarted.queue_index, Some(2), "selection follows track path, not old index");
        assert!(matches!(restarted_rx.try_recv(), Ok(DecoderCmd::Load(Source::File(p))) if p == p1));
        restarted.handle_status(Status::Opened {
            source: Source::File(p1), sample_rate: 48_000, channels: 2,
            title: None, duration: None,
        });
        assert_eq!(restarted.shared.phase(), Phase::Paused);
        assert!(format_queue(&restarted.local_queue, restarted.queue_index).contains("local queue (3 tracks)"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropped_relative_folder_persists_and_restores_first_when_saved_track_is_missing() {
        let relative = format!("target/kyouko-folder-test-{}", std::process::id());
        let absolute = std::path::absolute(&relative).unwrap();
        std::fs::create_dir_all(&absolute).unwrap();
        let file = absolute.join("Song.mp3");
        std::fs::write(&file, b"x").unwrap();
        let (mut broker, sink, rx) = broker_with_sink();
        broker.yt_queue_url = Some("https://youtube.com/playlist?list=old".into());
        broker.handle_command(Command::LoadDropped(vec![relative]));
        let saved = sink.lock().unwrap().last().unwrap().clone();
        assert_eq!(saved.last_folder.as_deref(), absolute.to_str());
        assert!(broker.yt_queue_url.is_none());
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Load(_))));
        // An unplayable batch must keep the existing folder and saved state.
        let saves = sink.lock().unwrap().len();
        broker.handle_command(Command::LoadDropped(vec!["missing.txt".into()]));
        assert_eq!(broker.last_folder, saved.last_folder);
        assert_eq!(sink.lock().unwrap().len(), saves);

        let (mut restarted, _, restart_rx) = broker_with_sink();
        restarted.handle_command(Command::RestoreState {
            last_folder: saved.last_folder,
            last_track: Some(absolute.join("removed.mp3").to_string_lossy().into_owned()),
        });
        assert_eq!(restarted.queue_index, Some(0));
        assert!(restarted.stage_paused);
        assert!(matches!(restart_rx.try_recv(), Ok(DecoderCmd::Load(Source::File(p))) if Path::new(&p) == file));
        // Explicit file input replaces the folder context, including a file
        // inside the same directory. Opened persists that replacement.
        restarted.handle_command(Command::Load { source: Source::File(file.to_string_lossy().into_owned()), paused: false });
        assert!(restarted.last_folder.is_none());
        broker.handle_command(Command::LoadDropped(vec![file.to_string_lossy().into_owned()]));
        assert!(sink.lock().unwrap().last().unwrap().last_folder.is_none());
        std::fs::remove_dir_all(absolute).unwrap();
    }

    #[test]
    fn startup_without_playable_folder_falls_back_to_last_track() {
        let empty = std::env::temp_dir().join(format!("kyouko-restore-{}-empty", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        for folder in [None, Some(empty.to_string_lossy().into_owned()),
            Some(empty.join("missing").to_string_lossy().into_owned())] {
            let (mut broker, _, rx) = broker_with_sink();
            let raw = "https://www.bilibili.com/video/BVtest";
            broker.handle_command(Command::RestoreState {
                last_folder: folder, last_track: Some(raw.into()),
            });
            assert!(broker.last_folder.is_none());
            assert_eq!(broker.persist_snapshot().last_track.as_deref(), Some(raw));
            assert!(broker.stage_paused);
            assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Load(Source::Youtube { format, .. })) if format == "30232"));
        }
        let (mut broker, _, rx) = broker_with_sink();
        broker.handle_command(Command::RestoreState { last_folder: Some(empty.to_string_lossy().into_owned()), last_track: None });
        assert_eq!(broker.shared.phase(), Phase::Stopped);
        assert!(rx.try_recv().is_err());
        std::fs::remove_dir_all(empty).unwrap();
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
    fn load_dropped_expands_filters_and_sorts() {
        // Two real files with distinct mtimes + one non-media file + one
        // missing path + a real folder containing a media file.
        let dir = std::env::temp_dir().join(format!("kyouko-drop-{}-expands", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let older = dir.join("older.mp3");
        let newer = dir.join("newer.flac");
        std::fs::write(&older, b"x").unwrap();
        std::fs::write(&newer, b"x").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&newer)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();

        // A dropped subfolder is expanded by the same shallow scan.
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("inner.wav"), b"x").unwrap();

        let paths = vec![
            newer.to_string_lossy().into_owned(),
            format!("{}\\nothing_here.mp3", dir.to_string_lossy()),
            dir.join("notes.txt").to_string_lossy().into_owned(),
            sub.to_string_lossy().into_owned(),
            "Z:\\missing\\drive\\track.flac".to_string(),
            older.to_string_lossy().into_owned(),
        ];
        let Some(batch) = collect_media_from_paths(&paths) else {
            panic!("mixed drop must resolve");
        };
        assert_eq!(batch.last_folder.as_deref(), Some(sub.to_str().unwrap()));
        let queue = batch.files;
        assert_eq!(queue.len(), 3, "older + newer + inner (notes/missing filtered)");
        // Newest first across the whole merged batch.
        assert!(queue[0].display_name().contains("newer.flac"));
        assert!(queue.iter().any(|s| s.display_name().contains("inner.wav")));
        assert!(queue.iter().any(|s| s.display_name().contains("older.mp3")));
        assert!(!queue.iter().any(|s| s.display_name().contains("notes")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_dropped_dispatches_first_and_queues_rest() {
        // Real files (the collector requires existing files; dropped paths
        // that don't resolve are skipped).
        let dir = std::env::temp_dir().join(format!("kyouko-drop-{}-dispatch", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.mp3");
        let b = dir.join("b.mp3");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();

        // Distinct mtimes: b is newer, so the newest-first sort leads with it.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&b)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();

        let (mut broker, _, rx) = broker_with_sink();
        broker.handle_command(Command::LoadDropped(vec![
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        ]));
        assert_eq!(broker.local_queue.len(), 2);
        assert_eq!(broker.queue_index, Some(0));
        // Newest first: b.mp3 leads the queue.
        let Ok(DecoderCmd::Load(Source::File(p))) = rx.try_recv() else {
            panic!("expected first dropped track to load")
        };
        assert!(p.contains("b.mp3"), "newest file plays first: {p}");
        // NextTrack walks the dropped queue to the older file.
        broker.handle_command(Command::NextTrack);
        let Ok(DecoderCmd::Load(Source::File(p))) = rx.try_recv() else {
            panic!("expected queue walk")
        };
        assert!(p.contains("a.mp3"), "oldest file follows: {p}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_dropped_with_no_media_leaves_state_untouched() {
        let (mut broker, _, rx) = broker_with_sink();
        broker.handle_command(Command::LoadDropped(vec![
            "C:\\nothing\\notes.txt".into(),
            "C:\\nothing\\image.png".into(),
        ]));
        assert!(broker.local_queue.is_empty());
        assert!(rx.try_recv().is_err(), "nothing dispatched for a media-free drop");
        assert_eq!(broker.shared.phase(), Phase::Stopped);
    }

    #[test]
    fn youtube_load_clears_the_local_queue() {
        let (mut broker, _, _rx) = broker_with_sink();
        broker.local_queue = vec![Source::File("C:\\album\\only.mp3".into())];
        broker.queue_index = Some(0);
        broker.last_folder = Some(r"C:\album".into());
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
        assert_eq!(broker.last_folder, None);
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
    fn yt_queue_listing_marks_the_playing_entry() {
        let titles = vec![
            "Entry One".to_string(),
            "Entry Two".to_string(),
            "Entry Three".to_string(),
        ];
        // current is the 1-based playlist index of the playing entry.
        let listing = format_yt_queue(&titles, Some(2));
        assert!(listing.contains("youtube playlist (3 videos)"));
        assert!(listing.contains("  [ 1] Entry One"));
        assert!(listing.contains("-> [ 2] Entry Two"), "arrow marks the playing entry");
        assert!(listing.contains("  [ 3] Entry Three"));
        assert!(!format_yt_queue(&titles, None).contains("->"));
    }

    #[test]
    fn yt_queue_resolved_only_applies_to_the_current_playlist() {
        let (mut broker, _, _rx) = broker_with_sink();
        let url_a = "https://youtube.com/playlist?list=A".to_string();
        // A playlist-shaped load sets the fetch URL for A.
        broker.handle_command(Command::Load {
            source: Source::Youtube {
                url: url_a.clone(),
                format: "140".into(),
                playlist_index: Some(1),
            },
            paused: false,
        });
        assert_eq!(broker.yt_queue_url.as_deref(), Some(url_a.as_str()));
        // The fetch for A resolves: titles accepted.
        broker.handle_command(Command::YtQueueResolved {
            url: url_a.clone(),
            titles: vec!["One".into(), "Two".into()],
        });
        assert_eq!(broker.yt_queue.len(), 2);
        // A stale fetch for a DIFFERENT playlist is dropped.
        broker.handle_command(Command::YtQueueResolved {
            url: "https://youtube.com/playlist?list=B".into(),
            titles: vec!["Wrong".into()],
        });
        assert_eq!(broker.yt_queue.len(), 2);
        assert!(!broker.yt_queue.iter().any(|t| t == "Wrong"));
    }

    #[test]
    fn yt_jump_requires_resolved_titles_and_bounds_checks() {
        let (mut broker, _, rx) = broker_with_sink();
        let url = "https://youtube.com/playlist?list=A".to_string();
        let pl = |n: usize| Source::Youtube {
            url: url.clone(),
            format: "140".into(),
            playlist_index: Some(n),
        };
        broker.handle_command(Command::Load { source: pl(1), paused: false });
        assert!(matches!(rx.try_recv(), Ok(DecoderCmd::Load(..))), "initial load pending");
        // Titles still loading: jump is refused cleanly.
        broker.handle_command(Command::JumpToTrack(2));
        assert!(rx.try_recv().is_err(), "no jump dispatch while metadata loads");
        // Titles resolve (simulating the flat-playlist fetch reporting back).
        broker.handle_command(Command::YtQueueResolved {
            url: url.clone(),
            titles: vec!["One".into(), "Two".into()],
        });
        // Jump to track 2 (1-based) -> Load with playlist_index Some(2).
        broker.handle_command(Command::JumpToTrack(2));
        match rx.try_recv() {
            Ok(DecoderCmd::Load(Source::Youtube { playlist_index: Some(2), .. })) => {}
            other => panic!("expected jump load for entry 2, got {other:?}"),
        }
        // The jump target opens: Playing at entry 2.
        broker.handle_status(Status::Opened {
            source: pl(2),
            sample_rate: 44_100,
            channels: 2,
            title: None,
            duration: None,
        });
        assert_eq!(broker.shared.phase(), Phase::Playing);
        // Out of range: nothing dispatched, state unchanged.
        broker.handle_command(Command::JumpToTrack(9));
        assert!(rx.try_recv().is_err());
        assert_eq!(broker.shared.phase(), Phase::Playing);
    }

    #[test]
    fn url_shortcut_parser_extracts_web_targets() {
        let dir = std::env::temp_dir().join(format!("kyouko-url-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shortcut = dir.join("song.url");
        std::fs::write(
            &shortcut,
            "[InternetShortcut]\r\nURL=https://youtu.be/abc123\r\nIDList=\r\nHotKey=0\r\n",
        )
        .unwrap();
        assert_eq!(
            parse_url_shortcut(shortcut.to_string_lossy().as_ref()).as_deref(),
            Some("https://youtu.be/abc123")
        );
        // Case-insensitive key + leading whitespace on the value.
        std::fs::write(&shortcut, "[InternetShortcut]\r\n  url =  https://youtu.be/xyz  \r\n")
            .unwrap();
        assert_eq!(
            parse_url_shortcut(shortcut.to_string_lossy().as_ref()).as_deref(),
            Some("https://youtu.be/xyz")
        );
        // Malformed: no URL= line, empty file, nonexistent file.
        std::fs::write(&shortcut, "[InternetShortcut]\r\nIconIndex=0\r\n").unwrap();
        assert_eq!(parse_url_shortcut(shortcut.to_string_lossy().as_ref()), None);
        std::fs::write(&shortcut, "").unwrap();
        assert_eq!(parse_url_shortcut(shortcut.to_string_lossy().as_ref()), None);
        assert_eq!(
            parse_url_shortcut(dir.join("nope.url").to_string_lossy().as_ref()),
            None
        );
        // Non-web targets are rejected at parse time.
        std::fs::write(&shortcut, "URL=file://C:/local.htm\r\n").unwrap();
        assert_eq!(parse_url_shortcut(shortcut.to_string_lossy().as_ref()), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropped_youtube_shortcut_wins_over_media_files() {
        let dir = std::env::temp_dir().join(format!("kyouko-urlwin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("local.mp3"), b"x").unwrap();
        let shortcut = dir.join("song.url");
        std::fs::write(
            &shortcut,
            "[InternetShortcut]\r\nURL=https://youtu.be/dQw4w9WgXcQ\r\n",
        )
        .unwrap();

        let (mut broker, _, rx) = broker_with_sink();
        broker.handle_command(Command::LoadDropped(vec![
            dir.join("local.mp3").to_string_lossy().into_owned(),
            shortcut.to_string_lossy().into_owned(),
        ]));
        // The YouTube link wins: a Youtube Load is dispatched, the local
        // queue is cleared (superseded).
        match rx.try_recv() {
            Ok(DecoderCmd::Load(Source::Youtube { playlist_index, .. })) => {
                assert_eq!(playlist_index, None, "watch URL is a single video");
            }
            other => panic!("expected youtube load, got {other:?}"),
        }
        assert!(broker.local_queue.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropped_non_youtube_shortcut_is_skipped() {
        let (mut broker, _, rx) = broker_with_sink();
        let shortcut = std::env::temp_dir().join(format!("kyouko-nonurl-{}.url", std::process::id()));
        std::fs::write(&shortcut, "[InternetShortcut]\r\nURL=https://example.com/page\r\n")
            .unwrap();
        broker.handle_command(Command::LoadDropped(vec![shortcut.to_string_lossy().into_owned()]));
        assert!(rx.try_recv().is_err(), "non-YouTube .url dispatches nothing");
        assert_eq!(broker.shared.phase(), Phase::Stopped);
        std::fs::remove_file(&shortcut).unwrap();
    }

    #[test]
    fn bilibili_links_select_their_own_format() {
        // Watch URL and b23.tv shortlink both carry the Bilibili DASH audio
        // format; YouTube keeps its m4a default.
        for url in [
            "https://www.bilibili.com/video/BV1A3hpzAEnZ",
            "https://b23.tv/abc123",
            "http://www.bilibili.com/video/BVxyz?spx=1",
        ] {
            assert!(
                matches!(
                    Source::from_raw(url),
                    Source::Youtube { format, playlist_index: None, .. }
                        if format == "30232"
                ),
                "bilibili link must carry fmt 30232: {url}"
            );
        }
        assert!(matches!(
            Source::from_raw("https://youtu.be/x"),
            Source::Youtube { format, .. } if format == "140"
        ));
        assert!(matches!(
            Source::from_raw("https://example.com/page"),
            Source::Youtube { format, .. } if format == "140"
        ));
    }

    #[test]
    fn niconico_links_select_their_own_format() {
        // Watch URL and nico.ms shortlink both carry the Niconico named
        // audio format; YouTube keeps its m4a default.
        for url in [
            "https://www.nicovideo.jp/watch/sm46878644",
            "https://nico.ms/sm46878644",
            "http://www.nicovideo.jp/watch/sm9",
        ] {
            assert!(
                matches!(
                    Source::from_raw(url),
                    Source::Youtube { format, playlist_index: None, .. }
                        if format == "audio-aac-128kbps"
                ),
                "niconico link must carry audio-aac-128kbps: {url}"
            );
        }
        assert!(matches!(
            Source::from_raw("https://youtu.be/x"),
            Source::Youtube { format, .. } if format == "140"
        ));
        assert!(matches!(
            Source::from_raw("https://example.com/page"),
            Source::Youtube { format, .. } if format == "140"
        ));
    }

    #[test]
    fn dropped_niconico_shortcut_streams_via_yt_dlp() {
        let dir = std::env::temp_dir().join(format!("kyouko-nico-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shortcut = dir.join("sm.url");
        std::fs::write(
            &shortcut,
            "[InternetShortcut]\r\nURL=https://www.nicovideo.jp/watch/sm46878644\r\n",
        )
        .unwrap();

        let (mut broker, _, rx) = broker_with_sink();
        broker.handle_command(Command::LoadDropped(vec![shortcut.to_string_lossy().into_owned()]));
        match rx.try_recv() {
            Ok(DecoderCmd::Load(Source::Youtube { format, playlist_index, .. })) => {
                assert_eq!(format, "audio-aac-128kbps", "niconico drop must not use fmt 140");
                assert_eq!(playlist_index, None);
            }
            other => panic!("expected niconico stream load, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropped_bilibili_shortcut_streams_via_yt_dlp() {
        let dir = std::env::temp_dir().join(format!("kyouko-bili-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shortcut = dir.join("bv.url");
        std::fs::write(
            &shortcut,
            "[InternetShortcut]\r\nURL=https://www.bilibili.com/video/BV1A3hpzAEnZ\r\n",
        )
        .unwrap();

        let (mut broker, _, rx) = broker_with_sink();
        broker.handle_command(Command::LoadDropped(vec![shortcut.to_string_lossy().into_owned()]));
        match rx.try_recv() {
            Ok(DecoderCmd::Load(Source::Youtube { format, playlist_index, .. })) => {
                assert_eq!(format, "30232", "bilibili drop must not use fmt 140");
                assert_eq!(playlist_index, None);
            }
            other => panic!("expected bilibili stream load, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
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
        assert!(before.contains("eq:ON"), "EQ defaults on: {before}");
        // Band gains no longer render as text — they live in the Braille
        // strip — but the ON/OFF row must follow the enable state.
        shared.set_eq_enabled(false);
        shared.set_volume(0.3);
        let after = render_panel(&shared, None);
        assert!(after.contains("eq:OFF"), "panel should show EQ OFF: {after}");
        assert!(after.contains(" 30%"), "panel should show 30%: {after}");
        assert!(!after.contains(" 80%"));
        // And the text must not carry numeric gain cells anymore.
        assert!(!after.contains("+8"), "gains moved to the Braille strip: {after}");
    }

    #[test]
    fn panel_shows_eq_and_repeat_side_by_side() {
        let shared = SharedState::new();
        let line = render_panel(&shared, None).lines().nth(4).unwrap().to_string();
        // Both chunks on the 5th row: eq first, repeat after it.
        assert!(line.starts_with("eq:ON"), "eq chunk first: {line}");
        assert!(line.contains("rep:OFF"), "repeat chunk after eq: {line}");
        // The repeat chunk sits at a fixed column regardless of the eq
        // state width (matches render.rs's STATUS_REPEAT_COLS hit table).
        shared.set_eq_enabled(false);
        let off = render_panel(&shared, None).lines().nth(4).unwrap().to_string();
        assert_eq!(off.find("rep:"), line.find("rep:"), "repeat column drifts: {off}");
        // Every panel line stays within the 80 px text budget (14 cells).
        for line in render_panel(&shared, None).lines() {
            assert!(line.chars().count() <= 14, "panel line too wide: {line}");
        }
    }

    #[test]
    fn toggling_loop_refreshes_panel_and_persists() {
        let (mut broker, sink, _rx) = broker_with_sink();
        broker.handle_command(Command::ToggleLoop);
        let panel = broker.take_refresh().expect("loop toggle marks the view dirty");
        assert!(panel.contains("rep:ON"), "live repeat state: {panel}");
        assert!(sink.lock().unwrap().last().unwrap().loop_enabled);
        // Second toggle back and the panel follows again.
        broker.handle_command(Command::ToggleLoop);
        let panel = broker.take_refresh().expect("second toggle also refreshes");
        assert!(panel.contains("rep:OFF"), "live repeat state: {panel}");
    }

    #[test]
    fn toggling_eq_refreshes_panel() {
        let (mut broker, _sink, _rx) = broker_with_sink();
        broker.handle_command(Command::ToggleEq);
        let panel = broker.take_refresh().expect("eq toggle marks the view dirty");
        assert!(panel.contains("eq:OFF"), "live eq state: {panel}");
    }
}
