//! Presentation layer. Step 1: headless broker loop — the *same* broker the
//! Win32 module will drive, running on a blocking `select!` so the entire
//! message broker is testable before a single line of Win32 exists.
//!
//! Step 2 swaps this function's body for the real message loop; its shape is
//! already decided:
//!
//! ```text
//! create 200px layered window + tray icon
//! loop { GetMessageW }                       // parks in the kernel, zero CPU
//!   WM_APP_BROKER  → drain cmd_rx with try_recv → broker.handle_command
//!   WM_APP_STATUS  → drain status_rx           → broker.handle_status
//!   WM_APP_TRAY    → tray clicks: left = show/hide, right = menu (quit)
//!   WM_TIMER (1 Hz, ONLY while Playing) → read frames_played → refresh text
//!   WM_PAINT / redraw  → blit cached panel; only ever on state change
//! ```
//!
//! Producers wake the loop with `PostMessageW(hwnd, WM_APP_*)` after pushing
//! to a channel — the standard "message-only integrator" pattern: channels
//! carry data, one posted message carries the wake-up. When paused the timer
//! is killed, so the main thread parks in GetMessageW with literally zero
//! scheduled wake-ups.

use crossbeam_channel::{select, Receiver};

use crate::broker::{Broker, Command, Flow, Status};
use crate::log_info;
use crate::log_warn;

pub fn run(mut broker: Broker, cmd_rx: Receiver<Command>, status_rx: Receiver<Status>) {
    log_info!("UI", "headless mode (Win32 window lands in step 2) — panel follows");
    loop {
        // Blocks until a command or a status arrives. Thread parks in the
        // kernel between events — this loop *is* the zero-CPU idle state.
        select! {
            recv(cmd_rx) -> msg => match msg {
                Ok(cmd) => {
                    if broker.handle_command(cmd) == Flow::Exit {
                        break;
                    }
                }
                Err(_) => {
                    // All Command senders gone: stdin died and the static
                    // handle was never claimed. Treat as shutdown.
                    log_warn!("UI", "command channel closed — exiting");
                    break;
                }
            },
            recv(status_rx) -> msg => match msg {
                Ok(status) => broker.handle_status(status),
                Err(_) => {
                    log_warn!("UI", "status channel closed (decoder died) — exiting");
                    break;
                }
            },
        }
        // The broker is the only thing allowed to decide the panel changed.
        if let Some(panel) = broker.take_refresh() {
            println!("\n{panel}\n");
        }
    }
}
