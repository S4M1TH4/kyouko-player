//! Audio pipeline — the "voice" of the echo.
//!
//! ```text
//!   Source::File    → std::fs::File ─┐
//!   Source::Youtube → yt-dlp stdout ─┴→ symphonia probe → packets → f32
//!         → 10-band biquad EQ at source rate (coefficients rebuilt ONLY when eq_dirty)
//!         → volume scale
//!         → linear resampler (only when source rate ≠ device rate)
//!         → channel mix (only when source ≠ device layout)
//!         → pooled AudioChunk (4096 frames, generation-stamped) → bounded PCM ring
//!
//!   cpal/WASAPI callback (event-driven, OS-scheduled):
//!         try_recv chunk → copy/convert → recycle block; never allocates or blocks
//! ```
//!
//! Zero-poll contract of the decoder thread:
//! * idle → parks in `cmds.recv()`
//! * playing → one blocking source read per packet (~40/s of real work), a
//!   `try_recv` command check per packet, and — only when the chunk channel
//!   is full — parks in `select!` between the chunk channel and the command
//!   channel. It never spins, sleeps, or busy-waits.
//! * end of track → parks in `select!` waiting for the output callback to
//!   drain the buffered tail, so `Finished` never chops audio off the end.
//!
//! Orphan-proof yt-dlp lifecycle: the child lives in a shared slot that the
//! track's `ChildGuard` kills+reaps on EVERY teardown path (eof, stop, skip,
//! error, shutdown). `AudioControls::interrupt()` kills it from outside for
//! the one case the decoder can't handle itself: a network-stalled pipe read
//! that would otherwise defer the next command indefinitely.

use std::cell::Cell;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::process::{Child, Stdio};
#[cfg(test)]
use std::process::Command;
use std::sync::{Arc, Mutex, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, select, Receiver, Sender, TryRecvError, TrySendError};

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatReader, Packet, SeekMode, SeekTo, Track};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};
use symphonia::default::get_probe;

use crate::broker::{
    DecoderCmd, SharedState, Source, Status, CHUNK_FRAMES, EQ_BANDS,
    EQ_BAND_HZ,
};
use crate::log_error;
use crate::stream_buffer::StreamBuffer;
use crate::{log_info, log_warn, log_debug};

/// One decoded, EQ'd, volume-applied, resampled block of interleaved f32
/// samples at the DEVICE rate. Allocated once in the pool, then recycled.
/// `generation` stamps which load/seek produced it; stale chunks return to
/// the pool without ever deallocating on the real-time thread.
pub struct AudioChunk {
    /// Interleaved f32 at the DEVICE rate and layout.
    pub data: Box<[f32]>,
    len: usize,
    pub generation: u64,
}

impl AudioChunk {
    fn samples(&self) -> &[f32] { &self.data[..self.len] }
}

const BUFFER_MILLIS: usize = 1500;
const PREFILL_MILLIS: usize = 500;

fn buffer_depth(rate: u32) -> usize {
    (rate as usize * BUFFER_MILLIS / 1000).div_ceil(CHUNK_FRAMES).max(2)
}

struct BufferState {
    ready_generation: AtomicU64,
    eof_generation: AtomicU64,
    underrun_frames: AtomicU64,
}

/// A fixed pool travels through two bounded rings. Only the decoder fills
/// blocks; the callback returns them to the free ring instead of freeing them.
struct ChunkSender {
    ready: Sender<AudioChunk>,
    discard: Receiver<AudioChunk>,
    free: Receiver<AudioChunk>,
    recycle: Sender<AudioChunk>,
    sink_gone: Receiver<()>,
    state: Arc<BufferState>,
    prefill_samples: usize,
    priming: Cell<(u64, usize)>,
}

pub struct ChunkReceiver {
    queue: Receiver<AudioChunk>,
    recycle: Sender<AudioChunk>,
    // Keep recycling connected until the callback itself is destroyed, even
    // after decoder shutdown. No buffer can be dropped by a failed try_send.
    _free_guard: Receiver<AudioChunk>,
    _alive: Sender<()>,
    state: Arc<BufferState>,
}

fn chunk_channel(rate: u32, channels: usize, depth: usize) -> (ChunkSender, ChunkReceiver) {
    let (ready, queue) = bounded(depth);
    let (recycle, free) = bounded(depth + 1);
    let (alive, sink_gone) = bounded(0);
    for _ in 0..=depth {
        recycle.send(AudioChunk {
            data: vec![0.0; CHUNK_FRAMES * channels].into_boxed_slice(),
            len: 0, generation: u64::MAX,
        }).unwrap_or_else(|_| unreachable!("pool initialization"));
    }
    let state = Arc::new(BufferState {
        ready_generation: AtomicU64::new(u64::MAX),
        eof_generation: AtomicU64::new(u64::MAX),
        underrun_frames: AtomicU64::new(0),
    });
    (ChunkSender {
        ready, discard: queue.clone(), free: free.clone(), recycle: recycle.clone(), sink_gone,
        state: state.clone(),
        prefill_samples: (rate as usize * PREFILL_MILLIS / 1000 * channels)
            .min(depth * CHUNK_FRAMES * channels),
        priming: Cell::new((u64::MAX, 0)),
    }, ChunkReceiver { queue, recycle, _free_guard: free, _alive: alive, state })
}

impl ChunkSender {
    fn begin(&self, generation: u64) {
        self.state.ready_generation.store(u64::MAX, Ordering::Release);
        self.state.eof_generation.store(u64::MAX, Ordering::Release);
        self.priming.set((generation, 0));
        // The single producer calls this before publishing a new generation.
        // Reclaim old queued audio here, including when WASAPI is paused.
        while let Ok(chunk) = self.discard.try_recv() { recycle(&self.recycle, chunk); }
    }

    fn published(&self, generation: u64, samples: usize) {
        let (previous, count) = self.priming.get();
        let count = if previous == generation { count + samples } else { samples };
        self.priming.set((generation, count.min(self.prefill_samples)));
        if count >= self.prefill_samples {
            self.state.ready_generation.store(generation, Ordering::Release);
        }
    }

    fn finish(&self, generation: u64) {
        self.state.eof_generation.store(generation, Ordering::Release);
        // Short tracks must start even when they cannot fill the watermark.
        self.state.ready_generation.store(generation, Ordering::Release);
    }
}

fn recycle(sender: &Sender<AudioChunk>, chunk: AudioChunk) {
    // There are exactly depth+1 buffers and depth+1 free slots. Owning this
    // buffer proves at least one slot is free. A receiver guard prevents
    // disconnection. This path neither allocates nor deallocates.
    sender.try_send(chunk).unwrap_or_else(|_| unreachable!("PCM pool invariant"));
}

/// Kill switch for external source processes. Clonable handle handed to the
/// broker link; `interrupt()` tears down whatever yt-dlp child is currently
/// feeding the decoder, unblocking a stalled pipe read with EOF.
#[derive(Clone)]
pub struct AudioControls {
    child_slot: Arc<Mutex<Option<Child>>>,
    buffer_slot: BufferSlot,
}

type BufferSlot = Arc<Mutex<Option<Weak<StreamBuffer>>>>;

impl AudioControls {
    /// Kill the active yt-dlp child, if any. Safe to call any time; it is a
    /// no-op while playing local files. Idempotent.
    pub fn interrupt(&self) {
        if let Some(buffer) = self.buffer_slot.lock().unwrap_or_else(|e| e.into_inner())
            .as_ref().and_then(Weak::upgrade)
        {
            buffer.cancel();
        }
        if let Ok(mut slot) = self.child_slot.lock() {
            if let Some(child) = slot.as_mut() {
                log_warn!("AUDIO", "interrupt: killing external source process");
                let _ = child.kill();
            }
        }
    }

    pub fn hook(self) -> Arc<dyn Fn() + Send + Sync> {
        Arc::new(move || self.interrupt())
    }
}

pub struct AudioHandles {
    pub chunk_rx: ChunkReceiver,
    /// Signals (per generation) when the output callback has drained all
    /// buffered audio — used at end-of-track so the tail plays out fully.
    pub drained_tx: Sender<u64>,
    pub decoder: JoinHandle<()>,
    pub controls: AudioControls,
}

pub fn spawn(
    shared: Arc<SharedState>,
    cmds: Receiver<DecoderCmd>,
    status_tx: Sender<Status>,
) -> AudioHandles {
    let rate = match shared.sample_rate() { 0 => 48_000, n => n };
    let channels = match shared.device_channels() { 0 => 2, n => n as usize };
    let (chunk_tx, chunk_rx) = chunk_channel(rate, channels, buffer_depth(rate));
    let (drained_tx, drained_rx) = bounded::<u64>(4);
    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let buffer_slot: BufferSlot = Arc::new(Mutex::new(None));
    let decoder = thread::Builder::new()
        .name("kyouko-decoder".into())
        .spawn({
            let child_slot = Arc::clone(&child_slot);
            let buffer_slot = buffer_slot.clone();
            move || run_decoder(shared, cmds, status_tx, chunk_tx, drained_rx, child_slot, buffer_slot)
        })
        .expect("spawn decoder thread");
    AudioHandles { chunk_rx, drained_tx, decoder, controls: AudioControls { child_slot, buffer_slot } }
}

// ── Output stream (owned by the main thread; WASAPI event-driven) ───────────

/// Owns the cpal stream and mirrors the playback phase onto WASAPI:
/// `set_playing(false)` stops the audio engine entirely — the paused player
/// has zero callbacks, zero wake-ups, zero CPU.
pub struct AudioOut {
    stream: Option<cpal::Stream>,
    playing: bool,
}

impl AudioOut {
    pub fn new(stream: Option<cpal::Stream>) -> Self {
        Self { stream, playing: false }
    }

    pub fn set_playing(&mut self, on: bool) {
        if on == self.playing {
            return;
        }
        use cpal::traits::StreamTrait;
        match (&self.stream, on) {
            (Some(s), true) => {
                if let Err(e) = s.play() {
                    log_error!("AUDIO", "stream.play failed: {e}");
                }
            }
            (Some(s), false) => {
                if let Err(e) = s.pause() {
                    log_error!("AUDIO", "stream.pause failed: {e}");
                }
            }
            (None, _) => {}
        }
        self.playing = on;
        log_info!(
            "AUDIO",
            "output {}",
            if on { "running" } else { "paused — zero CPU until resumed" }
        );
    }
}

/// Select the native device layout before the decoder/pool starts. The UI
/// later plays/pauses its stream with AudioOut on phase transitions.
pub fn output_device(shared: &SharedState) -> Result<(cpal::Device, cpal::SupportedStreamConfig), String> {
    use cpal::traits::{DeviceTrait, HostTrait};

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no default output device".to_string())?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("default output config: {e}"))?;
    let device_rate = supported.sample_rate().0;
    let device_channels = supported.channels().max(1);
    shared.set_sample_rate(device_rate);
    shared.set_device_channels(device_channels as u32);
    let name = device.name().unwrap_or_else(|_| "<unnamed>".into());
    log_info!(
        "AUDIO",
        "output: {name} — {device_rate} Hz, {device_channels} ch, {:?}",
        supported.sample_format()
    );
    Ok((device, supported))
}

