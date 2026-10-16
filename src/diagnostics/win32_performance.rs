//! Finite, opt-in diagnostic controller. No desktop input is synthesized.
//! Callback entry and copied PCM use independent bounded first-event records.
use super::*;
use serde_json::{Value, json};
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Gdi::UpdateWindow;

pub(super) const MESSAGE: u32 = 0x8007;
enum Request {
    Inspect,
    Drawer,
    Synthetic(Vec<u8>),
}
struct Session {
    requests: Receiver<Request>,
    replies: crossbeam_channel::Sender<Value>,
    ready: crossbeam_channel::Sender<isize>,
}
static SESSION: OnceLock<Session> = OnceLock::new();
static TIMING: AtomicBool = AtomicBool::new(false);
static ACCEPTED: AtomicI64 = AtomicI64::new(0);
static APPLIED: AtomicI64 = AtomicI64::new(0);
static PRESENTED: AtomicI64 = AtomicI64::new(0);
static STATUS: AtomicI64 = AtomicI64::new(0);
static STATUS_PRESENTED: AtomicI64 = AtomicI64::new(0);
static READY: AtomicI64 = AtomicI64::new(0);

struct DeliverySlot {
    generation: AtomicU64,
    timestamp: AtomicI64,
}
struct DeliveryRecords<const N: usize> {
    used: AtomicUsize,
    active: AtomicUsize,
    slots: [DeliverySlot; N],
    overflow: AtomicUsize,
}
impl<const N: usize> DeliveryRecords<N> {
    const fn new() -> Self {
        Self {
            used: AtomicUsize::new(0),
            active: AtomicUsize::new(N),
            slots: [const {
                DeliverySlot {
                    generation: AtomicU64::new(u64::MAX),
                    timestamp: AtomicI64::new(0),
                }
            }; N],
            overflow: AtomicUsize::new(0),
        }
    }
    fn arm(&self, generation: u64) {
        let index = self.used.fetch_add(1, Ordering::Relaxed);
        if let Some(slot) = self.slots.get(index) {
            slot.generation.store(generation, Ordering::Relaxed);
            self.active.store(index, Ordering::Release);
        } else {
            self.active.store(N, Ordering::Release);
            self.overflow.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn record(&self, generation: u64, clock: impl FnOnce() -> i64) {
        // Never reuse a slot: a callback racing the next command can only
        // publish into its previous slot, never contaminate the new request.
        if let Some(slot) = self.slots.get(self.active.load(Ordering::Acquire))
            && slot.generation.load(Ordering::Relaxed) == generation
            && slot.timestamp.load(Ordering::Relaxed) == 0
        {
            let _ =
                slot.timestamp
                    .compare_exchange(0, clock(), Ordering::Release, Ordering::Relaxed);
        }
    }
    fn latest(&self) -> i64 {
        self.slots
            .get(self.active.load(Ordering::Acquire))
            .map_or(0, |slot| slot.timestamp.load(Ordering::Acquire))
    }
}
static DELIVERIES: DeliveryRecords<2048> = DeliveryRecords::new();
static FIRST_CALLBACKS: DeliveryRecords<2048> = DeliveryRecords::new();

fn qpc() -> i64 {
    let mut value = 0;
    unsafe {
        let _ = windows::Win32::System::Performance::QueryPerformanceCounter(&mut value);
    }
    value
}

pub(crate) fn pcm_delivered(generation: u64) {
    if TIMING.load(Ordering::Relaxed) {
        DELIVERIES.record(generation, qpc);
    }
}
pub(crate) fn callback_entered(generation: u64) {
    if TIMING.load(Ordering::Relaxed) {
        FIRST_CALLBACKS.record(generation, qpc);
    }
}
pub(super) fn accepted(command: &Command, shared: &SharedState) {
    if !TIMING.load(Ordering::Relaxed) {
        return;
    }
    STATUS.store(0, Ordering::Relaxed);
    STATUS_PRESENTED.store(0, Ordering::Relaxed);
    PRESENTED.store(0, Ordering::Relaxed);
    let increment = matches!(
        command,
        Command::Load { .. }
            | Command::RestoreState { .. }
            | Command::LoadDropped(_)
            | Command::NextTrack
            | Command::PrevTrack
            | Command::JumpToTrack(_)
            | Command::SeekRelative(_)
    ) || matches!(command, Command::TogglePause)
        && shared.phase() == Phase::Stopped;
    DELIVERIES.arm(shared.generation() + u64::from(increment));
    FIRST_CALLBACKS.arm(shared.generation() + u64::from(increment));
    ACCEPTED.store(qpc(), Ordering::Release);
}
pub(super) fn applied() {
    if TIMING.load(Ordering::Relaxed) {
        APPLIED.store(qpc(), Ordering::Release);
    }
}
pub(super) fn presented() {
    if TIMING.load(Ordering::Relaxed) {
        let _ = PRESENTED.compare_exchange(0, qpc(), Ordering::Release, Ordering::Relaxed);
        if STATUS.load(Ordering::Acquire) != 0 {
            let _ =
                STATUS_PRESENTED.compare_exchange(0, qpc(), Ordering::Release, Ordering::Relaxed);
        }
    }
}
pub(super) fn status(status: &Status, shared: &SharedState) {
    let current = match status {
        Status::Opened { load_id, .. }
        | Status::Seeked { load_id }
        | Status::SeekFailed { load_id } => *load_id == shared.load_id(),
        _ => false,
    };
    if TIMING.load(Ordering::Relaxed) && current {
        let _ = STATUS.compare_exchange(0, qpc(), Ordering::Release, Ordering::Relaxed);
    }
}
pub(super) fn ready(hwnd: HWND) {
    READY.store(qpc(), Ordering::Release);
    if let Some(session) = SESSION.get() {
        let _ = session.ready.try_send(hwnd.0 as isize);
    }
}

pub(super) fn inspect(s: &mut UiState) {
    let Some(session) = SESSION.get() else {
        return;
    };
    let Ok(request) = session.requests.try_recv() else {
        return;
    };
    let begin = qpc();
    let mut error = None;
    match request {
        Request::Inspect => {}
        Request::Drawer => {
            toggle_tracklist(s);
            if let Some(drawer) = s.drawer {
                unsafe {
                    let _ = UpdateWindow(drawer.hwnd);
                }
            }
        }
        Request::Synthetic(raw) => {
            error = s.broker.diagnostic_playlist(&raw).err();
            sync(s);
        }
    }
    let end = qpc();
    let list = s.broker.track_list();
    let (underruns, rendered, pcm_bytes) = s.audio_out.diagnostic_counters();
    let _ = session.replies.try_send(json!({
        "begin": begin, "end": end, "phase": format!("{:?}", s.shared.phase()),
        "generation": s.shared.generation(), "position": s.shared.position().as_secs_f64(),
        "output_key": s.audio_out.diagnostic_output_key(),
        "pcm_primed": s.audio_out.diagnostic_pcm_primed(s.shared.generation()),
        "count": list.titles.len(), "current": list.current.map(|i| i + 1),
        "title": list.current.and_then(|i| list.titles.get(i)), "loading": list.loading,
        "persisted_index": s.broker.persist_snapshot().last_playlist_index,
        "persisted_track": s.broker.persist_snapshot().last_track,
        "underruns": underruns, "rendered_frames": rendered, "pcm_bytes": pcm_bytes,
        "accepted": ACCEPTED.load(Ordering::Acquire), "applied": APPLIED.load(Ordering::Acquire),
        "presented": PRESENTED.load(Ordering::Acquire), "status": STATUS.load(Ordering::Acquire),
        "status_presented": STATUS_PRESENTED.load(Ordering::Acquire),
        "pcm": DELIVERIES.latest(), "record_overflow": DELIVERIES.overflow.load(Ordering::Relaxed), "error": error,
        "callback": FIRST_CALLBACKS.latest(), "callback_overflow": FIRST_CALLBACKS.overflow.load(Ordering::Relaxed),
    }));
}

fn metrics() -> Result<Value, String> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
        Threading::{GetCurrentProcess, GetPriorityClass, GetProcessTimes},
    };
    let mut memory = PROCESS_MEMORY_COUNTERS_EX::default();
    let size = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let (mut created, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            size,
        )
        .map_err(|e| e.to_string())?;
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exit,
            &mut kernel,
            &mut user,
        )
        .map_err(|e| e.to_string())?;
    }
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    Ok(
        json!({"qpc": qpc(), "cpu_ticks": ticks(kernel) + ticks(user),
        "working_set": memory.WorkingSetSize, "private_commit": memory.PrivateUsage,
        "peak_working_set": memory.PeakWorkingSetSize, "peak_pagefile": memory.PeakPagefileUsage,
        "priority_class": unsafe { GetPriorityClass(GetCurrentProcess()) }}),
    )
}

