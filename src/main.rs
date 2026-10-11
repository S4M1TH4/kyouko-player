#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! kyouko-player — the mountain echo; only heard, never seen.
//!
//! # Thread map (the whole program is this picture)
//!
//! ```text
//!                    Command (bounded 32)              DecoderCmd (bounded 8)
//!   [window / tray / hotkeys] ──────────────▶ [MAIN / broker] ───────────────▶ [decoder thread]
//!    native Win32 messages                   blocked in GetMessageW*          parked in recv()/select!
//!                                             owns: window, tray, Stream
//!                                                    ▲                              │ pooled AudioChunk (bounded)
//!                                                    │ Status (bounded 16)          ▼
//!                                                    └──────────────────── [WASAPI callback thread]
//!                                                step 2: PostMessageW wake     try_recv pop → memcpy out
//! ```
//!
//! (*step 1 runs the same broker on a blocking `select!` loop so the message
//! broker is testable headlessly before any Win32 exists.)
//!
//! # Sleep semantics — there is no polling anywhere
//!
//! | state   | main/broker            | decoder thread                  | WASAPI callback      |
//! |---------|------------------------|---------------------------------|----------------------|
//! | stopped | parked in msg loop     | parked in `recv()`              | not running          |
//! | playing | parked + 1 Hz WM_TIMER | awake: decode → EQ → push       | ~10 ms event-driven  |
//! | paused  | parked, NO timer       | parked (chunk channel is full)  | stopped via pause()  |
//!
//! Every arrow above is a channel send or a Windows message. Zero spinning.
//!
//! # Ownership
//!
//! The broker (main thread) alone owns every OS handle: the 200 px layered
//! window, the tray icon, and the cpal `Stream`. Nothing crosses thread
//! boundaries except messages, so no `Sync` bounds are ever needed on handles.
//!
//! # Shutdown protocol
//!
//! `Quit` → broker sends `DecoderCmd::Shutdown` → decoder kills any yt-dlp
//! child (never orphaned, network stops) → main joins the decoder → exit.
//! Debug-console Ctrl+C / console close use the same graceful shutdown path.
//! Release builds are GUI/tray processes with no console or stdin reader.

mod audio;
mod broker;
mod config;
mod logging;
mod process;
mod stream_buffer;
mod ui;

use crossbeam_channel::bounded;

use std::sync::Arc;

use crate::audio::AudioOut;
use crate::broker::{Broker, Command, DecoderCmd, DecoderLink, SharedState, Source, Status};
use crate::config::PersistedState;
use crate::ui::CMD_TX;

fn main() {
    logging::init();
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

    // The four message arteries. Bounded everywhere: a full queue is the
    // scheduler — senders block (park), nobody ever spins.
    let (cmd_tx, cmd_rx) = bounded::<Command>(32); // window, tray, hotkeys, and startup
    let (decoder_tx, decoder_rx) = bounded::<DecoderCmd>(8); // broker → decoder
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
    let audio = audio::spawn(shared.clone(), decoder_rx, status_tx);
    let audio_out = match device.and_then(|(device, config)| {
        audio::open_output_stream(device, config, audio.chunk_rx, audio.drained_tx, shared.clone())
    }) {
        Ok(stream) => AudioOut::new(Some(stream)),
        Err(e) => {
            log_error!("MAIN", "audio output unavailable: {e} — running silent");
            AudioOut::new(None)
        }
    };

    // The kill switch rides along with every Stop/Load/Shutdown so a stalled
    // yt-dlp pipe read can never defer a command or hang shutdown.
    let controls = audio.controls.clone();
    let link = DecoderLink::with_interrupt(decoder_tx, controls.hook());
    let shutdown_link = link.clone();

    // Persistence sink: the broker calls it on every save trigger
    // (volume / EQ gains / folder ingestion / opened track / Quit). Writes on
    // the broker thread — no locks, no background flusher thread.
    let saver: Arc<dyn Fn(PersistedState) + Send + Sync> =
        Arc::new(|state| config::store(&state));
    let broker = Broker::new(shared, link).with_saver(saver);
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
    log_info!("MAIN", "shutdown complete — echo faded");
}

/// Console control handler: Ctrl+C, Ctrl+Break and terminal-close are turned
/// into a graceful `Quit` so the decoder can kill its yt-dlp child instead of
/// leaving a network-reading orphan behind.
#[cfg(all(windows, debug_assertions))]
unsafe extern "system" fn on_console_ctrl(ctrl: u32) -> windows::core::BOOL {
    use windows::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT};
    if matches!(ctrl, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT) {
        log_warn!("MAIN", "console control event {ctrl} — requesting graceful quit");
        if let Some(tx) = CMD_TX.get() {
            let _ = tx.try_send(Command::Quit);
        }
        crate::ui::wake_broker();
        true.into()
    } else {
        false.into()
    }
}
