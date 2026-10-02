//! Cross-platform headless pump — the step-1 event loop, preserved verbatim.
//! Reached on non-Windows targets and via `KYOUKO_HEADLESS=1` for CI-style
//! debugging with no window, no tray, no audio device.

use crossbeam_channel::{select, Receiver};

use crate::broker::{Broker, Command, Flow, Status};
use crate::{log_info, log_warn};

pub fn run(mut broker: Broker, cmd_rx: Receiver<Command>, status_rx: Receiver<Status>) {
    log_info!("UI", "headless mode — panel follows on every state change");
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
