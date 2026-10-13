//! Opt-in hardware diagnostics. Raw records only in the callback; all timing
//! analysis, process CPU accounting and printing happen after stream teardown.
use super::*;
use std::cell::UnsafeCell;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

#[derive(Clone, Copy)]
struct Record {
    start: Instant,
    end: Instant,
    timestamp: cpal::StreamInstant,
    frames: usize,
}

pub(super) struct CallbackRecords {
    enabled: AtomicBool,
    count: AtomicUsize,
    records: Box<[UnsafeCell<Option<Record>>]>,
}

// Only the single stream callback writes records. The diagnostic reader must
// destroy/join that stream before inspecting them. No live reads are exposed.
unsafe impl Sync for CallbackRecords {}

impl CallbackRecords {
    pub(super) fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            count: AtomicUsize::new(0),
            records: (0..8192).map(|_| UnsafeCell::new(None)).collect(),
        }
    }
    pub(super) fn start(
        &self,
        info: &cpal::OutputCallbackInfo,
        frames: usize,
    ) -> Option<(Instant, cpal::StreamInstant, usize)> {
        self.enabled
            .load(Ordering::Relaxed)
            .then(|| (Instant::now(), info.timestamp().callback, frames))
    }
    pub(super) fn end(&self, start: Option<(Instant, cpal::StreamInstant, usize)>) {
        if let Some((start, timestamp, frames)) = start {
            let end = Instant::now();
            let index = self.count.load(Ordering::Relaxed);
            if let Some(slot) = self.records.get(index) {
                unsafe {
                    *slot.get() = Some(Record {
                        start,
                        end,
                        timestamp,
                        frames,
                    });
                }
                self.count.store(index + 1, Ordering::Release);
            }
        }
    }
}

#[cfg(windows)]
fn cpu_seconds() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
        .unwrap();
    }
    let ticks = |time: FILETIME| ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
    (ticks(kernel) + ticks(user)) as f64 / 10_000_000.0
}

#[test]
#[cfg(windows)]
#[ignore = "release CPU and WASAPI timing diagnostic using the saved local source"]
fn saved_track_release_pcm_performance() {
    use cpal::traits::StreamTrait;
    let saved = crate::config::load();
    let path = saved
        .last_track
        .as_ref()
        .filter(|path| std::path::Path::new(path).is_file())
        .cloned()
        .or_else(|| {
            saved
                .last_folder
                .as_ref()
                .and_then(|folder| std::fs::read_dir(folder).ok())
                .and_then(|entries| {
                    entries
                        .flatten()
                        .map(|entry| entry.path())
                        .find(|path| path.is_file() && crate::broker::is_media_path(path))
                })
                .map(|path| path.to_string_lossy().into_owned())
        })
        .expect("saved local track/folder required");
    let shared = SharedState::new();
    shared.set_volume(saved.volume);
    shared.set_eq_enabled(saved.eq_enabled);
    for (band, gain) in saved.eq_gains.iter().enumerate() {
        shared.set_eq_gain(band, *gain);
    }
    let (device, config) = output_device(&shared).unwrap();
    let rate = shared.sample_rate();
    let channels = shared.device_channels() as usize;
    let (cmd_tx, cmd_rx) = crate::decoder_wake::command_channel(8);
    let (status_tx, status_rx) = bounded(16);
    let audio = spawn(shared.clone(), cmd_rx, status_tx);
    let state = audio.chunk_rx.state.clone();
    let stream = open_output_stream(
        device,
        config,
        audio.chunk_rx,
        audio.drained_tx,
        shared.clone(),
    )
    .unwrap();
    cmd_tx
        .send(DecoderCmd::Load(
            Source::File(path.clone()),
            shared.load_id(),
        ))
        .unwrap();
    assert!(matches!(
        status_rx.recv_timeout(Duration::from_secs(15)).unwrap(),
        Status::Opened { .. }
    ));
    stream.play().unwrap();
    // Let priming/startup settle before the CPU observation, outside callback.
    thread::sleep(Duration::from_secs(2));
    let underruns_before = state.underrun_frames.load(Ordering::Relaxed);
    state.timing.enabled.store(true, Ordering::Relaxed);
    let cpu_before = cpu_seconds();
    let start = Instant::now();
    thread::sleep(Duration::from_secs(20));
    let wall = start.elapsed().as_secs_f64();
    let cpu = (cpu_seconds() - cpu_before) / wall * 100.0;
    stream.pause().unwrap();
    let _ = cmd_tx.send(DecoderCmd::Shutdown);
    audio.decoder.join().unwrap();
    drop(stream); // join the writer before reading raw records
    let records: Vec<_> = state
        .timing
        .records
        .iter()
        .take(state.timing.count.load(Ordering::Acquire))
        .map(|slot| unsafe { (*slot.get()).unwrap() })
        .collect();
    assert!(records.len() > 100);
    let mut durations: Vec<_> = records
        .iter()
        .map(|record| record.end.duration_since(record.start).as_secs_f64() * 1e6)
        .collect();
    durations.sort_by(f64::total_cmp);
    let mut intervals: Vec<_> = records
        .windows(2)
        .map(|pair| {
            pair[1]
                .timestamp
                .duration_since(pair[0].timestamp)
                .as_secs_f64()
                * 1e3
        })
        .collect();
    intervals.sort_by(f64::total_cmp);
    let gaps = records
        .windows(2)
        .filter(|pair| {
            pair[1]
                .timestamp
                .duration_since(pair[0].timestamp)
                .as_secs_f64()
                > 2.0 * pair[0].frames as f64 / rate as f64
        })
        .count();
    let p99 = |values: &[f64]| values[(values.len() * 99 / 100).min(values.len() - 1)];
    println!(
        "PCM_PERF track={path:?} rate={rate} channels={channels} seconds={wall:.3} cpu_one_core_pct={cpu:.4} pcm_sample_bytes={} blocks={} underrun_frames={} callbacks={} callback_us_p50={:.3} callback_us_p99={:.3} callback_us_max={:.3} interval_ms_p99={:.3} interval_ms_max={:.3} gaps_over_2_periods={gaps}",
        (buffer_depth(rate) + 1) * CHUNK_FRAMES * channels * size_of::<f32>(),
        buffer_depth(rate) + 1,
        state.underrun_frames.load(Ordering::Relaxed) - underruns_before,
        records.len(),
        durations[durations.len() / 2],
        p99(&durations),
        durations[durations.len() - 1],
        p99(&intervals),
        intervals[intervals.len() - 1]
    );
}
