//! Opt-in hardware diagnostics. Only raw records in preallocated callback
//! storage; analysis and export require stream teardown/join.
use super::*;
use std::cell::UnsafeCell;
use std::time::Instant;

#[derive(Clone, Copy)]
struct Record {
    start: Instant,
    end: Instant,
    timestamp: cpal::OutputStreamTimestamp,
    frames: usize,
    qpc: i64,
}

pub(super) struct CallbackRecords {
    enabled: AtomicBool,
    count: AtomicUsize,
    dropped: AtomicUsize,
    records: Box<[UnsafeCell<Option<Record>>]>,
}

// One stream owns the writer. Readers must join that stream before inspecting
// storage. There is no API to read records while callbacks can still run.
unsafe impl Sync for CallbackRecords {}

impl CallbackRecords {
    pub(super) fn new() -> Self {
        Self { enabled: AtomicBool::new(false), count: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            records: (0..8192).map(|_| UnsafeCell::new(None)).collect() }
    }
    pub(super) fn start(&self, info: &cpal::OutputCallbackInfo, frames: usize)
        -> Option<(Instant, cpal::OutputStreamTimestamp, usize, i64)> {
        if !self.enabled.load(Ordering::Relaxed) { return None; }
        let start = Instant::now();
        let mut qpc = 0;
        #[cfg(windows)]
        unsafe { let _ = windows::Win32::System::Performance::QueryPerformanceCounter(&mut qpc); }
        Some((start, info.timestamp(), frames, qpc))
    }
    pub(super) fn end(&self, start: Option<(Instant, cpal::OutputStreamTimestamp, usize, i64)>) {
        if let Some((start, timestamp, frames, qpc)) = start {
            let end = Instant::now();
            let index = self.count.load(Ordering::Relaxed);
            if let Some(slot) = self.records.get(index) {
                unsafe { *slot.get() = Some(Record { start, end, timestamp, frames, qpc }); }
                self.count.store(index + 1, Ordering::Release);
            } else { self.dropped.fetch_add(1, Ordering::Relaxed); }
        }
    }
    // Caller has destroyed/joined the stream's sole writer.
    unsafe fn joined_records(&self) -> Vec<Record> {
        self.records.iter().take(self.count.load(Ordering::Acquire))
            .map(|slot| unsafe { (*slot.get()).unwrap() }).collect()
    }
}

#[derive(Debug, PartialEq)]
struct Distribution { count: usize, p50: f64, p95: f64, p99: f64, max: f64 }

// Nearest-rank quantiles; regressions/empty data are never silently converted
// to a zero interval. Sorting and vector creation happen outside the callback.
fn distribution(mut values: Vec<f64>) -> Option<Distribution> {
    if values.is_empty() { return None; }
    values.sort_by(f64::total_cmp);
    let quantile = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1];
    Some(Distribution { count: values.len(), p50: quantile(50), p95: quantile(95),
        p99: quantile(99), max: *values.last().unwrap() })
}

struct Analysis {
    execution: Distribution,
    entry: Distribution,
    device: Option<Distribution>,
    playback: Option<Distribution>,
    clock_regressions: usize,
    entry_gaps: usize,
}
fn analyze(records: &[Record], rate: u32) -> Analysis {
    let execution = distribution(records.iter().map(|r| r.end.duration_since(r.start).as_secs_f64() * 1e6).collect()).unwrap();
    let entry = distribution(records.windows(2).map(|p| p[1].start.duration_since(p[0].start).as_secs_f64() * 1e3).collect()).unwrap();
    let clock_intervals = |playback: bool| records.windows(2).filter_map(|p| {
        let clock = |r: &Record| if playback { r.timestamp.playback } else { r.timestamp.callback };
        clock(&p[1]).checked_duration_since(clock(&p[0])).map(|d| d.as_secs_f64() * 1e3)
    }).collect();
    Analysis { execution, entry, device: distribution(clock_intervals(false)),
        playback: distribution(clock_intervals(true)),
        clock_regressions: records.windows(2).filter(|p| p[1].timestamp.callback < p[0].timestamp.callback).count(),
        entry_gaps: records.windows(2).filter(|p| p[1].start.duration_since(p[0].start).as_secs_f64() > 2.0 * p[0].frames as f64 / rate as f64).count() }
}

#[cfg(windows)]
fn cpu_seconds() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user).unwrap(); }
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    (ticks(kernel) + ticks(user)) as f64 / 10_000_000.0
}

