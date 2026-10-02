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
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering,
};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;

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

// ── Message vocabulary ───────────────────────────────────────────────────────

/// Something to play. One enum so the decoder owns *all* source handling.
#[derive(Clone, Debug)]
pub enum Source {
    /// Path as typed on the terminal (wide-char conversion happens at open).
    File(String),
    /// `format` is a yt-dlp format id; default "140" = m4a 128 kbps.
    Youtube { url: String, format: String },
}

impl Source {
    /// Short name for the panel: file stem-ish or the URL.
    pub fn display_name(&self) -> &str {
        match self {
            Source::File(p) => p.rsplit(['/', '\\']).next().unwrap_or(p),
            Source::Youtube { url, .. } => url,
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::File(p) => write!(f, "file({p})"),
            Source::Youtube { url, format } => write!(f, "youtube({url} [fmt {format}])"),
        }
    }
}

/// Input side: terminal now, tray menu / global hotkeys later. All of them
/// converge here — the broker is the single decision point.
#[derive(Clone, Debug)]
pub enum Command {
    Load(Source),
    Pause,
    Resume,
    Toggle,
    Stop,
    /// 0.0..=1.0 linear gain, applied in the decoder (callback stays a memcpy).
    Volume(f32),
    /// Band gain in dB, clamped to ±12. `None` = all bands.
    EqGain { band: Option<usize>, gain_db: f32 },
    EqEnabled(bool),
    DumpState,
    Quit,
}

/// Broker → decoder. Kept deliberately tiny: the decoder is a state machine
/// parked on this channel, not a service with a rich API.
#[derive(Debug)]
pub enum DecoderCmd {
    Load(Source),
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
    /// Current track's rate — makes `frames_played` meaningful as time.
    sample_rate: AtomicU32,
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
        self.sample_rate.store(0, Ordering::Relaxed);
    }
    pub fn set_sample_rate(&self, rate: u32) {
        self.sample_rate.store(rate, Ordering::Relaxed);
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

/// The decision point. Owns the decoder's command channel; everything else is
/// derived state. The presentation layer (headless loop now, Win32 later)
/// calls `handle_command` / `handle_status` and renders `view`.
pub struct Broker {
    shared: Arc<SharedState>,
    decoder_tx: Sender<DecoderCmd>,
    track: Option<TrackMeta>,
    view: String,
    view_dirty: bool,
}

impl Broker {
    pub fn new(shared: Arc<SharedState>, decoder_tx: Sender<DecoderCmd>) -> Self {
        let mut broker = Self {
            shared,
            decoder_tx,
            track: None,
            view: String::new(),
            view_dirty: false,
        };
        broker.refresh();
        broker
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
            Command::Load(source) => {
                log_info!("BROKER", "load: {source}");
                self.track = None;
                self.shared.reset_playhead();
                self.shared.bump_generation();
                self.set_phase(Phase::Loading, "load requested");
                self.send_decoder(DecoderCmd::Load(source));
            }
            Command::Pause => {
                if self.shared.phase() == Phase::Playing {
                    self.set_phase(Phase::Paused, "pause");
                } else {
                    log_warn!("BROKER", "pause ignored while {}", self.shared.phase());
                }
            }
            Command::Resume => {
                if self.shared.phase() == Phase::Paused {
                    self.set_phase(Phase::Playing, "resume");
                } else {
                    log_warn!("BROKER", "resume ignored while {}", self.shared.phase());
                }
            }
            Command::Toggle => match self.shared.phase() {
                Phase::Playing => self.set_phase(Phase::Paused, "toggle"),
                Phase::Paused => self.set_phase(Phase::Playing, "toggle"),
                p => log_warn!("BROKER", "toggle ignored while {p}"),
            },
            Command::Stop => {
                log_info!("BROKER", "stop");
                self.send_decoder(DecoderCmd::Stop);
                self.track = None;
                self.shared.reset_playhead();
                self.set_phase(Phase::Stopped, "stop");
            }
            Command::Volume(v) => {
                let v = v.clamp(0.0, 1.0);
                self.shared.set_volume(v);
                log_info!("BROKER", "volume: {:.0}%", v * 100.0);
            }
            Command::EqGain { band, gain_db } => match band {
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
            },
            Command::EqEnabled(on) => {
                self.shared.set_eq_enabled(on);
                log_info!("BROKER", "eq: {}", if on { "ON" } else { "OFF" });
            }
            Command::DumpState => {
                let s = &self.shared;
                log_info!(
                    "BROKER",
                    "state dump: phase={} generation={} rate={} frames={} pos={} vol={:.2} eq_on={} gains_cdb={:?}",
                    s.phase(),
                    s.generation(),
                    s.sample_rate.load(Ordering::Relaxed),
                    s.frames_played.load(Ordering::Relaxed),
                    fmt_mmss(s.position()),
                    s.volume(),
                    s.eq_enabled(),
                    s.eq_gains_cdb.each_ref().map(|g| g.load(Ordering::Relaxed)),
                );
                self.refresh(); // re-print the panel with the dump
            }
            Command::Quit => {
                log_info!("BROKER", "quit — shutting decoder down (kills yt-dlp child)");
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
                self.track = Some(TrackMeta { source, title, duration, sample_rate, channels });
                self.shared.set_sample_rate(sample_rate);
                self.shared.reset_playhead();
                // Step 3: the cpal stream is (re)started here, not before.
                self.set_phase(Phase::Playing, "stream opened");
            }
            Status::Finished => {
                log_info!("BROKER", "track finished");
                self.track = None;
                self.shared.reset_playhead();
                self.set_phase(Phase::Stopped, "track finished");
            }
            Status::Failed { source, reason } => {
                log_error!("BROKER", "failed to open {source}: {reason}");
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
        if let Err(e) = self.decoder_tx.send(cmd) {
            log_error!("BROKER", "decoder channel dead: {e}");
        }
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

fn row(content: &str) -> String {
    let mut s = String::with_capacity(32);
    s.push('|');
    s.push(' ');
    let mut w = 0;
    for c in content.chars() {
        if w >= 28 {
            break;
        }
        s.push(c);
        w += 1;
    }
    for _ in w..28 {
        s.push(' ');
    }
    s.push(' ');
    s.push('|');
    s
}

fn band_row(labels: &[&str]) -> String {
    labels.iter().map(|l| format!("{l:>4}")).collect()
}

fn gain_row(shared: &SharedState, start: usize) -> String {
    (start..start + 5)
        .map(|b| format!("{:+4.0}", shared.eq_gain_db(b)))
        .collect()
}

/// 8 rows, 32 columns — sized for the 200 px floating window in step 2.
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
    p.push_str("+------------------------------+\n");
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
    p.push('\n');
    p.push_str("+------------------------------+");
    p
}

#[cfg(test)]
mod tests {
    use super::*;

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