pub fn open_output_stream(
    device: cpal::Device,
    supported: cpal::SupportedStreamConfig,
    chunk_rx: ChunkReceiver,
    drained_tx: Sender<u64>,
    shared: Arc<SharedState>,
) -> Result<cpal::Stream, String> {
    use cpal::traits::DeviceTrait;
    let device_channels = supported.channels().max(1);

    let config = cpal::StreamConfig {
        channels: device_channels,
        sample_rate: supported.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    let err_cb = move |err| log_warn!("AUDIO", "output stream error: {err}");

    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            &config,
            popper::<f32>(chunk_rx, drained_tx, Arc::clone(&shared), device_channels),
            err_cb,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream(
            &config,
            popper::<i16>(chunk_rx, drained_tx, Arc::clone(&shared), device_channels),
            err_cb,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_output_stream(
            &config,
            popper::<u16>(chunk_rx, drained_tx, Arc::clone(&shared), device_channels),
            err_cb,
            None,
        ),
        other => return Err(format!("unsupported device sample format {other:?}")),
    }
    .map_err(|e| format!("build output stream: {e}"))?;

    // Stay silent until the first Playing phase; tolerate "not yet running".
    use cpal::traits::StreamTrait;
    let _ = stream.pause();
    Ok(stream)
}

/// Conversion is direct into the device's slice: integer devices need no
/// temporary Vec, even when WASAPI changes the callback size.
trait OutputSample: Copy + Send + 'static {
    fn from_pcm(sample: f32) -> Self;
}
impl OutputSample for f32 {
    fn from_pcm(sample: f32) -> Self { sample }
}
impl OutputSample for i16 {
    fn from_pcm(sample: f32) -> Self { (sample.clamp(-1.0, 1.0) * 32767.0) as i16 }
}
impl OutputSample for u16 {
    fn from_pcm(sample: f32) -> Self { ((sample.clamp(-1.0, 1.0) + 1.0) * 32767.5).round() as u16 }
}

struct PcmOutput {
    chunks: ChunkReceiver,
    current: Option<AudioChunk>,
    pos: usize,
    started: u64,
}

impl PcmOutput {
    fn new(chunks: ChunkReceiver) -> Self {
        Self { chunks, current: None, pos: 0, started: u64::MAX }
    }

    fn render<T: OutputSample>(&mut self, data: &mut [T], shared: &SharedState, channels: usize) {
        let generation = shared.generation();
        let mut written = 0;
        while written < data.len() {
            // A load/seek may race this callback. Leave freshly queued audio
            // for the next invocation instead of discarding its new generation
            // using this invocation's older snapshot.
            if shared.generation() != generation { break; }
            if self.current.as_ref().is_some_and(|chunk| chunk.generation != generation || self.pos >= chunk.len) {
                recycle(&self.chunks.recycle, self.current.take().unwrap());
                self.pos = 0;
            }
            if self.current.is_none() {
                match self.chunks.queue.try_recv() {
                    Ok(chunk) => { self.current = Some(chunk); continue; }
                    Err(_) => break,
                }
            }
            // Consume stale generations above even during priming, so a full
            // old queue cannot prevent a newly loaded/seeked source filling.
            if self.chunks.state.ready_generation.load(Ordering::Acquire) != generation { break; }
            self.started = generation;
            let chunk = self.current.as_ref().unwrap();
            let n = (chunk.len - self.pos).min(data.len() - written);
            for (dst, &src) in data[written..written + n].iter_mut().zip(&chunk.samples()[self.pos..self.pos + n]) {
                *dst = T::from_pcm(src);
            }
            self.pos += n;
            written += n;
        }
        data[written..].fill(T::from_pcm(0.0));
        if shared.generation() != generation { return; }
        // Silence while priming or starved must not move the track playhead.
        shared.add_frames_played((written / channels) as u64);
        let eof = self.chunks.state.eof_generation.load(Ordering::Acquire) == generation;
        if written < data.len() && self.started == generation && !eof {
            self.chunks.state.underrun_frames.fetch_add(((data.len() - written) / channels) as u64, Ordering::Relaxed);
        }
    }

    fn drained(&self, generation: u64) -> bool {
        self.chunks.state.eof_generation.load(Ordering::Acquire) == generation
            && self.current.as_ref().is_none_or(|chunk| chunk.generation != generation || self.pos >= chunk.len)
            && self.chunks.queue.is_empty()
    }
}

/// No I/O, heap allocation/free, logging or state locks. All sample storage
/// was allocated before playback; only bounded nonblocking ring operations,
/// conversion and atomic playhead/diagnostic updates occur here.
fn popper<T: OutputSample>(
    chunk_rx: ChunkReceiver,
    drained_tx: Sender<u64>,
    shared: Arc<SharedState>,
    channels: u16,
) -> impl FnMut(&mut [T], &cpal::OutputCallbackInfo) + Send + 'static {
    let mut output = PcmOutput::new(chunk_rx);
    move |data, _info| {
        output.render(data, &shared, channels as usize);
        if output.drained(shared.generation()) { let _ = drained_tx.try_send(shared.generation()); }
    }
}

// ── Decoder thread ──────────────────────────────────────────────────────────

enum PlayExit {
    /// Natural end of stream; `Status::Finished` already sent.
    Eof,
    /// `Status::Failed` already sent.
    Failed,
    /// User stop; no status (the broker set the phase itself).
    Stopped,
    /// A Load arrived mid-play: the new source must be opened immediately.
    NewSource(Source),
    Shutdown,
    /// The chunk channel's consumer is gone (output stream failed to open).
    SinkGone,
}

fn run_decoder(
    shared: Arc<SharedState>,
    cmds: Receiver<DecoderCmd>,
    status_tx: Sender<Status>,
    chunk_tx: ChunkSender,
    drained_rx: Receiver<u64>,
    child_slot: Arc<Mutex<Option<Child>>>,
    buffer_slot: BufferSlot,
) {
    log_info!("DECODER", "online — parked on command channel (zero CPU)");
    let mut cache: Option<StreamCache> = None;
    loop {
        let first = match cmds.recv() {
            Ok(cmd) => {
                log_debug!("DECODER", "recv {cmd:?}");
                match cmd {
                    DecoderCmd::Load(s) => s,
                    // Nothing is open — a seek has nothing to move.
                    DecoderCmd::SeekRelative(_) => continue,
                    DecoderCmd::Stop => { cache = None; continue; }
                    DecoderCmd::Shutdown => break,
                }
            }
            Err(_) => break,
        };
        let mut next = Some(first);
        'tracks: while let Some(source) = next.take() {
            select_stream_cache(&mut cache, &source, shared.loop_enabled());
            let exit = play_source(
                &shared, &cmds, &status_tx, &chunk_tx, &drained_rx, &child_slot,
                &buffer_slot, &mut cache, source,
            );
            match exit {
                PlayExit::NewSource(s) => next = Some(s),
                PlayExit::Eof => {
                    if !shared.loop_enabled() { cache = None; }
                    break 'tracks;
                }
                PlayExit::Stopped | PlayExit::Failed => { cache = None; break 'tracks; }
                PlayExit::Shutdown | PlayExit::SinkGone => {
                    log_info!("DECODER", "offline");
                    return;
                }
            }
        }
    }
    log_info!("DECODER", "offline");
}

struct StreamCache {
    source: Source,
    buffer: Arc<StreamBuffer>,
    title: Option<String>,
}

/// Release the previous allocation before starting another source. Reuse
/// only a completed, successful buffer for the exact looping track.
fn select_stream_cache(cache: &mut Option<StreamCache>, source: &Source, looping: bool) {
    if !looping || cache.as_ref().is_some_and(|cached| cached.source != *source || !cached.buffer.complete()) {
        *cache = None;
    }
}

struct DownloadWorker {
    buffer: Arc<StreamBuffer>,
    worker: Option<JoinHandle<()>>,
    slot: BufferSlot,
}

impl DownloadWorker {
    fn start(mut input: impl Read + Send + 'static, slot: &BufferSlot) -> Result<Self, String> {
        let buffer = StreamBuffer::new();
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&buffer));
        let producer = buffer.clone();
        let worker = thread::Builder::new().name("kyouko-buffer".into()).spawn(move || {
            let result = (|| {
                let mut chunk = [0u8; 32 * 1024];
                loop {
                    let count = input.read(&mut chunk)?;
                    if count == 0 { return Ok(()); }
                    producer.append(&chunk[..count])?;
                }
            })();
            producer.finish(result);
        }).map_err(|e| format!("cannot start stream buffer: {e}"))?;
        Ok(Self { buffer, worker: Some(worker), slot: slot.clone() })
    }
}

