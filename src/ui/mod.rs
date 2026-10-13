//! Presentation layer.
//!
//! Step 2 shape — the Win32 pump:
//!
//! ```text
//! create 80px layered window + global hotkeys
//! loop { GetMessageW }                        // parks in the kernel, zero CPU
//!   WM_APP_BROKER  → drain cmd_rx (try_recv)  → broker.handle_command → sync()
//!   WM_APP_STATUS  → drain status_rx          → broker.handle_status  → sync()
//!   WM_APP_OUTPUT  → default-device recovery on the owning/UI thread → sync()
//!   WM_TIMER (1 Hz, ONLY while Playing) → broker.tick() → sync()
//! ```
//!
//! Producers push to a channel, then post one wake message (`wake_broker` /
//! `wake_status`): channels carry the data, a single posted message carries
//! the wake-up. When not playing the timer is killed, so the main thread
//! parks in `GetMessageW` with zero scheduled wake-ups.
//!
//! `sync()` is the only bridge between the platform and the pure core: it
//! starts/stops the timer on phase transitions and repaints when the broker's
//! panel is dirty. Everything else is message plumbing.

mod headless;
#[cfg(windows)]
mod render;
#[cfg(windows)]
mod win32;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(test, not(windows), debug_assertions))]
use std::sync::atomic::AtomicU32;

use crossbeam_channel::{Receiver, Sender};

use crate::audio::AudioOut;
use crate::broker::{Broker, Command, Status};

/// Write end of the command channel, parked in a static so UI interactions
/// and the console control handler (both outside the normal producer path)
/// can inject commands. `main` fills it before spawning any threads.
pub static CMD_TX: OnceLock<Sender<Command>> = OnceLock::new();

static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(all(windows, debug_assertions))]
pub fn request_quit() {
    QUIT_REQUESTED.store(true, Ordering::Release);
    if let Some(tx) = CMD_TX.get() { let _ = tx.try_send(Command::Quit); }
    wake_broker();
}

pub fn take_quit_request() -> bool { QUIT_REQUESTED.swap(false, Ordering::AcqRel) }

#[cfg(windows)]
#[derive(Default)]
struct LocalInbox {
    commands: std::collections::VecDeque<Command>,
    quit: bool,
}

#[cfg(windows)]
thread_local! {
    // Native input already runs on the owner thread. Stage it until the
    // message handler returns, rather than competing for a full worker pipe.
    static LOCAL_INBOX: std::cell::RefCell<LocalInbox> = std::cell::RefCell::new(LocalInbox::default());
}

#[cfg(windows)]
pub fn queue_local(command: Command) {
    LOCAL_INBOX.with_borrow_mut(|inbox| {
        if matches!(command, Command::Quit) { inbox.quit = true; }
        else if !inbox.quit { inbox.commands.push_back(command); }
    });
    wake_broker();
}

#[cfg(windows)]
pub fn take_local() -> Option<Command> {
    LOCAL_INBOX.with_borrow_mut(|inbox| {
        if std::mem::take(&mut inbox.quit) {
            inbox.commands.clear();
            Some(Command::Quit)
        } else { inbox.commands.pop_front() }
    })
}

#[cfg(windows)]
pub fn has_local() -> bool {
    LOCAL_INBOX.with_borrow(|inbox| inbox.quit || !inbox.commands.is_empty())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn native_controls_keep_fifo_order_without_using_the_worker_channel() {
        queue_local(Command::SeekRelative(-5.0));
        queue_local(Command::ToggleLoop);
        queue_local(Command::VolumeStep(10));
        assert!(matches!(take_local(), Some(Command::SeekRelative(-5.0))));
        assert!(matches!(take_local(), Some(Command::ToggleLoop)));
        assert!(matches!(take_local(), Some(Command::VolumeStep(10))));
        assert!(take_local().is_none());
    }

    #[test]
    fn quit_has_priority_over_staged_native_commands_even_with_a_full_worker_pipe() {
        let (tx, rx) = crossbeam_channel::bounded(2);
        tx.send(Command::ToggleLoop).unwrap();
        tx.send(Command::ToggleEq).unwrap();
        queue_local(Command::SeekRelative(5.0));
        queue_local(Command::Quit);
        queue_local(Command::VolumeStep(10));
        assert!(rx.is_full());
        assert!(matches!(take_local(), Some(Command::Quit)));
        assert!(take_local().is_none());
        assert_eq!(rx.len(), 2);
    }
}

