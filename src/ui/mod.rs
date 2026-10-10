//! Presentation layer.
//!
//! Step 2 shape — the Win32 pump:
//!
//! ```text
//! create 80px layered window + procedural tray icon
//! loop { GetMessageW }                        // parks in the kernel, zero CPU
//!   WM_APP_BROKER  → drain cmd_rx (try_recv)  → broker.handle_command → sync()
//!   WM_APP_STATUS  → drain status_rx          → broker.handle_status  → sync()
//!   WM_APP_TRAY    → left-click: pause/resume; right-click: Show/Hide Echo + Quit
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

pub mod glyph;
mod headless;
#[cfg(windows)]
mod render;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod win32;

use std::sync::OnceLock;

use crossbeam_channel::{Receiver, Sender};

use crate::audio::AudioOut;
use crate::broker::{Broker, Command, Status};

/// Write end of the command channel, parked in a static so UI interactions
/// and the console control handler (both outside the normal producer path)
/// can inject commands. `main` fills it before spawning any threads.
pub static CMD_TX: OnceLock<Sender<Command>> = OnceLock::new();

// WM_APP + 1..3. Kept as literals so the constants exist on every target;
// win32.rs uses them as window messages, tray.rs as the callback message.
pub const WM_APP_BROKER: u32 = 0x8001;
pub const WM_APP_STATUS: u32 = 0x8002;
pub const WM_APP_TRAY: u32 = 0x8003;

static PUMP_HWND: OnceLock<isize> = OnceLock::new();

/// Called by the pump once its window exists. Until then wake-ups are no-ops;
/// the pump performs an initial channel drain after creation, so nothing is
/// ever lost in between.
pub fn set_pump_hwnd(hwnd: isize) {
    PUMP_HWND.set(hwnd).ok();
}

/// stdin thread / tray / Ctrl+C → broker.
pub fn wake_broker() {
    post(WM_APP_BROKER);
}

/// decoder thread → broker.
pub fn wake_status() {
    post(WM_APP_STATUS);
}

#[cfg(windows)]
fn post(msg: u32) {
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
fn post(_msg: u32) {}

pub fn run(
    broker: Broker,
    cmd_rx: Receiver<Command>,
    status_rx: Receiver<Status>,
    audio_out: AudioOut,
) {
    // Debug escape hatch: the step-1 headless pump, kept for CI-style runs.
    if std::env::var_os("KYOUKO_HEADLESS").is_some() {
        headless::run(broker, cmd_rx, status_rx, audio_out);
        return;
    }
    #[cfg(windows)]
    win32::run(broker, cmd_rx, status_rx, audio_out);
    #[cfg(not(windows))]
    headless::run(broker, cmd_rx, status_rx, audio_out);
}