impl Drop for DownloadWorker {
    fn drop(&mut self) {
        self.buffer.cancel();
        // ActiveTrack declares ChildGuard before this worker: the pipe's
        // process is killed/reaped before joining a blocked producer.
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Everything needed to pull decoded audio from one source. Dropping it kills
/// and reaps any yt-dlp child — the orphan-proof guarantee.
struct ActiveTrack {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    sample_rate: u32,
    time_base: Option<TimeBase>,
    channels: usize,
    /// Container seek support; streams read from a seekable RAM reader.
    seekable: bool,
    duration: Option<Duration>,
    title: Option<String>,
    /// yt-dlp's stderr tail, kept mid-track so decode failures show why.
    err_tail: Option<Arc<Mutex<Vec<u8>>>>,
    _child: Option<ChildGuard>,
    // Drop order is important: kill the child, then join its pipe reader.
    _download: Option<DownloadWorker>,
    stream_buffer: Option<Arc<StreamBuffer>>,
    seek_packet: Option<Packet>,
    next_ts: u64,
}

impl ActiveTrack {
    /// Log the last lines of yt-dlp's stderr — the reason it died, if it died.
    fn log_child_stderr(&mut self) {
        if let Some(tail) = self.err_tail.take() {
            log_stderr_tail(&tail);
        }
    }
}

struct ChildGuard {
    slot: Arc<Mutex<Option<Child>>>,
}

impl ChildGuard {
    /// Pipe EOF can also mean yt-dlp exited with an HTTP/extraction error.
    /// Inspect without waiting under the lock: Stop/Skip must remain able
    /// to interrupt a child that is still finishing its output.
    fn eof_failure(&self) -> Option<String> {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_mut()?.try_wait() {
            Ok(Some(status)) if !status.success() => Some(format!("yt-dlp exited with {status}")),
            Err(error) => Some(format!("cannot inspect yt-dlp exit: {error}")),
            _ => None,
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.slot.lock() {
            if let Some(mut child) = slot.take() {
                let _ = child.kill(); // no-op if it already exited
                let _ = child.wait(); // reap
            }
        }
    }
}

/// Exact yt-dlp command line for a load. Pure so the playlist-index logic
/// (which governs crawling bounds) is unit-testable without spawning.
fn yt_dlp_args(format: &str, playlist_index: Option<usize>, url: &str) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let mut a: Vec<OsString> = Vec::new();
    match playlist_index {
        // Advancing playlist entry n: the list is the source of truth.
        Some(n) => {
            a.push("--playlist-items".into());
            a.push(n.to_string().into());
        }
        // Single video (or an untracked URL): --no-playlist alone, and a
        // --playlist-items 1 bound so a playlist-only URL can never crawl.
        None => {
            a.push("--no-playlist".into());
            a.push("--playlist-items".into());
            a.push("1".into());
        }
    }
    a.push("-f".into());
    a.push(format.into());
    a.push("-o".into());
    a.push("-".into());
    a.push(url.into());
    a
}

fn open_track(
    source: &Source, child_slot: &Arc<Mutex<Option<Child>>>,
    buffer_slot: &BufferSlot, cached: Option<Arc<StreamBuffer>>,
) -> Result<ActiveTrack, String> {
    let mut err_tail: Option<Arc<Mutex<Vec<u8>>>> = None;
    match open_track_inner(source, child_slot, buffer_slot, cached, &mut err_tail) {
        Ok(t) => Ok(t),
        Err(e) => {
            if let Some(tail) = err_tail {
                log_stderr_tail(&tail);
            }
            Err(e)
        }
    }
}

fn log_stderr_tail(tail: &Arc<Mutex<Vec<u8>>>) {
    let buf = tail.lock().unwrap_or_else(|e| e.into_inner());
    let text = String::from_utf8_lossy(&buf);
    // yt-dlp progress writes carriage returns, not newlines — the "last
    // line" is simply the tail of the text.
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(300);
    let last: String = chars[start..].iter().collect();
    let last = last.trim();
    if !last.is_empty() {
        log_warn!("AUDIO", "yt-dlp stderr: …{last}");
    }
}

fn open_track_inner(
    source: &Source,
    child_slot: &Arc<Mutex<Option<Child>>>,
    buffer_slot: &BufferSlot,
    cached: Option<Arc<StreamBuffer>>,
    err_tail_out: &mut Option<Arc<Mutex<Vec<u8>>>>,
) -> Result<ActiveTrack, String> {
    let mut download = None;
    let mut stream_buffer = cached;
    let (mss, child_guard): (MediaSourceStream, Option<ChildGuard>) = match source {
        Source::File(path) => {
            let file = File::open(path).map_err(|e| format!("open: {e}"))?;
            // MediaSourceStream does its own buffering (buffer_len option).
            let mss = MediaSourceStream::new(Box::new(file), Default::default());
            (mss, None)
        }
        Source::Youtube { .. } if stream_buffer.is_some() => {
            let reader = stream_buffer.as_ref().unwrap().reader();
            log_info!("DECODER", "replay from RAM — no title resolution or network download");
            (MediaSourceStream::new(Box::new(reader), Default::default()), None)
        }
        Source::Youtube { url, format, playlist_index } => {
            log_info!(
                "DECODER",
                "spawning yt-dlp -f {format} (playlist entry {:?})",
                playlist_index
            );
            let mut child = crate::process::command("yt-dlp")
                .args(yt_dlp_args(format, *playlist_index, url))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("yt-dlp spawn failed ({e}) — is yt-dlp in PATH?"))?;
            let stdout = child.stdout.take().expect("yt-dlp stdout piped");
            let stderr = child.stderr.take().expect("yt-dlp stderr piped");
            // Drain stderr on a transient thread; keep the tail for the
            // failure log. The thread dies at the next EOF (child exit/kill).
            let err_tail = Arc::new(Mutex::new(Vec::new()));
            {
                let err_tail = Arc::clone(&err_tail);
                thread::spawn(move || {
                    let mut chunk = [0u8; 1024];
                    let mut reader = stderr;
                    loop {
                        match reader.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let mut buf =
                                    err_tail.lock().unwrap_or_else(|e| e.into_inner());
                                if buf.len() < 8192 {
                                    buf.extend_from_slice(&chunk[..n]);
                                }
                            }
                        }
                    }
                });
            }
            let guard = ChildGuard { slot: Arc::clone(child_slot) };
            *child_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
            *err_tail_out = Some(err_tail);
            let producer = DownloadWorker::start(stdout, buffer_slot)?;
            let mss = MediaSourceStream::new(Box::new(producer.buffer.reader()), Default::default());
            stream_buffer = Some(producer.buffer.clone());
            download = Some(producer);
            (mss, Some(guard))
        }
    };

    let mut hint = Hint::new();
    if let Source::File(path) = source {
        if let Some(ext) = path.rsplit(['.', '/', '\\']).next() {
            hint.with_extension(ext);
        }
    }
    let seekable = mss.is_seekable() || stream_buffer.is_some();
    let probed = get_probe()
        .format(&hint, mss, &Default::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;
    let mut format = probed.format;

    let track: &Track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("no audio track in source")?;
    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(0);
    let time_base = track.codec_params.time_base;
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(0);
    if sample_rate == 0 {
        return Err(format!("track has no usable sample rate ({sample_rate} Hz)"));
    }
    // `channels == 0` is allowed: AAC in fragmented MP4 (yt-dlp stdout) only
    // exposes the layout once the first packet is decoded. play_source sends
    // Status::Opened with the decoder's authoritative spec at that point.
    let duration = track
        .codec_params
        .n_frames
        .zip(track.codec_params.time_base)
        .map(|(n, tb)| {
            let t = tb.calc_time(n);
            Duration::from_secs_f64(t.seconds as f64 + t.frac)
        });
    let decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &Default::default())
        .map_err(|e| format!("decoder: {e}"))?;
    // `track`'s borrow of `format` ended with the decoder construction above.
    let title = format
        .metadata()
        .current()
        .and_then(|rev| {
            rev.tags()
                .iter()
                .find(|t| t.key.eq_ignore_ascii_case("title"))
                .map(|t| t.value.to_string())
        });

    Ok(ActiveTrack {
        format,
        decoder,
        track_id,
        sample_rate,
        time_base,
        channels,
        seekable,
        duration,
        title,
        // The tail rides on the track so mid-stream decode failures can show
        // why yt-dlp died; on the success path it is simply dropped.
        err_tail: std::mem::take(err_tail_out),
        _child: child_guard,
        _download: download,
        stream_buffer,
        seek_packet: None,
        next_ts: 0,
    })
}

enum SendOutcome {
    Sent,
    Stopped,
    NewSource(Source),
    /// A seek arrived while parked waiting for chunk space.
    SeekRelative(f64),
    Shutdown,
    SinkGone,
}

/// Copy into a free block and publish with backpressure. Parks in `select!`
/// for a free block/ready slot OR a command, so pause never burns idle CPU.
fn send_chunk(
    chunk_tx: &ChunkSender,
    cmds: &Receiver<DecoderCmd>,
    samples: &[f32],
    generation: u64,
) -> SendOutcome {
    if matches!(chunk_tx.sink_gone.try_recv(), Err(TryRecvError::Disconnected)) {
        return SendOutcome::SinkGone;
    }
    // All allocation is done when the pool is created. While every block is
    // owned by the output, park on free space OR a decoder command.
    let mut chunk = select! {
        recv(chunk_tx.free) -> block => match block {
            Ok(block) => block,
            Err(_) => return SendOutcome::SinkGone,
        },
        recv(chunk_tx.sink_gone) -> _ => return SendOutcome::SinkGone,
        recv(cmds) -> msg => return send_command(msg),
    };
    chunk.data[..samples.len()].copy_from_slice(samples);
    chunk.len = samples.len();
    chunk.generation = generation;
    loop {
        match chunk_tx.ready.try_send(chunk) {
            Ok(()) => {
                chunk_tx.published(generation, samples.len());
                return SendOutcome::Sent;
            }
            Err(TrySendError::Full(c)) => {
                chunk = c;
                select! {
                    send(chunk_tx.ready, chunk) -> res => match res {
                        Ok(()) => {
                            chunk_tx.published(generation, samples.len());
                            return SendOutcome::Sent;
                        }
                        Err(_send_err) => return SendOutcome::SinkGone,
                    },
                    recv(cmds) -> msg => {
                        recycle(&chunk_tx.recycle, chunk);
                        return send_command(msg);
                    },
                    recv(chunk_tx.sink_gone) -> _ => return SendOutcome::SinkGone,
                }
            }
            Err(TrySendError::Disconnected(_)) => return SendOutcome::SinkGone,
        }
    }
}

fn send_command(msg: Result<DecoderCmd, crossbeam_channel::RecvError>) -> SendOutcome {
    match msg {
        Ok(DecoderCmd::Stop) => SendOutcome::Stopped,
        Ok(DecoderCmd::Shutdown) | Err(_) => SendOutcome::Shutdown,
        Ok(DecoderCmd::Load(source)) => SendOutcome::NewSource(source),
        Ok(DecoderCmd::SeekRelative(delta)) => SendOutcome::SeekRelative(delta),
    }
}

/// One line, trimmed, control-char-free, capped — source titles must never
/// smuggle newlines into the panel renderer.
fn clean_title(raw: &str) -> String {
    let line = raw.lines().next().unwrap_or("").trim();
    line.chars().take(120).collect()
}

/// Single-shot title resolution, run SEQUENTIALLY on the decoder thread
/// before the audio child is spawned — there is never more than one yt-dlp
/// process alive per load. The child lives in the shared ChildGuard slot, so
/// `AudioControls::interrupt()` (Stop / Skip / Shutdown) kills it mid-fetch;
/// the parked `read_to_string` unblocks with EOF and it is reaped at once.
/// Failure of any kind -> None (the panel keeps the URL; playback proceeds).
fn fetch_title_blocking(
    url: &str,
    format: &str,
    playlist_index: Option<usize>,
    child_slot: &Arc<Mutex<Option<Child>>>,
) -> Option<String> {
    // `-f {format}` alongside --print: Bilibili's stream ids differ from
    // YouTube's, and resolving against the same format the audio child will
    // use keeps the metadata pass honest about playability (the request
    // never downloads; it only prints).
    let mut args: Vec<std::ffi::OsString> =
        vec!["--print".into(), "title".into(), "-f".into(), format.into()];
    match playlist_index {
        Some(n) => {
            args.push("--playlist-items".into());
            args.push(n.to_string().into());
        }
        None => args.push("--no-playlist".into()),
    }
    args.push(url.into());
    let mut child = crate::process::command("yt-dlp")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    if let Ok(mut slot) = child_slot.lock() {
        *slot = Some(child);
    }
    log_info!("DECODER", "resolving title (single-shot, tracked)");
    let mut raw = String::new();
    let read_ok = std::io::BufReader::new(stdout).read_to_string(&mut raw).is_ok();
    // Reap regardless of how the read ended (natural exit or kill).
    if let Ok(mut slot) = child_slot.lock() {
        if let Some(mut c) = slot.take() {
            let _ = c.kill(); // no-op after natural exit
            let _ = c.wait();
        }
    }
    let title = if read_ok { clean_title(&raw) } else { String::new() };
    if title.is_empty() {
        log_warn!("DECODER", "title fetch failed — keeping URL");
        return None;
    }
    log_info!("DECODER", "title resolved: {title}");
    Some(title)
}

