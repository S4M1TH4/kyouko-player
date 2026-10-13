//! Debug/non-Windows headless diagnostics. Windows release builds require
//! a usable GUI; a failed window initialization shuts down gracefully.

#[cfg(any(not(windows), debug_assertions))]
use crossbeam_channel::{select, Receiver};
#[cfg(all(windows, not(debug_assertions)))]
use crossbeam_channel::Receiver;

use crate::audio::AudioOut;
use crate::broker::{Broker, Command, Status};
#[cfg(any(not(windows), debug_assertions))]
use crate::broker::{Flow, Phase};
#[cfg(any(not(windows), debug_assertions))]
use crate::{log_info, log_warn};

pub fn run(
    broker: Broker,
    cmd_rx: Receiver<Command>,
    status_rx: Receiver<Status>,
    audio_out: AudioOut,
) {
    #[cfg(all(windows, not(debug_assertions)))]
    {
        let mut broker = broker;
        crate::log_error!("UI", "GUI unavailable — shutting down");
        broker.handle_command(Command::Quit);
        drop((cmd_rx, status_rx, audio_out));
    }
    #[cfg(any(not(windows), debug_assertions))]
    run_debug(broker, cmd_rx, status_rx, audio_out);
}

#[cfg(any(not(windows), debug_assertions))]
fn run_debug(
    mut broker: Broker,
    cmd_rx: Receiver<Command>,
    status_rx: Receiver<Status>,
    mut audio_out: AudioOut,
) {
    let shared = broker.shared();
    audio_out.start_notifications();
    let output_events = audio_out.event_receiver();
    let background_events = broker.background_events();
    let mut last_phase = shared.phase();
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
                    // All command producers gone: treat as shutdown.
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
            recv(output_events) -> _ => audio_out.handle_device_events(&mut broker),
            recv(background_events) -> _ => broker.poll_background(),
        }
        if super::take_quit_request() {
            broker.handle_command(Command::Quit);
            break;
        }
        audio_out.handle_device_events(&mut broker);
        // Mirror the phase onto the output stream (WASAPI play/pause).
        let phase = shared.phase();
        if phase != last_phase {
            audio_out.set_playing(phase == Phase::Playing);
            last_phase = phase;
        }
        // The broker is the only thing allowed to decide the panel changed.
        if let Some(panel) = broker.take_refresh() {
            println!("\n{panel}\n");
        }
    }
}