fn command(step: &Value) -> Result<Command, String> {
    let value = step["value"].as_str().unwrap_or("");
    Ok(match step["command"].as_str().unwrap_or("") {
        "load" => Command::Load {
            source: Source::from_raw(value),
            paused: step["paused"].as_bool().unwrap_or(false),
        },
        "drop" => Command::LoadDropped(vec![value.to_string()]),
        "pause" => Command::TogglePause,
        "seek" => Command::SeekRelative(step["seconds"].as_f64().ok_or("seek seconds")?),
        "next" => Command::NextTrack,
        "previous" => Command::PrevTrack,
        "jump" => Command::JumpToTrack(step["index"].as_u64().ok_or("jump index")? as usize),
        "stop" => Command::Stop,
        "quit" => Command::Quit,
        _ => return Err("unknown diagnostic command".into()),
    })
}

fn write_observation(mut writer: impl std::io::Write, row: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut writer, row).map_err(|e| e.to_string())?;
    writer
        .write_all(b"\n")
        .and_then(|()| writer.flush())
        .map_err(|e| e.to_string())
}

pub(crate) fn start() -> Option<std::thread::JoinHandle<()>> {
    let plan_path = std::env::var_os("KYOUKO_APP_PLAN")?;
    let output = std::env::var_os("KYOUKO_APP_RESULT")?;
    let plan: Value = serde_json::from_slice(&std::fs::read(plan_path).ok()?).ok()?;
    // Isolation is mandatory, even for runs which only observe idle.
    let state = crate::config::path();
    if !state.with_file_name("diagnostic-owned").is_file() {
        return None;
    }
    let (request_tx, requests) = crossbeam_channel::bounded(1);
    let (replies, reply_rx) = crossbeam_channel::bounded(1);
    let (ready, ready_rx) = crossbeam_channel::bounded(1);
    SESSION
        .set(Session {
            requests,
            replies,
            ready,
        })
        .ok()?;
    TIMING.store(plan["timing"].as_bool().unwrap_or(true), Ordering::Relaxed);
    Some(std::thread::spawn(move || {
        let mut rows = Vec::new();
        let observation_path =
            std::env::var_os("KYOUKO_APP_OBSERVATIONS").map(std::path::PathBuf::from);
        let mut streamed_observations = 0_u64;
        let result = (|| -> Result<(), String> {
            let hwnd = ready_rx
                .recv_timeout(Duration::from_secs(30))
                .map_err(|e| e.to_string())?;
            // A bounded writer avoids retaining every large snapshot during long runs.
            // Opening, serialization and flushing stay on this finite controller thread.
            let mut observations = observation_path
                .as_ref()
                .map(|path| std::fs::File::create_new(path).map(std::io::BufWriter::new))
                .transpose()
                .map_err(|e| format!("observation file: {e}"))?;
            rows.push(json!({"label": "window_ready", "qpc": READY.load(Ordering::Acquire), "metrics": metrics()?}));
            let request = |r| -> Result<Value, String> {
                request_tx.send(r).map_err(|e| e.to_string())?;
                unsafe {
                    PostMessageW(Some(HWND(hwnd as *mut _)), MESSAGE, WPARAM(0), LPARAM(0))
                        .map_err(|e| e.to_string())?;
                }
                reply_rx
                    .recv_timeout(Duration::from_secs(30))
                    .map_err(|e| e.to_string())
            };
            rows.push(json!({"label": "ui_usable", "snapshot": request(Request::Inspect)?}));
            for step in plan["steps"].as_array().ok_or("steps array")? {
                let begin = qpc();
                let deadline =
                    Instant::now() + Duration::from_secs(step["timeout"].as_u64().unwrap_or(30));
                let kind = step["kind"].as_str().unwrap_or("command");
                if kind == "settle" {
                    std::thread::sleep(Duration::from_secs(step["seconds"].as_u64().unwrap_or(2)));
                }
                if kind == "observe" {
                    std::thread::sleep(Duration::from_secs(2));
                    let snapshot_before = request(Request::Inspect)?;
                    let before = metrics()?;
                    std::thread::sleep(Duration::from_secs(step["seconds"].as_u64().unwrap_or(20)));
                    let after = metrics()?;
                    let snapshot_after = request(Request::Inspect)?;
                    let row = json!({"step": step, "before": before, "after": after,
                        "snapshot_before": snapshot_before, "snapshot_after": snapshot_after});
                    if let Some(writer) = observations.as_mut() {
                        // Export after the measured interval and release this row before the next.
                        write_observation(writer, &row)?;
                        streamed_observations += 1;
                    } else {
                        rows.push(row);
                    }
                    continue;
                }
                let mut snapshot = match kind {
                    "drawer" => request(Request::Drawer)?,
                    "synthetic" => request(Request::Synthetic(
                        std::fs::read(step["value"].as_str().ok_or("metadata path")?)
                            .map_err(|e| e.to_string())?,
                    ))?,
                    "wait" | "settle" => request(Request::Inspect)?,
                    _ => {
                        let cmd = command(step)?;
                        crate::ui::CMD_TX
                            .get()
                            .ok_or("command sender")?
                            .send(cmd)
                            .map_err(|e| e.to_string())?;
                        crate::ui::wake_broker();
                        if step["command"] == "quit" {
                            while TIMING.load(Ordering::Relaxed)
                                && APPLIED.load(Ordering::Acquire) < begin
                                && Instant::now() < deadline
                            {
                                std::thread::sleep(Duration::from_millis(2));
                            }
                            rows.push(json!({"step": step, "begin": begin, "accepted": ACCEPTED.load(Ordering::Acquire),
                                "applied": APPLIED.load(Ordering::Acquire), "end": qpc()}));
                            break;
                        }
                        request(Request::Inspect)?
                    }
                };
                loop {
                    if !snapshot["error"].is_null() {
                        return Err(format!("UI diagnostic action: {}", snapshot["error"]));
                    }
                    let phase_ok = step["phase"]
                        .as_str()
                        .is_none_or(|p| snapshot["phase"] == p);
                    let count_ok = step["count"]
                        .as_u64()
                        .is_none_or(|c| snapshot["count"] == c && snapshot["loading"] == false);
                    let metadata_ok = !step["metadata"].as_bool().unwrap_or(false)
                        || snapshot["loading"] == false;
                    let pcm_ok = !step["pcm"].as_bool().unwrap_or(false)
                        || snapshot["pcm"].as_i64().unwrap_or(0) >= begin;
                    let status_ok = !step["status"].as_bool().unwrap_or(false)
                        || snapshot["status"].as_i64().unwrap_or(0) >= begin;
                    if phase_ok && count_ok && metadata_ok && pcm_ok && status_ok {
                        break;
                    }
                    if Instant::now() >= deadline {
                        return Err(format!("step timeout: {step}; last snapshot: {snapshot}"));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                    snapshot = request(Request::Inspect)?;
                }
                if let Some(index) = step["expected_index"].as_u64()
                    && snapshot["current"] != index
                {
                    return Err(format!("selection mismatch: {snapshot}"));
                }
                if let Some(index) = step["expected_index"].as_u64() {
                    if snapshot["persisted_index"].is_number()
                        && snapshot["persisted_index"] != index
                    {
                        return Err(format!("persistence selection mismatch: {snapshot}"));
                    }
                    if let Some(track) = snapshot["persisted_track"].as_str()
                        && !track.starts_with("https:")
                        && std::path::Path::new(track)
                            .file_name()
                            .and_then(|s| s.to_str())
                            != snapshot["title"].as_str()
                    {
                        return Err(format!("local selected source/title mismatch: {snapshot}"));
                    }
                }
                rows.push(json!({"step": step, "begin": begin, "end": qpc(), "snapshot": snapshot, "metrics": metrics()?}));
            }
            Ok(())
        })();
        if result.is_err()
            && let Some(tx) = crate::ui::CMD_TX.get()
        {
            let _ = tx.send(Command::Quit);
            crate::ui::wake_broker();
        }
        let mut frequency = 0;
        unsafe {
            let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency);
        }
        let data = json!({"pid": std::process::id(), "qpc_frequency": frequency, "rows": rows, "error": result.err(),
            "observation_file": observation_path, "streamed_observations": streamed_observations});
        let _ = std::fs::write(output, serde_json::to_vec_pretty(&data).unwrap());
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observations_round_trip_as_independent_json_lines_and_report_write_errors() {
        let rows = [
            json!({"title":"坂上\nなち", "before":{"private_commit":123}}),
            json!({"step":2}),
        ];
        let mut bytes = Vec::new();
        for row in &rows {
            write_observation(&mut bytes, row).unwrap();
        }
        let decoded: Vec<Value> = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(decoded, rows);
        struct FailingWriter {
            fail_flush: bool,
        }
        impl std::io::Write for FailingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.fail_flush {
                    Ok(bytes.len())
                } else {
                    Err(std::io::Error::other("write failed"))
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("flush failed"))
            }
        }
        assert!(
            write_observation(FailingWriter { fail_flush: false }, &rows[0])
                .unwrap_err()
                .contains("write failed")
        );
        assert!(
            write_observation(FailingWriter { fail_flush: true }, &rows[0])
                .unwrap_err()
                .contains("flush failed")
        );
    }
    #[test]
    fn diagnostic_commands_validate_parameters_without_devices() {
        assert!(matches!(
            command(&json!({"command":"jump", "index":5000})),
            Ok(Command::JumpToTrack(5000))
        ));
        assert!(command(&json!({"command":"seek"})).is_err());
        assert!(command(&json!({"command":"unknown"})).is_err());
    }
    #[test]
    fn first_callback_and_first_pcm_are_independent_boundaries() {
        let callbacks = DeliveryRecords::<1>::new();
        let pcm = DeliveryRecords::<1>::new();
        callbacks.arm(7);
        pcm.arm(7);
        callbacks.record(7, || 100);
        assert_eq!(pcm.latest(), 0, "callback entry need not deliver PCM");
        pcm.record(7, || 200);
        assert_eq!(callbacks.latest(), 100);
        assert_eq!(pcm.latest(), 200);
        callbacks.record(7, || panic!("later callbacks must not read the clock"));
    }
    #[test]
    fn first_delivery_is_generation_fenced_fixed_and_never_reused() {
        let records = DeliveryRecords::<2>::new();
        records.arm(10);
        records.record(9, || panic!("obsolete generation must not read the clock"));
        assert_eq!(records.latest(), 0);
        records.record(10, || 100);
        records.record(10, || panic!("only first delivery reads the clock"));
        assert_eq!(records.latest(), 100);
        records.arm(11);
        records.record(10, || panic!("old callback cannot fill new slot"));
        assert_eq!(records.latest(), 0);
        records.record(11, || 200);
        assert_eq!(records.latest(), 200);
        records.arm(12);
        records.record(12, || panic!("overflow never grows storage"));
        assert_eq!(records.latest(), 0);
        assert_eq!(records.overflow.load(Ordering::Relaxed), 1);
    }
}