#[test]
fn diagnostic_quantiles_handle_empty_and_nearest_rank() {
    assert_eq!(distribution(vec![]), None);
    assert_eq!(distribution(vec![4.0, 1.0, 3.0, 2.0]), Some(Distribution { count: 4, p50: 2.0, p95: 4.0, p99: 4.0, max: 4.0 }));
}
#[test]
fn diagnostic_entry_intervals_are_independent_of_device_clock() {
    let start = Instant::now();
    let records: Vec<_> = [0, 10, 40].into_iter().enumerate().map(|(i, ms)| Record {
        start: start + Duration::from_millis(ms), end: start + Duration::from_millis(ms) + Duration::from_micros(5),
        timestamp: cpal::OutputStreamTimestamp { callback: cpal::StreamInstant::new(0, [0, 10_000_000, 9_000_000][i]), playback: cpal::StreamInstant::new(1, i as u32 * 10_000_000) },
        frames: 480, qpc: 0,
    }).collect();
    let a = analyze(&records, 48_000);
    assert_eq!(a.execution.max, 5.0);
    assert_eq!(a.entry.max, 30.0);
    assert_eq!(a.device.unwrap().max, 10.0);
    assert_eq!(a.clock_regressions, 1);
    assert_eq!(a.entry_gaps, 1);
    assert_eq!(a.playback.unwrap().max, 10.0);
}
#[test]
fn diagnostic_fixed_storage_reports_overflow_without_growth() {
    let records = CallbackRecords::new();
    let t = cpal::StreamInstant::new(0, 0);
    let info = cpal::OutputCallbackInfo::new(cpal::OutputStreamTimestamp { callback: t, playback: t });
    records.enabled.store(true, Ordering::Relaxed);
    for _ in 0..8195 { records.end(records.start(&info, 480)); }
    assert_eq!(records.count.load(Ordering::Relaxed), 8192);
    assert_eq!(records.dropped.load(Ordering::Relaxed), 3);
}