// WM_APP + 1..3. Kept as literals so the constants exist on every target;
// win32.rs uses them as channel wake-up messages.
pub const WM_APP_BROKER: u32 = 0x8001;
pub const WM_APP_STATUS: u32 = 0x8002;
pub const WM_APP_OUTPUT: u32 = 0x8003;

static PUMP_HWND: OnceLock<isize> = OnceLock::new();

/// Called by the pump once its window exists. Until then wake-ups are no-ops;
/// the pump performs an initial channel drain after creation, so nothing is
/// ever lost in between.
pub fn set_pump_hwnd(hwnd: isize) {
    PUMP_HWND.set(hwnd).ok();
}

/// window / global hotkeys / debug Ctrl+C → broker.
pub fn wake_broker() {
    post(WM_APP_BROKER);
}

/// decoder thread → broker.
pub fn wake_status() {
    post(WM_APP_STATUS);
}

/// CPAL errors and Core Audio endpoint notifications → owning UI thread.
pub fn wake_output() {
    post(WM_APP_OUTPUT);
}

// The diagnostic pump consumes the same posted wakes as the native owner.
// Its one-bit mailbox replaces callback-side Crossbeam notification too.
#[cfg(any(test, not(windows), debug_assertions))]
static HEADLESS_ACTIVE: AtomicBool = AtomicBool::new(false);
#[cfg(any(test, not(windows), debug_assertions))]
static HEADLESS_WAKE: AtomicU32 = AtomicU32::new(0);

#[cfg(any(test, not(windows), debug_assertions))]
pub(crate) struct HeadlessWakeGuard;
#[cfg(any(test, not(windows), debug_assertions))]
impl HeadlessWakeGuard {
    pub(crate) fn new() -> Self {
        HEADLESS_ACTIVE.store(true, Ordering::Release);
        Self
    }
}
#[cfg(any(test, not(windows), debug_assertions))]
impl Drop for HeadlessWakeGuard {
    fn drop(&mut self) { HEADLESS_ACTIVE.store(false, Ordering::Release); }
}

#[cfg(any(test, not(windows), debug_assertions))]
fn wake_headless() {
    if HEADLESS_ACTIVE.load(Ordering::Acquire) && HEADLESS_WAKE.swap(1, Ordering::Release) == 0 {
        atomic_wait::wake_one(&HEADLESS_WAKE);
    }
}

#[cfg(any(not(windows), debug_assertions))]
pub(crate) fn wait_headless() {
    if HEADLESS_WAKE.swap(0, Ordering::Acquire) == 0 {
        atomic_wait::wait(&HEADLESS_WAKE, 0);
        HEADLESS_WAKE.swap(0, Ordering::Acquire);
    }
}

#[cfg(all(test, windows))]
pub(crate) fn wait_headless_timeout(timeout: std::time::Duration) {
    if HEADLESS_WAKE.swap(0, Ordering::Acquire) == 0 {
        let expected = 0u32;
        unsafe {
            let _ = windows::Win32::System::Threading::WaitOnAddress(
                (&HEADLESS_WAKE as *const AtomicU32).cast(), (&expected as *const u32).cast(), size_of::<u32>(),
                Some(timeout.as_millis().clamp(1, u32::MAX as u128 - 1) as u32));
        }
        HEADLESS_WAKE.swap(0, Ordering::Acquire);
    }
}

#[cfg(windows)]
fn post(msg: u32) {
    #[cfg(any(test, debug_assertions))]
    wake_headless();
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
    if let Some(&h) = PUMP_HWND.get() {
        unsafe {
            // Invalid/destroyed hwnd just fails harmlessly — wake-ups racing
            // shutdown are ignored, the channel data stays queued.
            let _ = PostMessageW(Some(HWND(h as *mut _)), msg, WPARAM(0), LPARAM(0));
        }
    }
}

#[cfg(not(windows))]
fn post(_msg: u32) { wake_headless(); }

pub fn run(
    broker: Broker,
    cmd_rx: Receiver<Command>,
    status_rx: Receiver<Status>,
    audio_out: AudioOut,
) {
    // Debug escape hatch: the step-1 headless pump, kept for CI-style runs.
    if cfg!(debug_assertions) && std::env::var_os("KYOUKO_HEADLESS").is_some() {
        headless::run(broker, cmd_rx, status_rx, audio_out);
        return;
    }
    #[cfg(windows)]
    win32::run(broker, cmd_rx, status_rx, audio_out);
    #[cfg(not(windows))]
    headless::run(broker, cmd_rx, status_rx, audio_out);
}
