#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! kyouko-player — the mountain echo; only heard, never seen.
//!
//! # Thread map (the whole program is this picture)
//!
//! ```text
//!                    Command (bounded 32)              DecoderCmd (bounded 8)
//!   [window / hotkeys] ────────────────────▶ [MAIN / broker] ───────────────▶ [decoder thread]
//!    native Win32 messages                   blocked in GetMessageW*          parked in address wait
//!                                             owns: window, Stream
//!                                                    ▲                              │ pooled AudioChunk (bounded)
//!                                                    │ Status (bounded 16)          ▼
//!                                                    └──────────────────── [decoder sends status]
//!                                                                         [WASAPI callback thread]
//!                                                                            PCM claim → copy/convert
//! ```
//!
//! (*headless diagnostics run the same broker on an atomic wake mailbox so the
//! broker is testable headlessly before any Win32 exists.)
//!
//! # Sleep semantics — there is no polling anywhere
//!
//! | state   | main/broker            | decoder thread                  | WASAPI callback      |
//! |---------|------------------------|---------------------------------|----------------------|
//! | stopped | parked in msg loop     | parked in address wait          | not running          |
//! | playing | parked + 1 Hz WM_TIMER | awake: decode → EQ → push       | ~10 ms event-driven  |
//! | paused  | parked, NO timer       | parked (PCM capacity is full)   | stopped via pause()  |
//!
//! PCM uses bounded atomic rings; commands publish before an address wake.
//!
//! # Ownership
//!
//! The broker (main thread) alone owns every OS handle: the 80 px layered
//! window and the cpal `Stream`. Nothing crosses thread
//! boundaries except messages, so no `Sync` bounds are ever needed on handles.
//!
//! # Shutdown protocol
//!
//! `Quit` → broker sends `DecoderCmd::Shutdown` → decoder kills any yt-dlp
//! child (never orphaned, network stops) → main joins the decoder → exit.
//! Debug-console Ctrl+C / console close use the same graceful shutdown path.
//! Release builds are GUI overlay processes with no console or stdin reader.

mod audio;
mod decoder_wake;
mod pcm_queue;
#[cfg(windows)]
mod audio_device;
mod broker;
mod config;
mod deadline;
mod logging;
mod process;
#[cfg(windows)]
mod playlist;
mod stream_buffer;
mod ui;

#[cfg(test)]
#[path = "../build/icon.rs"]
mod executable_icon;

use crossbeam_channel::bounded;

use std::sync::Arc;

use crate::audio::AudioOut;
use crate::broker::{Broker, Command, DecoderCmd, DecoderLink, SharedState, Source, Status};
use crate::ui::CMD_TX;