/// Seek the open track by `delta` seconds (clamped at 0). Local files use
/// symphonia's native container seek; streams seek through downloaded RAM
/// only, without HTTP requests. Returns true when the playhead moved. Resets codec
/// state, stale buffers, the chunk generation and the playhead so decoding
/// resumes seamlessly at the new position.
#[allow(clippy::too_many_arguments)]
fn seek_relative(
    track: &mut ActiveTrack,
    shared: &SharedState,
    delta: f64,
    pending: &mut Vec<f32>,
    eq: &mut EqChain,
    resampler: &mut Option<LinearResampler>,
    generation: &mut u64,
) -> bool {
    if !track.seekable {
        log_info!("DECODER", "container does not support seeking");
        return false;
    }
    if !delta.is_finite() { return false; }
    let previous = shared.position().as_secs_f64();
    let mut target = (previous + delta).max(0.0);
    if let Some(duration) = track.duration { target = target.min(duration.as_secs_f64()); }
    let buffer = track.stream_buffer.clone();
    let _downloaded_seek = buffer.as_ref().map(|buffer| buffer.downloaded_seek());
    let to = SeekTo::Time {
        time: Time { seconds: target as u64, frac: target.fract() },
        track_id: Some(track.track_id),
    };
    let device_rate = shared.sample_rate().max(1);
    let seek_result = track.format.seek(SeekMode::Coarse, to).and_then(|seeked| {
        // Some containers only update a sample offset during seek. Verify
        // its bytes are already available now, while RAM-only reads are
        // enforced, and keep that packet for the next decode iteration.
        let packet = track.format.next_packet()?;
        Ok((seeked, packet))
    });
    if seek_result.is_err() {
        let _ = track.format.seek(SeekMode::Coarse, SeekTo::TimeStamp {
            ts: track.next_ts, track_id: track.track_id,
        });
    }
    match seek_result {
        Ok((seeked, packet)) => {
            track.next_ts = packet.ts().saturating_add(packet.dur());
            track.seek_packet = Some(packet);
            let actual = track.time_base.map(|base| {
                let time = base.calc_time(seeked.actual_ts);
                time.seconds as f64 + time.frac
            }).unwrap_or(target);
            track.decoder.reset();
            pending.clear();
            eq.built = false; // biquad state is stale past the seek
            *resampler = None; // fractional carry is stale past the seek
            shared.bump_generation(); // in-flight stale chunks are discarded by the callback
            *generation = shared.generation(); // fresh chunks carry the new one
            shared.set_playhead_frames((actual * device_rate as f64).round() as u64);
            log_info!("DECODER", "seek {delta:+.1}s -> playhead {:.1}s (RAM for streams)", actual);
            true
        }
        Err(SymphoniaError::Unsupported(_)) => {
            log_info!("DECODER", "container does not support this seek");
            false
        }
        Err(e) => {
            log_warn!("DECODER", "seek failed: {e}");
            false
        }
    }
}

fn play_source(
    shared: &Arc<SharedState>,
    cmds: &Receiver<DecoderCmd>,
    status_tx: &Sender<Status>,
    chunk_tx: &ChunkSender,
    drained_rx: &Receiver<u64>,
    child_slot: &Arc<Mutex<Option<Child>>>,
    buffer_slot: &BufferSlot,
    cache: &mut Option<StreamCache>,
    source: Source,
) -> PlayExit {
    let mut generation = shared.generation();
    chunk_tx.begin(generation);
    let cached = cache
        .as_ref()
        .filter(|cached| cached.source == source && cached.buffer.complete());
    let replay_buffer = cached.map(|cached| cached.buffer.clone());
    // Single-shot, SEQUENTIAL title resolution: one yt-dlp at a time, ever.
    // The prefetch child lives in the same ChildGuard slot as the audio
    // child would, so Stop/Skip/Shutdown kill it mid-fetch (its parked read
    // unblocks with EOF) and it is reaped immediately.
    let mut resolved_title = if let Some(cached) = cached {
        cached.title.clone()
    } else {
        match &source {
            Source::Youtube {
                url,
                format,
                playlist_index,
                ..
            } => fetch_title_blocking(url, format, *playlist_index, child_slot),
            _ => None,
        }
    };
    // The prefetch may have been killed by an interrupt for a NEWER command
    // (Stop / Skip) — honor it here instead of spawning a doomed audio child.
    match cmds.try_recv() {
        Ok(DecoderCmd::Stop) => return PlayExit::Stopped,
        Ok(DecoderCmd::Shutdown) => return PlayExit::Shutdown,
        Ok(DecoderCmd::Load(s)) => return PlayExit::NewSource(s),
        Ok(DecoderCmd::SeekRelative(_)) | Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => return PlayExit::Shutdown,
    }
    let mut track = match open_track(&source, child_slot, buffer_slot, replay_buffer) {
        Ok(t) => t,
        Err(reason) => {
            log_error!("DECODER", "open failed: {reason}");
            let _ = status_tx.send(Status::Failed {
                source,
                reason,
                generation,
            });
            crate::ui::wake_status();
            return PlayExit::Failed;
        }
    };
    let replay_title = track.title.clone().or_else(|| resolved_title.clone());
    log_info!(
        "DECODER",
        "opened: {} Hz, {} ch{}, title {:?}",
        track.sample_rate,
        track.channels,
        track
            .duration
            .map(|d| format!(", {}", d.as_secs_f32() as u32))
            .unwrap_or_default(),
        track.title
    );

    // Device-side targets. `dst_channels` can't be finalized yet when the
    // container withholds the source layout (AAC-in-fMP4 via yt-dlp) — it is
    // resolved when Status::Opened fires on the first decoded packet below.
    let dst_rate = shared.sample_rate().max(1);
    let mut dst_channels = match shared.device_channels() {
        0 => track.channels as u32,
        n => n,
    } as usize;
    let mut chunk_samples = CHUNK_FRAMES * dst_channels;

    let mut eq = EqChain::new();
    let mut resampler: Option<LinearResampler> = None;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut pending: Vec<f32> = Vec::new();
    let mut rate_buf: Vec<f32> = Vec::new();
    let mut mix_buf: Vec<f32> = Vec::new();
    let mut bad_packets: u64 = 0;
    let mut opened = false;
    'decode: loop {
        let mut exit = PlayExit::Eof;
        let mut failure_reason = None;

        'play: loop {
            // Command check between packets: the loop only runs while doing real
            // work, so this is work-time checking, not idle polling.
            match cmds.try_recv() {
                Ok(DecoderCmd::Stop) => {
                    exit = PlayExit::Stopped;
                    break 'play;
                }
                Ok(DecoderCmd::Shutdown) => {
                    exit = PlayExit::Shutdown;
                    break 'play;
                }
                Ok(DecoderCmd::Load(s)) => {
                    exit = PlayExit::NewSource(s);
                    break 'play;
                }
                Ok(DecoderCmd::SeekRelative(delta)) => {
                    if seek_relative(
                        &mut track,
                        shared,
                        delta,
                        &mut pending,
                        &mut eq,
                        &mut resampler,
                        &mut generation,
                    ) {
                        chunk_tx.begin(generation);
                        let _ = status_tx.send(Status::Seeked);
                        crate::ui::wake_status();
                    }
                    continue;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    exit = PlayExit::Shutdown;
                    break 'play;
                }
            }

            let packet = match track
                .seek_packet
                .take()
                .map(Ok)
                .unwrap_or_else(|| track.format.next_packet())
            {
                Ok(p) => p,
                Err(SymphoniaError::IoError(ref e)) if e.kind() == ErrorKind::UnexpectedEof => {
                    break 'play; // natural end — exit stays Eof
                }
                Err(SymphoniaError::ResetRequired) => {
                    log_warn!(
                        "DECODER",
                        "stream reset — restarting through failure recovery"
                    );
                    failure_reason = Some("stream reset requires a fresh decoder".to_string());
                    exit = PlayExit::Failed;
                    break 'play;
                }
                Err(e) => {
                    log_error!("DECODER", "demux: {e}");
                    track.log_child_stderr();
                    exit = PlayExit::Failed;
                    break 'play;
                }
            };
            track.next_ts = packet.ts().saturating_add(packet.dur());
            if packet.track_id() != track.track_id {
                continue; // attached streams (cover art etc.)
            }

            let decoded = match track.decoder.decode(&packet) {
                Ok(d) => d,
                Err(SymphoniaError::DecodeError(_)) => {
                    bad_packets += 1;
                    if bad_packets <= 3 || bad_packets % 200 == 0 {
                        log_warn!("DECODER", "bad packet #{} — skipping", bad_packets);
                    }
                    continue;
                }
                Err(e) => {
                    log_error!("DECODER", "decode: {e}");
                    track.log_child_stderr();
                    exit = PlayExit::Failed;
                    break 'play;
                }
            };

            let spec = *decoded.spec();
            // First good packet: the decoder's spec is authoritative. Streams that
            // withheld their layout at open (AAC-in-fMP4) resolve here, and only
            // now does the broker hear about the track — before any chunk flows.
            if !opened {
                track.channels = spec.channels.count();
                if track.sample_rate == 0 {
                    track.sample_rate = spec.rate;
                }
                if track.channels == 0 {
                    log_error!("DECODER", "decoded audio has no channels");
                    exit = PlayExit::Failed;
                    break 'play;
                }
                dst_channels = match shared.device_channels() {
                    0 => track.channels,
                    n => n as usize,
                };
                chunk_samples = CHUNK_FRAMES * dst_channels;
                pending.reserve(chunk_samples * 2);
                // Container tags win (local files); the prefetched YouTube
                // title fills the gap.
                let _ = status_tx.send(Status::Opened {
                    source: source.clone(),
                    sample_rate: track.sample_rate,
                    channels: track.channels as u16,
                    title: track.title.take().or_else(|| resolved_title.take()),
                    duration: track.duration,
                });
                crate::ui::wake_status();
                opened = true;
            }
            let sb = sample_buf
                .get_or_insert_with(|| SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
            sb.copy_interleaved_ref(decoded);
            let frames = sb.len() / track.channels;
            if frames == 0 {
                continue;
            }

            // EQ at the source rate, before any resampling.
            if shared.eq_enabled() {
                if shared.take_eq_dirty() || !eq.built {
                    eq.rebuild(&shared.eq_gains(), track.sample_rate, track.channels);
                }
                eq.apply(
                    &mut sb.samples_mut()[..frames * track.channels],
                    track.channels,
                );
            }
            // Volume after EQ.
            let vol = shared.volume();
            if vol != 1.0 {
                for s in &mut sb.samples_mut()[..frames * track.channels] {
                    *s *= vol;
                }
            }

            // Resample only on rate mismatch; then mix to the device layout.
            rate_buf.clear();
            if track.sample_rate == dst_rate {
                rate_buf.extend_from_slice(&sb.samples()[..frames * track.channels]);
            } else {
                let rs = resampler.get_or_insert_with(|| {
                    LinearResampler::new(track.sample_rate, dst_rate, track.channels)
                });
                rs.process(
                    &sb.samples()[..frames * track.channels],
                    frames,
                    &mut rate_buf,
                );
            }
            if dst_channels != track.channels {
                mix_buf.clear();
                convert_channels(&rate_buf, track.channels, dst_channels, &mut mix_buf);
                pending.extend_from_slice(&mix_buf);
            } else {
                pending.extend_from_slice(&rate_buf);
            }

            // Emit fixed-size chunks; backpressure parks in send_chunk.
            while pending.len() >= chunk_samples {
                match send_chunk(chunk_tx, cmds, &pending[..chunk_samples], generation) {
                    SendOutcome::Sent => { pending.drain(..chunk_samples); }
                    SendOutcome::Stopped => {
                        exit = PlayExit::Stopped;
                        break 'play;
                    }
                    SendOutcome::SeekRelative(delta) => {
                        if seek_relative(
                            &mut track,
                            shared,
                            delta,
                            &mut pending,
                            &mut eq,
                            &mut resampler,
                            &mut generation,
                        ) {
                            chunk_tx.begin(generation);
                            let _ = status_tx.send(Status::Seeked);
                            crate::ui::wake_status();
                            continue 'play;
                        }
                        // A refused seek leaves pending samples intact.
                    }
                    SendOutcome::NewSource(s) => {
                        exit = PlayExit::NewSource(s);
                        break 'play;
                    }
                    SendOutcome::Shutdown => {
                        exit = PlayExit::Shutdown;
                        break 'play;
                    }
                    SendOutcome::SinkGone => {
                        log_error!("DECODER", "audio sink gone — dropping track");
                        exit = PlayExit::SinkGone;
                        break 'play;
                    }
                }
            }
        }

        if matches!(exit, PlayExit::Eof) {
            failure_reason = track
                ._child
                .as_ref()
                .and_then(ChildGuard::eof_failure)
                .or_else(|| (!opened).then(|| "source ended without decodable audio".to_string()));
            if let Some(reason) = &failure_reason {
                log_error!("DECODER", "playback failed: {reason}");
                track.log_child_stderr();
                exit = PlayExit::Failed;
            }
        }

        // EOF: flush the short tail chunk so nothing is lost, then wait for the
        // output callback to hand everything to the device before Finished.
        if matches!(exit, PlayExit::Eof) && !pending.is_empty() && pending.len() < chunk_samples {
            loop {
                match send_chunk(chunk_tx, cmds, &pending, generation) {
                    SendOutcome::Sent => { pending.clear(); break; }
                    SendOutcome::Stopped => {
                        exit = PlayExit::Stopped;
                        break;
                    }
                    SendOutcome::SeekRelative(d) => {
                        if seek_relative(
                            &mut track,
                            shared,
                            d,
                            &mut pending,
                            &mut eq,
                            &mut resampler,
                            &mut generation,
                        ) {
                            chunk_tx.begin(generation);
                            let _ = status_tx.send(Status::Seeked);
                            crate::ui::wake_status();
                            continue 'decode;
                        }
                    }
                    SendOutcome::NewSource(s) => {
                        exit = PlayExit::NewSource(s);
                        break;
                    }
                    SendOutcome::Shutdown => {
                        exit = PlayExit::Shutdown;
                        break;
                    }
                    SendOutcome::SinkGone => {
                        exit = PlayExit::SinkGone;
                        break;
                    }
                }
            }
        }
        match exit {
            PlayExit::Eof => {
                // Arm EOF after clearing old signals; mid-track starvation is
                // never mistaken for successful completion.
                while drained_rx.try_recv().is_ok() {}
                chunk_tx.finish(generation);
                loop {
                    select! {
                        recv(drained_rx) -> g => match g {
                            Ok(g) if g == generation => {
                                // A child may close stdout just before its exit
                                // code becomes available. Check again after the
                                // buffered tail drains rather than reporting a
                                // failed HTTP stream as successful completion.
                                if let Some(reason) = track._child.as_ref().and_then(ChildGuard::eof_failure) {
                                    track.log_child_stderr();
                                    drop(track);
                                    let _ = status_tx.send(Status::Failed { source, reason, generation });
                                    crate::ui::wake_status();
                                    return PlayExit::Failed;
                                }
                                // Store the same allocation before notifying the
                                // broker; its repeat Load will reopen at byte 0.
                                if shared.loop_enabled() {
                                    if let Some(buffer) = track.stream_buffer.as_ref().filter(|buffer| buffer.complete()) {
                                        *cache = Some(StreamCache {
                                            source: source.clone(), buffer: buffer.clone(), title: replay_title.clone(),
                                        });
                                    }
                                }
                                let _ = status_tx.send(Status::Finished);
                                crate::ui::wake_status();
                                return PlayExit::Eof;
                            }
                            _ => continue,
                        },
                        recv(cmds) -> msg => match msg {
                            Ok(DecoderCmd::Stop) => return PlayExit::Stopped,
                            Ok(DecoderCmd::Shutdown) => return PlayExit::Shutdown,
                            Ok(DecoderCmd::Load(s)) => return PlayExit::NewSource(s),
                            Ok(DecoderCmd::SeekRelative(delta)) => {
                                if seek_relative(&mut track, shared, delta, &mut pending, &mut eq, &mut resampler, &mut generation) {
                                    chunk_tx.begin(generation);
                                    let _ = status_tx.send(Status::Seeked);
                                    crate::ui::wake_status();
                                    continue 'decode;
                                }
                            }
                            Err(_) => return PlayExit::Shutdown,
                        },
                    }
                }
            }
            PlayExit::Failed => {
                let reason = failure_reason.unwrap_or_else(|| "decode failed".into());
                drop(track); // reap the old child before the broker can retry
                let _ = status_tx.send(Status::Failed {
                    source,
                    reason,
                    generation,
                });
                crate::ui::wake_status();
                return PlayExit::Failed;
            }
            other => return other,
        }
    }
}

