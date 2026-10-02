//! kyouko-player — the mountain echo; only heard, never seen.
//!
//! # Thread map (the whole program is this picture)
//!
//! ```text
//!                    Command (bounded 32)              DecoderCmd (bounded 8)
//!   [stdin thread] ─────────────────────────▶ [MAIN / broker] ───────────────▶ [decoder thread]
//!    blocked in ReadFile, parks on EOF        blocked in GetMessageW*          parked in recv()/select!
//!                                             owns: window, tray, Stream
//!                                                    ▲                              │ AudioChunk (bounded 16)
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
//! | state   | stdin thread | main/broker            | decoder thread                  | WASAPI callback      |
//! |---------|--------------|------------------------|---------------------------------|----------------------|
//! | stopped | parked       | parked in msg loop     | parked in `recv()`              | not running          |
//! | playing | parked       | parked + 1 Hz WM_TIMER | awake: decode → EQ → push       | ~10 ms event-driven  |
//! | paused  | parked       | parked, NO timer       | parked (chunk channel is full)  | stopped via pause()  |
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
//! Ctrl+C / terminal close are intercepted and routed through the same path.

mod audio;
mod broker;
mod config;
mod logging;
mod terminal;
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

    #[cfg(windows)]
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
    for (band, gain) in restored.eq_gains.iter().enumerate() {
        shared.set_eq_gain(band, *gain);
    }
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
        "restored: volume {:.0}%, eq ({}), last_track {}",
        restored.volume * 100.0,
        eq_desc,
        restored.last_track.as_deref().unwrap_or("<none>")
    );

    // The four message arteries. Bounded everywhere: a full queue is the
    // scheduler — senders block (park), nobody ever spins.
    let (cmd_tx, cmd_rx) = bounded::<Command>(32); // stdin (→ step 2: tray menu too)
    let (decoder_tx, decoder_rx) = bounded::<DecoderCmd>(8); // broker → decoder
    let (status_tx, status_rx) = bounded::<Status>(16); // decoder → broker
    CMD_TX.set(cmd_tx.clone()).ok();

    // Launch track: an explicit CLI argument plays right away; otherwise a
    // persisted last_track is STAGED in the decoder — buffered, resume is
    // instant — with the output left stopped (paused, 0% CPU).
    let initial = match std::env::args().nth(1) {
        Some(arg) => Some((Source::from_raw(&arg), false)),
        None => restored
            .last_track
            .as_deref()
            .map(Source::from_raw)
            .map(|s| (s, true)),
    };
    if let Some((source, paused)) = initial {
        log_info!(
            "MAIN",
            "launch track: {source} ({})",
            if paused { "staged paused" } else { "autoplay" }
        );
        let _ = cmd_tx.send(Command::Load { source, paused });
    }

    let audio = audio::spawn(shared.clone(), decoder_rx, status_tx);
    let audio_out = match audio::open_output_stream(audio.chunk_rx, audio.drained_tx, shared.clone()) {
        Ok(stream) => AudioOut::new(Some(stream)),
        Err(e) => {
            log_error!("MAIN", "audio output unavailable: {e} — running silent");
            AudioOut::new(None)
        }
    };
    let _terminal = terminal::spawn(cmd_tx);

    // The kill switch rides along with every Stop/Load/Shutdown so a stalled
    // yt-dlp pipe read can never defer a command or hang shutdown.
    let controls = audio.controls.clone();
    let link = DecoderLink::with_interrupt(decoder_tx, controls.hook());
    let shutdown_link = link.clone();

    // Persistence sink: the broker calls it on every save trigger
    // (volume / EQ gains / opened track / Quit). Fire-and-forget writes on
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
    // Returning from main terminates the parked stdin thread; that is fine.
}

/// Console control handler: Ctrl+C, Ctrl+Break and terminal-close are turned
/// into a graceful `Quit` so the decoder can kill its yt-dlp child instead of
/// leaving a network-reading orphan behind.
#[cfg(windows)]
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