fn main() {
    #[cfg(all(windows, feature = "release-diagnostics"))]
    let diagnostic = ui::performance::start();
    // Claim the state directory before logging, restoration or device setup.
    // A duplicate launch must not touch another instance's settings/logs/hotkeys.
    let _instance = match config::InstanceGuard::acquire(&config::path()) {
        Ok(Some(guard)) => guard,
        Ok(None) => { eprintln!("kyouko-player is already running"); return; }
        Err(error) => {
            let _logging = logging::init();
            log_error!("MAIN", "cannot acquire instance lock: {error}");
            return;
        }
    };
    let _logging = logging::init();
    log_info!(
        "MAIN",
        "kyouko-player {} — the mountain echo; only heard, never seen",
        env!("CARGO_PKG_VERSION")
    );

    #[cfg(all(windows, debug_assertions))]
    unsafe {
        // Non-ASCII titles must survive the debug log.
        use windows::Win32::System::Console::SetConsoleOutputCP;
        let _ = SetConsoleOutputCP(65001); // CP_UTF8
        use windows::Win32::System::Console::SetConsoleCtrlHandler;
        if SetConsoleCtrlHandler(Some(on_console_ctrl), true).is_err() {
            log_warn!("MAIN", "SetConsoleCtrlHandler failed — Ctrl+C will be abrupt");
        }
    }

    // Restore persisted state before anything can play: volume and EQ apply
    // to the shared atomics immediately, so even the first track inherits them.
    let restored = config::load();
    let shared = SharedState::new();
    shared.set_volume(restored.volume);
    shared.set_eq_enabled(restored.eq_enabled);
    for (band, gain) in restored.eq_gains.iter().enumerate() {
        shared.set_eq_gain(band, *gain);
    }
    shared.set_loop_enabled(restored.loop_enabled);
    let eq_desc = {
        let mut parts = Vec::new();
        for (band, gain) in restored.eq_gains.iter().enumerate() {
            if gain.abs() >= 0.05 {
                parts.push(format!("{}Hz {:+.1}", crate::broker::EQ_BAND_HZ[band], gain));
            }
        }
        if parts.is_empty() {
            "flat".to_string()
        } else {
            parts.join(", ")
        }
    };
    log_info!(
        "MAIN",
        "restored: volume {:.0}%, eq {} ({}), loop {}, last_track {}, last_folder {}",
        restored.volume * 100.0,
        if restored.eq_enabled { "ON" } else { "OFF" },
        eq_desc,
        if restored.loop_enabled { "ON" } else { "OFF" },
        restored.last_track.as_deref().unwrap_or("<none>"),
        restored.last_folder.as_deref().unwrap_or("<none>")
    );

    // Core cross-thread arteries are bounded. UI-native input is staged on
    // its owner thread; decoder dispatch coalesces under saturation.
    let (cmd_tx, cmd_rx) = bounded::<Command>(32); // window, hotkeys, and startup
    let (decoder_tx, decoder_rx) = decoder_wake::command_channel(8); // broker → decoder
    let (status_tx, status_rx) = bounded::<Status>(16); // decoder → broker
    CMD_TX.set(cmd_tx.clone()).ok();

    // An explicit startup file/URL takes precedence. Otherwise the broker rebuilds
    // the saved folder queue and stages its last track paused (0% CPU idle).
    if let Some(arg) = std::env::args().nth(1) {
        let source = Source::from_raw(&arg);
        log_info!("MAIN", "launch track: {source} (autoplay)");
        let _ = cmd_tx.send(Command::Load { source, paused: false });
    } else if restored.last_folder.is_some() || restored.last_track.is_some() {
        let _ = cmd_tx.send(Command::RestoreState {
            last_folder: restored.last_folder,
            last_track: restored.last_track,
            last_playlist_index: restored.last_playlist_index,
        });
    }

    // Fix the device layout before allocating the PCM pool or starting decode.
    let device = audio::output_device(&shared);
    if device.is_err() {
        // A disconnected startup still needs a valid provisional PCM clock.
        shared.set_sample_rate(48_000);
        shared.set_device_channels(2);
    }
    let audio = audio::spawn(shared.clone(), decoder_rx.clone(), status_tx);
    let audio_out = AudioOut::managed(device, audio.chunk_rx, audio.drained_tx, shared.clone(), audio.controls.clone());

    // The kill switch rides along with every Stop/Load/Shutdown so a stalled
    // yt-dlp pipe read can never defer a command or hang shutdown.
    let controls = audio.controls.clone();
    let link = DecoderLink::with_interrupt(decoder_tx, decoder_rx.clone(), controls.clone().hook())
        .with_suspend(controls.suspend_hook());
    let shutdown_link = link.clone();

    // Coalesce snapshots outside the UI. The writer parks when nothing changes
    // and its guard drains the latest snapshot before releasing the instance lock.
    let persistence = config::SaveWriter::start(config::path()).ok();
    let saver = if let Some(writer) = &persistence { writer.saver() } else {
        log_warn!("CFG", "cannot start state writer; saving synchronously");
        Arc::new(|state| config::store(&state))
    };
    let mut broker = Broker::new(shared, link).with_saver(saver).with_playlist_fetching();
    if let Some(writer) = &persistence { broker = broker.with_flush(writer.flusher()); }
    log_info!("MAIN", "broker online — entering sleep-on-idle event loop");
    ui::run(broker, cmd_rx, status_rx, audio_out);

    log_info!("MAIN", "event loop over — reaping decoder");
    // Defensive shutdown for error-path exits; skipped when the decoder
    // already exited (Quit flow sends its own Shutdown).
    if !audio.decoder.is_finished() {
        shutdown_link.send(DecoderCmd::Shutdown);
    }
    audio.controls.interrupt();
    let _ = audio.decoder.join();
    drop(persistence);
    log_info!("MAIN", "shutdown complete — echo faded");
    #[cfg(all(windows, feature = "release-diagnostics"))]
    if let Some(diagnostic) = diagnostic { let _ = diagnostic.join(); }
}

/// Console control handler: Ctrl+C, Ctrl+Break and terminal-close are turned
/// into a graceful `Quit` so the decoder can kill its yt-dlp child instead of
/// leaving a network-reading orphan behind.
#[cfg(all(windows, debug_assertions))]
unsafe extern "system" fn on_console_ctrl(ctrl: u32) -> windows::core::BOOL {
    use windows::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT};
    if matches!(ctrl, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT) {
        log_warn!("MAIN", "console control event {ctrl} — requesting graceful quit");
        crate::ui::request_quit();
        true.into()
    } else {
        false.into()
    }
}