// ── DSP: 10-band peaking biquads, linear resampler, channel mix ─────────────

fn convert_channels(input: &[f32], src_ch: usize, dst_ch: usize, out: &mut Vec<f32>) {
    if src_ch == dst_ch {
        out.extend_from_slice(input);
        return;
    }
    for frame in input.chunks_exact(src_ch) {
        match (src_ch, dst_ch) {
            (1, 2) => {
                out.push(frame[0]);
                out.push(frame[0]);
            }
            (2, 1) => out.push((frame[0] + frame[1]) * 0.5),
            _ => {
                // Generic fold: downmix averages, upmix replicates.
                if dst_ch < src_ch {
                    let group = src_ch as f32 / dst_ch as f32;
                    for c in 0..dst_ch {
                        let lo = (c as f32 * group).floor() as usize;
                        let hi = (((c + 1) as f32 * group).ceil() as usize).min(src_ch);
                        let sum: f32 = frame[lo..hi.max(lo + 1)].iter().sum();
                        out.push(sum / (hi.max(lo + 1) - lo) as f32);
                    }
                } else {
                    for c in 0..dst_ch {
                        out.push(frame[c % src_ch]);
                    }
                }
            }
        }
    }
}

/// One RBJ peaking biquad. Coefficients are f64; state is per channel.
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    /// [x1, x2, y1, y2] per channel.
    z: Vec<[f64; 4]>,
    /// A 0 dB peaking band is an exact passthrough — skip it entirely.
    active: bool,
}

impl Biquad {
    fn set(&mut self, freq_hz: u32, gain_db: f32, sample_rate: u32, channels: usize) {
        self.active = gain_db.abs() >= 0.05;
        let a = 10f64.powf(gain_db as f64 / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * freq_hz as f64 / sample_rate as f64;
        let (sw, cw) = w0.sin_cos();
        let alpha = sw / 2.2; // Q = 1.1 — gentle shelves, bands don't fight
        let a0 = 1.0 + alpha / a;
        self.b0 = (1.0 + alpha * a) / a0;
        self.b1 = (-2.0 * cw) / a0;
        self.b2 = (1.0 - alpha * a) / a0;
        self.a1 = (-2.0 * cw) / a0;
        self.a2 = (1.0 - alpha / a) / a0;
        if self.z.len() != channels {
            self.z = vec![[0.0; 4]; channels];
        } else {
            for z in &mut self.z {
                *z = [0.0; 4];
            }
        }
    }

    #[inline]
    fn process(&mut self, ch: usize, x: f32) -> f32 {
        let z = &mut self.z[ch];
        let xv = x as f64;
        let y = self.b0 * xv + self.b1 * z[0] + self.b2 * z[1] - self.a1 * z[2] - self.a2 * z[3];
        *z = [xv, z[0], y, z[2]];
        y as f32
    }
}

/// 10-band cascade. `apply` is the only per-sample cost of the whole player's
/// DSP: ~20 multiply-adds per sample for stereo with all bands active.
struct EqChain {
    bands: Vec<Biquad>,
    built: bool,
}

impl EqChain {
    fn new() -> Self {
        Self { bands: (0..EQ_BANDS).map(|_| Biquad { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0, z: Vec::new(), active: false }).collect(), built: false }
    }

    /// Rebuild coefficients — called only when `eq_dirty` flips (a command
    /// touched a gain) or on track open. f64 math, then straight back to f32.
    fn rebuild(&mut self, gains_db: &[f32; EQ_BANDS], sample_rate: u32, channels: usize) {
        for (band, &f0) in EQ_BAND_HZ.iter().enumerate() {
            self.bands[band].set(f0, gains_db[band], sample_rate, channels);
        }
        self.built = true;
    }

    fn apply(&mut self, buf: &mut [f32], channels: usize) {
        for band in &mut self.bands {
            if !band.active {
                continue;
            }
            for ch in 0..channels {
                for i in (ch..buf.len()).step_by(channels) {
                    buf[i] = band.process(ch, buf[i]);
                }
            }
        }
    }
}

/// Streaming linear resampler. Carries the last source frame between calls,
/// so arbitrary packet sizes chain seamlessly. Quality is honest linear
/// interpolation — transparent for playback SRC (44.1↔48 k).
struct LinearResampler {
    step: f64, // source frames per output frame
    /// Fractional source-frame position relative to `prev` (0.0 = exactly prev).
    t: f64,
    prev: Vec<f32>,
    channels: usize,
}

impl LinearResampler {
    fn new(src_rate: u32, dst_rate: u32, channels: usize) -> Self {
        Self {
            step: src_rate as f64 / dst_rate as f64,
            t: 0.0,
            prev: vec![0.0; channels],
            channels,
        }
    }