#[test]
#[cfg(windows)]
#[ignore = "release CPU/WASAPI diagnostic using saved source/settings; optional real worker replacements"]
fn saved_track_release_pcm_performance() {
    use crate::broker::{Command as PlayerCommand, DecoderLink, Phase};
    fn pump(output: &mut AudioOut, broker: &mut Broker, statuses: &Receiver<Status>, duration: Duration) {
        crate::ui::wait_headless_timeout(duration);
        for status in statuses.try_iter() { broker.handle_status(status); }
        broker.poll_background();
        output.handle_device_events(broker);
        output.set_playing(broker.shared().phase() == Phase::Playing);
    }
    fn observe(output: &mut AudioOut, broker: &mut Broker, statuses: &Receiver<Status>, duration: Duration) {
        let until = Instant::now() + duration;
        loop {
            let remaining = until.saturating_duration_since(Instant::now());
            if remaining.is_zero() { break; }
            pump(output, broker, statuses, remaining);
        }
    }
    let saved = crate::config::load();
    assert!(saved.last_track.as_ref().is_some_and(|p| std::path::Path::new(p).is_file()) || saved.last_folder.as_ref().is_some_and(|p| std::path::Path::new(p).is_dir()), "saved local track/folder required");
    let _wake = crate::ui::HeadlessWakeGuard::new();
    let shared = SharedState::new();
    shared.set_volume(saved.volume); shared.set_eq_enabled(saved.eq_enabled); shared.set_loop_enabled(saved.loop_enabled);
    for (band, gain) in saved.eq_gains.iter().enumerate() { shared.set_eq_gain(band, *gain); }
    let device = output_device(&shared).unwrap();
    let (tx, rx) = crate::decoder_wake::command_channel(8);
    let (status_tx, status_rx) = bounded(16);
    let audio = spawn(shared.clone(), rx.clone(), status_tx);
    let decoder_id = audio.decoder.thread().id();
    let mut output = AudioOut::managed(Ok(device), audio.chunk_rx, audio.drained_tx, shared.clone(), audio.controls.clone());
    let mut broker = Broker::new(shared.clone(), DecoderLink::with_interrupt(tx, rx, audio.controls.clone().hook()).with_suspend(audio.controls.clone().suspend_hook()));
    let original_device = output.key.as_ref().unwrap().clone();
    let replace = std::env::var("KYOUKO_PERF_REPLACE").is_ok_and(|v| v == "1");
    let mut observations = Vec::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        broker.handle_command(PlayerCommand::RestoreState { last_track: saved.last_track.clone(), last_folder: saved.last_folder.clone(), last_playlist_index: saved.last_playlist_index });
        let deadline = Instant::now() + Duration::from_secs(15);
        while shared.phase() != Phase::Paused {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "saved source load stalled");
            pump(&mut output, &mut broker, &status_rx, remaining);
        }
        println!("PERF_DEVICE key={original_device:?} process_priority_class={}", unsafe { windows::Win32::System::Threading::GetPriorityClass(windows::Win32::System::Threading::GetCurrentProcess()) });
        println!("PERF_SOURCE settings={saved:?} replace={replace} realtime={} pid={}", cfg!(feature = "output-realtime"), std::process::id());
        broker.handle_command(PlayerCommand::TogglePause); output.set_playing(true);
        for segment in 0..if replace { 4 } else { 1 } {
            observe(&mut output, &mut broker, &status_rx, Duration::from_secs(2));
            let counters = output.receiver.as_ref().unwrap().state.clone();
            assert_eq!(counters.ready_generation.load(Ordering::Acquire), shared.generation());
            let rate = shared.sample_rate(); let channels = shared.device_channels() as usize;
            let cpal::platform::StreamInner::Wasapi(worker) = output.stream.as_ref().unwrap().as_inner();
            let startup = worker.output_worker_startup().cloned();
            let epoch = output.signals.stream_state.load(Ordering::Acquire) & !3;
            let underruns = counters.underrun_frames.load(Ordering::Relaxed);
            let cpu_before = cpu_seconds(); let start = Instant::now();
            counters.timing.enabled.store(true, Ordering::Relaxed);
            observe(&mut output, &mut broker, &status_rx, Duration::from_secs(if replace { 5 } else { 20 }));
            counters.timing.enabled.store(false, Ordering::Relaxed);
            let wall = start.elapsed().as_secs_f64(); let cpu = (cpu_seconds() - cpu_before) / wall * 100.0;
            let underruns = counters.underrun_frames.load(Ordering::Relaxed) - underruns;
            assert!(Arc::ptr_eq(&counters, &output.receiver.as_ref().unwrap().state), "unplanned output replacement invalidates observation");
            assert_eq!(output.key.as_ref(), Some(&original_device), "comparison requires the identical device/configuration");
            observations.push((segment, counters, rate, channels, wall, cpu, underruns, epoch, startup));
            assert_eq!(shared.phase(), Phase::Playing);
            if replace && segment < 3 {
                let position = shared.position().as_secs_f64();
                let recovered = Instant::now();
                output.signals.endpoint_changed(); // reopen actual worker, same device/policy
                output.handle_device_events(&mut broker);
                let deadline = Instant::now() + Duration::from_secs(15);
                while shared.phase() != Phase::Playing {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    assert!(!remaining.is_zero(), "output recovery stalled");
                    pump(&mut output, &mut broker, &status_rx, remaining);
                }
                assert!((shared.position().as_secs_f64() - position).abs() < 0.15);
                assert_eq!(audio.decoder.thread().id(), decoder_id);
                println!("PERF_REPLACE segment={segment} restore_s={:.6} position_s={position:.6}", recovered.elapsed().as_secs_f64());
            }
        }
    }));
    broker.handle_command(PlayerCommand::Quit); output.set_playing(false);
    drop(output); audio.decoder.join().unwrap(); // all callback writers joined before analysis
    if let Err(error) = result { std::panic::resume_unwind(error); }
    let mut frequency = 0;
    unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency).unwrap(); }
    for (segment, counters, rate, channels, wall, cpu, underruns, epoch, startup) in observations {
        let records = unsafe { counters.timing.joined_records() };
        assert!(records.len() > 100);
        let a = analyze(&records, rate);
        println!("PCM_PERF segment={segment} epoch={epoch} startup={startup:?} rate={rate} channels={channels} seconds={wall:.6} cpu_one_core_pct={cpu:.6} pcm_sample_bytes={} blocks={} underrun_frames={underruns} callbacks={} dropped={} callback_us={:?} actual_entry_ms={:?} device_clock_ms={:?} predicted_playback_ms={:?} clock_regressions={} entry_gaps_over_2_periods={}",
            (buffer_depth(rate)+1)*CHUNK_FRAMES*channels*size_of::<f32>(), buffer_depth(rate)+1, records.len(), counters.timing.dropped.load(Ordering::Relaxed), a.execution, a.entry, a.device, a.playback, a.clock_regressions, a.entry_gaps);
        if let Ok(prefix) = std::env::var("KYOUKO_PERF_RECORDS") {
            use std::io::Write;
            let mut file = std::fs::File::create(format!("{prefix}-{segment}.csv")).unwrap();
            writeln!(file, "pid,epoch,tid,qpc_frequency,qpc,entry_relative_ns,execution_ns,device_clock_ns,predicted_playback_ns,frames").unwrap();
            for r in &records {
                writeln!(file, "{},{},{},{frequency},{},{},{},{},{},{}", std::process::id(), epoch, startup.as_ref().map_or(0, |s| s.thread_id), r.qpc, r.start.duration_since(records[0].start).as_nanos(), r.end.duration_since(r.start).as_nanos(), r.timestamp.callback.as_nanos(), r.timestamp.playback.as_nanos(), r.frames).unwrap();
            }
        }
    }
}