    /// Consumes `input` (interleaved, `in_frames` frames) and appends
    /// resampled frames to `out`.
    fn process(&mut self, input: &[f32], in_frames: usize, out: &mut Vec<f32>) {
        let ch = self.channels;
        loop {
            let k = self.t as usize; // floor
            if k >= in_frames {
                // Everything needed is beyond this packet: carry the tail.
                self.prev.copy_from_slice(&input[(in_frames - 1) * ch..in_frames * ch]);
                self.t -= in_frames as f64;
                return;
            }
            let frac = (self.t - k as f64) as f32;
            for c in 0..ch {
                let a = if k == 0 { self.prev[c] } else { input[(k - 1) * ch + c] };
                let b = input[k * ch + c];
                out.push(a + (b - a) * frac);
            }
            self.t += self.step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_pcm(sender: &ChunkSender, cmds: &Receiver<DecoderCmd>, samples: &[f32], generation: u64) {
        assert!(matches!(send_chunk(sender, cmds, samples, generation), SendOutcome::Sent));
    }

    #[test]
    fn pcm_ring_primes_half_a_second_and_absorbs_a_half_second_producer_stall() {
        for rate in [44_100, 48_000, 96_000, 192_000] {
            let shared = SharedState::new();
            shared.set_sample_rate(rate);
            let depth = buffer_depth(rate);
            assert!(depth * CHUNK_FRAMES >= rate as usize * BUFFER_MILLIS / 1000);
            let (sender, receiver) = chunk_channel(rate, 2, depth);
            sender.begin(shared.generation());
            let (_cmd_tx, cmd_rx) = bounded(1);
            let mut output = PcmOutput::new(receiver);
            let samples = vec![0.25; CHUNK_FRAMES * 2];
            let mut device = vec![1.0; rate as usize / 100 * 2]; // 10 ms
            push_pcm(&sender, &cmd_rx, &samples, shared.generation());
            output.render(&mut device, &shared, 2);
            assert!(device.iter().all(|&sample| sample == 0.0));
            assert_eq!(shared.position(), Duration::ZERO);
            for _ in 1..depth { push_pcm(&sender, &cmd_rx, &samples, shared.generation()); }
            // No producer activity at all during the next 500 ms of demand.
            for _ in 0..50 {
                output.render(&mut device, &shared, 2);
                assert!(device.iter().all(|&sample| sample == 0.25));
            }
            assert_eq!(sender.state.underrun_frames.load(Ordering::Relaxed), 0);
            assert_eq!(shared.position(), Duration::from_millis(500));
        }
    }

    #[test]
    fn pcm_ring_recycles_the_same_allocations_through_seek_and_changing_callback_sizes() {
        let shared = SharedState::new();
        shared.set_sample_rate(48_000);
        let (sender, receiver) = chunk_channel(48_000, 2, 2);
        let pointers: Vec<_> = sender.free.try_iter().collect();
        let addresses: Vec<_> = pointers.iter().map(|chunk| chunk.data.as_ptr() as usize).collect();
        for chunk in pointers { recycle(&sender.recycle, chunk); }
        let (_cmd_tx, cmd_rx) = bounded(1);
        let mut output = PcmOutput::new(receiver);
        let samples = vec![0.5; CHUNK_FRAMES * 2];
        for _ in 0..20 {
            sender.begin(shared.generation());
            push_pcm(&sender, &cmd_rx, &samples, shared.generation());
            sender.finish(shared.generation());
            let mut short = [0i16; 960];
            output.render(&mut short, &shared, 2);
            assert!(short.iter().all(|&sample| sample == 16383));
            // Seek/skip invalidates the partial block; an empty callback
            // waits silently, then a short new tail renders without old audio.
            shared.bump_generation();
            shared.reset_playhead();
            sender.begin(shared.generation());
            let mut large = [0u16; CHUNK_FRAMES * 4];
            output.render(&mut large, &shared, 2);
            assert!(large.iter().all(|&sample| sample == 32768));
            push_pcm(&sender, &cmd_rx, &[0.25; 100], shared.generation());
            sender.finish(shared.generation());
            output.render(&mut large, &shared, 2);
            assert!(large[..100].iter().all(|&sample| sample == 40959));
            assert!(large[100..].iter().all(|&sample| sample == 32768));
            assert!(output.drained(shared.generation()));
            assert_eq!(shared.position(), Duration::from_secs_f64(50.0 / 48_000.0));
            output.render(&mut large, &shared, 2); // return the consumed tail
            shared.bump_generation();
        }
        assert_eq!(sender.free.len(), 3);
        for chunk in sender.free.try_iter() {
            assert!(addresses.contains(&(chunk.data.as_ptr() as usize)), "all allocations are reused");
        }
    }

    #[test]
    fn pcm_ring_starvation_never_advances_time_or_reports_eof() {
        let shared = SharedState::new();
        shared.set_sample_rate(48_000);
        let (sender, receiver) = chunk_channel(48_000, 1, 2);
        let (_cmd_tx, cmd_rx) = bounded(1);
        let mut output = PcmOutput::new(receiver);
        // Fill the watermark and consume everything while producer is held.
        for _ in 0..2 { push_pcm(&sender, &cmd_rx, &[0.5; CHUNK_FRAMES], 0); }
        let mut data = [0.0; CHUNK_FRAMES * 2];
        output.render(&mut data, &shared, 1);
        let position = shared.position();
        output.render(&mut data, &shared, 1);
        assert_eq!(position, shared.position());
        assert!(!output.drained(0));
        assert_eq!(sender.state.underrun_frames.load(Ordering::Relaxed), data.len() as u64);
        sender.finish(0);
        assert!(output.drained(0));
    }

    thread_local! {
        static GENERATION_RACE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    }

    #[derive(Clone, Copy)]
    struct RacingSample(f32);
    impl OutputSample for RacingSample {
        fn from_pcm(sample: f32) -> Self {
            // Deterministically interleave a load while copying the last old
            // block, without timing assumptions or synthetic OS/UI input.
            let switch = GENERATION_RACE.with(|hook| hook.borrow_mut().take());
            if let Some(switch) = switch { switch(); }
            Self(sample)
        }
    }

    #[test]
    fn load_during_callback_keeps_new_audio_and_does_not_advance_its_playhead() {
        let shared = SharedState::new();
        shared.set_sample_rate(48_000);
        let (sender, receiver) = chunk_channel(48_000, 1, 2);
        let (_cmd_tx, cmd_rx) = bounded(1);
        push_pcm(&sender, &cmd_rx, &[0.5; 16], 0);
        sender.finish(0);
        let mut output = PcmOutput::new(receiver);
        let race_shared = shared.clone();
        GENERATION_RACE.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
            race_shared.bump_generation();
            race_shared.reset_playhead();
            sender.begin(1);
            push_pcm(&sender, &cmd_rx, &[0.25; 16], 1);
            sender.finish(1);
        })));
        let mut device = [RacingSample(1.0); 64];
        output.render(&mut device, &shared, 1);
        assert_eq!(shared.position(), Duration::ZERO);
        assert_eq!(output.chunks.queue.len(), 1, "new audio must survive the stale callback");
        output.render(&mut device, &shared, 1);
        assert!(device[..16].iter().all(|sample| sample.0 == 0.25));
        assert!(device[16..].iter().all(|sample| sample.0 == 0.0));
        assert_eq!(shared.position(), Duration::from_secs_f64(16.0 / 48_000.0));
    }

    #[test]
    fn full_pcm_ring_remains_interruptible_and_reclaims_stale_paused_audio() {
        let (sender, receiver) = chunk_channel(48_000, 1, 2);
        let (cmd_tx, cmd_rx) = bounded(1);
        for _ in 0..2 { push_pcm(&sender, &cmd_rx, &[0.5; CHUNK_FRAMES], 0); }
        cmd_tx.send(DecoderCmd::SeekRelative(5.0)).unwrap();
        assert!(matches!(send_chunk(&sender, &cmd_rx, &[0.5; CHUNK_FRAMES], 0), SendOutcome::SeekRelative(5.0)));
        assert_eq!(sender.free.len(), 1, "the unsent block must return to its pool");
        sender.begin(1); // paused output never needs to run to free stale blocks
        assert!(receiver.queue.is_empty());
        assert_eq!(sender.free.len(), 3);
        drop(receiver);
        assert!(matches!(send_chunk(&sender, &cmd_rx, &[0.0; 100], 1), SendOutcome::SinkGone));
    }

    // Count only operations on the current test thread, avoiding false
    // positives from concurrently running decoder/UI tests.
    struct CallbackAllocator;
    thread_local! {
        static ALLOC_TRACKING: Cell<bool> = const { Cell::new(false) };
        static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
        static DEALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    }
    #[global_allocator]
    static TEST_ALLOCATOR: CallbackAllocator = CallbackAllocator;
    unsafe impl std::alloc::GlobalAlloc for CallbackAllocator {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            ALLOC_TRACKING.with(|tracking| {
                if tracking.get() { ALLOCATIONS.with(|count| count.set(count.get() + 1)); }
            });
            unsafe { std::alloc::System.alloc(layout) }
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: std::alloc::Layout) {
            ALLOC_TRACKING.with(|tracking| {
                if tracking.get() { DEALLOCATIONS.with(|count| count.set(count.get() + 1)); }
            });
            unsafe { std::alloc::System.dealloc(pointer, layout); }
        }
    }

    #[test]
    fn pcm_callback_does_not_allocate_or_free_on_first_call_seek_or_integer_output() {
        let shared = SharedState::new();
        let (sender, receiver) = chunk_channel(48_000, 1, 2);
        let (_cmd_tx, cmd_rx) = bounded(1);
        let mut output = PcmOutput::new(receiver);
        let mut data = [0i16; CHUNK_FRAMES * 2];
        for _ in 0..3 {
            sender.begin(shared.generation());
            for _ in 0..2 { push_pcm(&sender, &cmd_rx, &[0.5; CHUNK_FRAMES], shared.generation()); }
            sender.finish(shared.generation());
            ALLOC_TRACKING.with(|tracking| tracking.set(true));
            output.render(&mut data[..37], &shared, 1);
            output.render(&mut data, &shared, 1);
            shared.bump_generation();
            output.render(&mut data, &shared, 1);
            ALLOC_TRACKING.with(|tracking| tracking.set(false));
        }
        assert_eq!(ALLOCATIONS.with(Cell::get), 0);
        assert_eq!(DEALLOCATIONS.with(Cell::get), 0);
    }

    fn saved_local_tracks() -> (crate::config::PersistedState, Vec<std::path::PathBuf>) {
        let saved = crate::config::load();
        let folder = saved.last_folder.as_ref().expect("state.cfg needs a local folder for this diagnostic");
        let mut files: Vec<_> = std::fs::read_dir(folder).unwrap().flatten().map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| matches!(ext.to_str(), Some("ogg" | "mp3" | "flac" | "wav" | "m4a" | "mp4"))))
            .collect();
        files.sort();
        assert!(!files.is_empty(), "the saved folder must contain media");
        (saved, files)
    }

    fn restore_test_controls(shared: &SharedState, saved: &crate::config::PersistedState) {
        shared.set_volume(saved.volume);
        shared.set_eq_enabled(saved.eq_enabled);
        for (band, gain) in saved.eq_gains.iter().enumerate() { shared.set_eq_gain(band, *gain); }
    }

    #[test]
    #[ignore = "requires local media in the folder saved in state.cfg"]
    fn saved_folder_full_tracks_decode_through_the_pcm_pool() {
        let (saved, files) = saved_local_tracks();
        let shared = SharedState::new();
        shared.set_sample_rate(48_000);
        shared.set_device_channels(2);
        restore_test_controls(&shared, &saved);
        let (cmd_tx, cmd_rx) = bounded(8);
        let (status_tx, status_rx) = bounded(16);
        let audio = spawn(shared.clone(), cmd_rx, status_tx);
        for file in files {
            let started = std::time::Instant::now();
            shared.bump_generation();
            cmd_tx.send(DecoderCmd::Load(Source::File(file.to_string_lossy().into_owned()))).unwrap();
            let duration = match status_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                Status::Opened { duration, .. } => duration.unwrap(),
                other => panic!("local source failed: {other:?}"),
            };
            let mut frames = 0;
            let mut peak_wait = Duration::ZERO;
            loop {
                let wait = std::time::Instant::now();
                match audio.chunk_rx.queue.recv_timeout(Duration::from_millis(10)) {
                    Ok(chunk) => {
                        peak_wait = peak_wait.max(wait.elapsed());
                        assert_eq!(chunk.generation, shared.generation());
                        assert!(chunk.samples().iter().all(|sample| sample.is_finite()));
                        frames += chunk.len / 2;
                        recycle(&audio.chunk_rx.recycle, chunk);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        if audio.chunk_rx.state.eof_generation.load(Ordering::Acquire) == shared.generation() {
                            audio.drained_tx.send(shared.generation()).unwrap();
                            assert!(matches!(status_rx.recv_timeout(Duration::from_secs(5)).unwrap(), Status::Finished));
                            break;
                        }
                        assert!(started.elapsed() < Duration::from_secs(30), "local decoder stalled");
                    }
                    Err(error) => panic!("local decoder disconnected: {error}"),
                }
            }
            let seconds = frames as f64 / 48_000.0;
            assert!((seconds - duration.as_secs_f64()).abs() < 0.1, "no decoded audio is lost");
            assert!(started.elapsed() < duration / 10, "decoder must comfortably outpace playback");
            println!("{}: {seconds:.3}s decoded in {:.3}s ({:.1}x realtime), peak chunk wait {:.3}ms",
                file.display(), started.elapsed().as_secs_f64(), seconds / started.elapsed().as_secs_f64(), peak_wait.as_secs_f64() * 1000.0);
        }
        cmd_tx.send(DecoderCmd::Shutdown).unwrap();
        audio.decoder.join().unwrap();
    }

    #[test]
    #[ignore = "requires the saved local folder and an actual output device"]
    fn saved_folder_wasapi_output_has_no_underruns() {
        use cpal::traits::StreamTrait;
        let (saved, files) = saved_local_tracks();
        let shared = SharedState::new();
        restore_test_controls(&shared, &saved);
        let (device, config) = output_device(&shared).unwrap();
        let (cmd_tx, cmd_rx) = bounded(8);
        let (status_tx, status_rx) = bounded(16);
        let audio = spawn(shared.clone(), cmd_rx, status_tx);
        let counters = audio.chunk_rx.state.clone();
        let stream = open_output_stream(device, config, audio.chunk_rx, audio.drained_tx, shared.clone()).unwrap();
        for file in files {
            shared.bump_generation();
            shared.reset_playhead();
            cmd_tx.send(DecoderCmd::Load(Source::File(file.to_string_lossy().into_owned()))).unwrap();
            assert!(matches!(status_rx.recv_timeout(Duration::from_secs(10)).unwrap(), Status::Opened { .. }));
            let before = counters.underrun_frames.load(Ordering::Relaxed);
            stream.play().unwrap();
            // Real WASAPI callbacks, real file reads and saved active EQ.
            // Sleep is solely the automated observation interval, never a
            // production scheduling mechanism; config/UI stay untouched.
            thread::sleep(Duration::from_secs(10));
            stream.pause().unwrap();
            let underruns = counters.underrun_frames.load(Ordering::Relaxed) - before;
            println!("{}: {:.3}s rendered, {underruns} underrun frames", file.display(), shared.position().as_secs_f64());
            assert!(shared.position() > Duration::from_secs(9));
            assert_eq!(underruns, 0);
        }
        cmd_tx.send(DecoderCmd::Shutdown).unwrap();
        audio.decoder.join().unwrap();
        drop(stream);
    }

    fn pcm_wav(samples: &[i16], rate: u32) -> Vec<u8> {
        let size = (samples.len() * 2) as u32;
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&(36 + size).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&size.to_le_bytes());
        for sample in samples { wav.extend_from_slice(&sample.to_le_bytes()); }
        wav
    }

    #[test]
    fn track_change_or_repeat_off_releases_the_previous_stream_allocation() {
        let source = Source::from_raw("https://youtu.be/cache");
        for next in [Source::from_raw("https://youtu.be/next"), source.clone()] {
            let buffer = StreamBuffer::new();
            buffer.append(b"cached track").unwrap();
            buffer.finish(Ok(()));
            let weak = Arc::downgrade(&buffer);
            let mut cache = Some(StreamCache { source: source.clone(), buffer, title: None });
            select_stream_cache(&mut cache, &source, true);
            assert!(weak.upgrade().is_some());
            select_stream_cache(&mut cache, &next, next != source);
            assert!(cache.is_none());
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn downloader_releases_bytes_and_joins_before_the_next_track() {
        let slot: BufferSlot = Arc::new(Mutex::new(None));
        let download = DownloadWorker::start(std::io::Cursor::new(b"one downloaded track".to_vec()), &slot).unwrap();
        let weak = Arc::downgrade(&download.buffer);
        let mut reader = download.buffer.reader();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(&bytes, b"one downloaded track");
        drop(reader);
        drop(download);
        assert!(weak.upgrade().is_none());
        assert!(slot.lock().unwrap().is_none());
    }

    #[test]
    fn cached_stream_seek_works_while_final_audio_is_draining() {
        use crate::broker::Phase;
        let shared = SharedState::new();
        shared.set_sample_rate(48_000);
        shared.set_phase(Phase::Playing);
        let source = Source::Youtube { url: "ram-test://tail".into(), format: "140".into(), playlist_index: None };
        let buffer = StreamBuffer::new();
        buffer.append(&pcm_wav(&vec![8192; 4096], 48_000)).unwrap();
        buffer.finish(Ok(()));
        let mut cache = Some(StreamCache { source: source.clone(), buffer, title: None });
        let (cmd_tx, cmd_rx) = bounded(8);
        let (status_tx, status_rx) = bounded(16);
        let (chunk_tx, chunk_rx) = chunk_channel(48_000, 1, 4);
        let (_drained_tx, drained_rx) = bounded(4);
        let decoder_shared = shared.clone();
        let decoder = thread::spawn(move || play_source(&decoder_shared, &cmd_rx, &status_tx, &chunk_tx, &drained_rx,
            &Arc::new(Mutex::new(None)), &Arc::new(Mutex::new(None)), &mut cache, source));
        assert!(matches!(status_rx.recv_timeout(Duration::from_secs(5)).unwrap(), Status::Opened { .. }));
        let first = chunk_rx.queue.recv_timeout(Duration::from_secs(5)).unwrap();
        // No drain notification is sent. The complete short WAV reaches the
        // final drain wait while playback still owns its downloaded reader.
        thread::sleep(Duration::from_millis(10));
        shared.set_playhead_frames(2400);
        cmd_tx.send(DecoderCmd::SeekRelative(-0.05)).unwrap();
        assert!(matches!(status_rx.recv_timeout(Duration::from_secs(5)).unwrap(), Status::Seeked));
        let replayed = chunk_rx.queue.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first.samples(), replayed.samples());
        assert_ne!(first.generation, replayed.generation);
        cmd_tx.send(DecoderCmd::Stop).unwrap();
        assert!(matches!(decoder.join().unwrap(), PlayExit::Stopped));
    }

    #[test]
    fn loop_replays_from_ram_without_spawning_any_source_process() {
        use crate::broker::Phase;
        use std::sync::atomic::{AtomicBool, Ordering};
        let shared = SharedState::new();
        shared.set_sample_rate(48_000);
        shared.set_loop_enabled(true);
        shared.set_phase(Phase::Playing);
        let source = Source::Youtube { url: "ram-test://offline".into(), format: "140".into(), playlist_index: None };
        let buffer = StreamBuffer::new();
        buffer.append(&pcm_wav(&vec![8192; 4096], 48_000)).unwrap();
        buffer.finish(Ok(()));
        let weak = Arc::downgrade(&buffer);
        let mut cache = Some(StreamCache { source: source.clone(), buffer, title: Some("RAM title".into()) });
        let (_cmd_tx, cmd_rx) = bounded(8);
        let (status_tx, status_rx) = bounded(16);
        let (chunk_tx, chunk_rx) = chunk_channel(48_000, 1, 4);
        let (drained_tx, drained_rx) = bounded(4);
        let child_slot = Arc::new(Mutex::new(None));
        let buffer_slot: BufferSlot = Arc::new(Mutex::new(None));
        let done = Arc::new(AtomicBool::new(false));
        // A virtual output callback drains chunks and signals the same EOF
        // event as WASAPI. No UI, network, or audio device is used.
        let consumer_shared = shared.clone();
        let consumer_done = done.clone();
        let consumer = thread::spawn(move || {
            let mut rounds = Vec::new();
            while !consumer_done.load(Ordering::Acquire) {
                match chunk_rx.queue.recv_timeout(Duration::from_millis(5)) {
                    Ok(chunk) => {
                        rounds.push(chunk.samples().to_vec());
                        recycle(&chunk_rx.recycle, chunk);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        let _ = drained_tx.try_send(consumer_shared.generation());
                    }
                    Err(_) => break,
                }
            }
            rounds
        });
        let decoder_shared = shared.clone();
        let decoder = thread::spawn(move || {
            for _ in 0..2 {
                assert!(matches!(play_source(&decoder_shared, &cmd_rx, &status_tx, &chunk_tx, &drained_rx,
                    &child_slot, &buffer_slot, &mut cache, source.clone()), PlayExit::Eof));
                assert!(child_slot.lock().unwrap().is_none(), "repeat must not spawn yt-dlp");
                assert!(buffer_slot.lock().unwrap().is_none(), "repeat must not spawn a downloader");
            }
            drop(cache);
        });
        for _ in 0..2 {
            assert!(matches!(status_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                Status::Opened { title: Some(title), .. } if title == "RAM title"));
            assert!(matches!(status_rx.recv_timeout(Duration::from_secs(5)).unwrap(), Status::Finished));
        }
        decoder.join().unwrap();
        done.store(true, Ordering::Release);
        let rounds = consumer.join().unwrap();
        assert_eq!(rounds.len(), 2);
        assert_eq!(rounds[0], rounds[1], "both plays decode identical cached bytes from offset zero");
        assert!(weak.upgrade().is_none(), "the last cache owner must release all bytes");
    }

    #[test]
    fn all_streaming_platforms_seek_a_downloaded_memory_container() {
        for url in ["https://youtu.be/test", "https://www.bilibili.com/video/BVtest", "https://www.nicovideo.jp/watch/sm1"] {
            let buffer = StreamBuffer::new();
            buffer.append(&pcm_wav(&vec![8192; 48_000], 48_000)).unwrap();
            buffer.finish(Ok(()));
            let mut track = open_track(&Source::from_raw(url), &Arc::new(Mutex::new(None)),
                &Arc::new(Mutex::new(None)), Some(buffer)).unwrap();
            let shared = SharedState::new();
            shared.set_sample_rate(48_000);
            let mut generation = shared.generation();
            let mut pending = vec![1.0];
            let mut eq = EqChain::new();
            let mut resampler = None;
            assert!(seek_relative(&mut track, &shared, 0.5, &mut pending, &mut eq, &mut resampler, &mut generation));
            assert!(shared.position() <= Duration::from_millis(500));
            assert!(shared.position() >= Duration::from_millis(450), "container seek returns the actual packet timestamp");
            assert!(pending.is_empty());
            assert!(seek_relative(&mut track, &shared, -10.0, &mut pending, &mut eq, &mut resampler, &mut generation));
            assert_eq!(shared.position(), Duration::ZERO);
            assert!(track._child.is_none());
            assert!(track._download.is_none());
        }
    }

    #[test]
    fn progressive_aac_mp4_starts_and_seeks_before_download_finishes() {
        let buffer = StreamBuffer::new();
        // Intentionally leave the producer unfinished: MP4 must probe via
        // its progressive path, not require a final HTTP/file length.
        let data = include_bytes!("../tests/fixtures/silence.m4a");
        let payload = data.windows(4).position(|bytes| bytes == b"mdat").unwrap() + 4;
        let prefix = payload + (data.len() - payload) / 2;
        buffer.append(&data[..prefix]).unwrap();
        let mut track = open_track(&Source::from_raw("https://youtu.be/progressive"),
            &Arc::new(Mutex::new(None)), &Arc::new(Mutex::new(None)), Some(buffer.clone())).unwrap();
        let packet = track.format.next_packet().unwrap();
        assert!(track.decoder.decode(&packet).is_ok());
        let shared = SharedState::new();
        shared.set_sample_rate(48_000);
        let mut generation = shared.generation();
        assert!(seek_relative(&mut track, &shared, 0.05, &mut Vec::new(), &mut EqChain::new(), &mut None, &mut generation));
        assert!(!buffer.complete());
        assert!(shared.position() > Duration::from_millis(20));
        assert!(!seek_relative(&mut track, &shared, 0.2, &mut Vec::new(), &mut EqChain::new(), &mut None, &mut generation),
            "undownloaded sample bytes must be refused without waiting");
        assert!(seek_relative(&mut track, &shared, -5.0, &mut Vec::new(), &mut EqChain::new(), &mut None, &mut generation));
        assert_eq!(shared.position(), Duration::ZERO);
        assert!(track.decoder.decode(&track.seek_packet.take().unwrap()).is_ok());
        buffer.append(&data[prefix..]).unwrap();
        buffer.finish(Ok(()));
        assert!(seek_relative(&mut track, &shared, 0.2, &mut Vec::new(), &mut EqChain::new(), &mut None, &mut generation));
    }

    #[test]
    fn zero_gain_eq_is_exact_passthrough() {
        let mut eq = EqChain::new();
        eq.rebuild(&[0.0; EQ_BANDS], 44_100, 2);
        let original: Vec<f32> = (0..1000).map(|i| ((i as f32) * 0.001).sin()).collect();
        let mut buf = original.clone();
        eq.apply(&mut buf, 2);
        assert_eq!(buf, original, "0 dB peaking bands must be an identity");
    }

    #[test]
    fn resampler_length_and_constancy() {
        // 44.1k → 48k: 4410 frames in, ~4800 out, constant stays constant.
        let mut rs = LinearResampler::new(44_100, 48_000, 1);
        let input = vec![0.5f32; 4410];
        let mut out = Vec::new();
        rs.process(&input, 4410, &mut out);
        assert!((out.len() as i64 - 4800).abs() <= 2, "got {} frames", out.len());
        // The first two frames interpolate from the zero-initialized carry —
        // a 2-sample soft start. Everything after is exact.
        assert!(out[2..].iter().all(|&s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn resampler_downsample_length() {
        let mut rs = LinearResampler::new(48_000, 44_100, 2);
        let input = vec![0.25f32; 4800 * 2];
        let mut out = Vec::new();
        rs.process(&input, 4800, &mut out);
        assert!((out.len() as i64 - 4410 * 2).abs() <= 4, "got {} samples", out.len());
    }

    #[test]
    fn clean_title_takes_first_line_and_caps() {
        assert_eq!(clean_title("Neo Tokyo [HD]\nsecond line\r\n"), "Neo Tokyo [HD]");
        assert_eq!(clean_title("  padded  "), "padded");
        assert_eq!(clean_title(""), "");
        let long = "x".repeat(300);
        assert_eq!(clean_title(&long).chars().count(), 120);
    }

    #[test]
    fn yt_dlp_args_bound_by_playlist_index() {
        let to_str = |a: &std::ffi::OsString| a.to_string_lossy().to_string();

        let single = yt_dlp_args("140", None, "u");
        let single: Vec<String> = single.iter().map(to_str).collect();
        assert!(single.contains(&"--no-playlist".to_string()));
        let i = single.iter().position(|a| a == "--playlist-items").unwrap();
        assert_eq!(single[i + 1], "1");

        let third = yt_dlp_args("140", Some(3), "u");
        let third: Vec<String> = third.iter().map(to_str).collect();
        assert!(
            !third.contains(&"--no-playlist".to_string()),
            "playlist entry must not force --no-playlist"
        );
        let i = third.iter().position(|a| a == "--playlist-items").unwrap();
        assert_eq!(third[i + 1], "3");
        assert_eq!(third.last().unwrap(), "u");
    }

    #[test]
    #[ignore = "requires yt-dlp and live network access"]
    fn live_youtube_decodes_audio() {
        use crate::broker::{Broker, Command as PlayerCommand, DecoderLink, Phase};
        let url = std::env::var("KYOUKO_TEST_URL")
            .unwrap_or_else(|_| "https://www.youtube.com/watch?v=dnObAMACYrA".into());
        let shared = SharedState::new();
        shared.set_sample_rate(48_000);
        shared.set_device_channels(2);
        let (cmd_tx, cmd_rx) = bounded(8);
        let (status_tx, status_rx) = bounded(16);
        let audio = spawn(shared.clone(), cmd_rx, status_tx);
        let mut broker = Broker::new(shared.clone(), DecoderLink::with_interrupt(
            cmd_tx.clone(), audio.controls.clone().hook(),
        ));
        let result = (|| -> Result<(), String> {
            // Changing a live, partially downloaded stream must unblock the
            // old reader and start the next URL on the same decoder thread.
            for raw in [&*url, "https://www.youtube.com/watch?v=jNQXAC9IVRw"] {
                broker.handle_command(PlayerCommand::Load {
                    source: Source::from_raw(raw), paused: false,
                });
                let deadline = std::time::Instant::now() + Duration::from_secs(60);
                while shared.phase() == Phase::Loading {
                    let status = status_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                        .map_err(|e| format!("waiting for {raw} to open: {e}"))?;
                    broker.handle_status(status);
                }
                if shared.phase() != Phase::Playing {
                    return Err(format!("stream did not enter Playing: {raw}"));
                }
                loop {
                    let chunk = audio.chunk_rx.queue.recv_timeout(Duration::from_secs(10))
                        .map_err(|e| format!("waiting for decoded audio: {e}"))?;
                    if chunk.generation != shared.generation() { continue; }
                    assert!(!chunk.samples().is_empty());
                    assert!(chunk.samples().iter().all(|sample| sample.is_finite()));
                    break;
                }
            }
            Ok(())
        })();
        cmd_tx.send(DecoderCmd::Shutdown).unwrap();
        audio.controls.interrupt();
        audio.decoder.join().unwrap();
        assert!(audio.controls.child_slot.lock().unwrap().is_none());
        assert!(audio.controls.buffer_slot.lock().unwrap().is_none());
        result.unwrap();
    }

    #[test]
    fn real_decoder_failures_retry_twice_per_track_then_exhaust_queue() {
        use crate::broker::{Broker, Command as PlayerCommand, DecoderLink, Phase};
        let dir = std::env::temp_dir().join(format!("kyouko-retry-{}-decoder", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("invalid.mp3"), b"not audio").unwrap();
        // A valid PCM WAV header with no samples must report failure instead
        // of waiting for an audio callback that never started.
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&36u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&96_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(dir.join("empty.wav"), wav).unwrap();

        let shared = SharedState::new();
        let (cmd_tx, cmd_rx) = bounded(8);
        let (status_tx, status_rx) = bounded(16);
        let (chunk_tx, _chunk_rx) = chunk_channel(48_000, 2, 16);
        let (_drained_tx, drained_rx) = bounded(2);
        let child_slot = Arc::new(Mutex::new(None));
        let decoder_shared = shared.clone();
        let decoder = thread::spawn(move || {
            run_decoder(decoder_shared, cmd_rx, status_tx, chunk_tx, drained_rx, child_slot, Arc::new(Mutex::new(None)));
        });
        let mut broker = Broker::new(shared.clone(), DecoderLink::with_interrupt(
            cmd_tx.clone(), Arc::new(|| {}),
        ));
        broker.handle_command(PlayerCommand::Load {
            source: Source::File(dir.to_string_lossy().into_owned()), paused: false,
        });
        let mut attempts = std::collections::HashMap::new();
        let mut reasons = Vec::new();
        let result = (|| -> Result<(), String> {
            for _ in 0..4 {
                let status = status_rx.recv_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())?;
                if let Status::Failed { source, reason, .. } = &status {
                    *attempts.entry(source.raw().to_string()).or_insert(0usize) += 1;
                    reasons.push(reason.clone());
                } else {
                    return Err(format!("expected failure, got {status:?}"));
                }
                broker.handle_status(status);
            }
            Ok(())
        })();
        cmd_tx.send(DecoderCmd::Shutdown).unwrap();
        decoder.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        result.unwrap();
        assert_eq!(attempts.len(), 2);
        assert!(attempts.values().all(|&count| count == 2));
        assert_eq!(shared.phase(), Phase::Stopped);
        assert!(status_rx.try_recv().is_err(), "no additional failures after queue exhaustion");
        assert!(reasons.iter().any(|r| r.contains("without decodable audio")));
    }

    #[test]
    fn child_guard_detects_unsuccessful_stream_exit() {
        for code in [0, 7] {
            #[cfg(windows)]
            let mut child = Command::new("cmd.exe").args(["/C", &format!("exit {code}")])
                .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            #[cfg(not(windows))]
            let mut child = Command::new("sh").args(["-c", &format!("exit {code}")])
                .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            child.wait().unwrap();
            let slot = Arc::new(Mutex::new(Some(child)));
            let guard = ChildGuard { slot: slot.clone() };
            assert_eq!(guard.eof_failure().is_some(), code != 0);
            drop(guard);
            assert!(slot.lock().unwrap().is_none());
        }
    }

    #[test]
    fn channel_conversion() {
        let mut out = Vec::new();
        convert_channels(&[0.1, 0.9], 2, 1, &mut out);
        assert_eq!(out, vec![0.5]);
        out.clear();
        convert_channels(&[0.7], 1, 2, &mut out);
        assert_eq!(out, vec![0.7, 0.7]);
    }
}
