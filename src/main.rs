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
mod logging;
mod terminal;
mod ui;

use std::sync::OnceLock;

use crossbeam_channel::bounded;

use crate::broker::{Broker, Command, DecoderCmd, SharedState, Status};

/// Write end of the command channel, kept in a static so the console control
/// handler (which runs on an OS-spawned thread) can inject a `Quit`.
static CMD_TX: OnceLock<crossbeam_channel::Sender<Command>> = OnceLock::new();

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

    let shared = SharedState::new();

    // The four message arteries. Bounded everywhere: a full queue is the
    // scheduler — senders block (park), nobody ever spins.
    let (cmd_tx, cmd_rx) = bounded::<Command>(32); // stdin (→ step 2: tray menu too)
    let (decoder_tx, decoder_rx) = bounded::<DecoderCmd>(8); // broker → decoder
    let (status_tx, status_rx) = bounded::<Status>(16); // decoder → broker
    CMD_TX.set(cmd_tx.clone()).ok();

    let audio = audio::spawn(shared.clone(), decoder_rx, status_tx);
    let _terminal = terminal::spawn(cmd_tx);

    // Keep a clone so shutdown can reach the decoder even if the broker loop
    // exits through an error path instead of a Quit.
    let decoder_tx_shutdown = decoder_tx.clone();

    let broker = Broker::new(shared, decoder_tx);
    log_info!("MAIN", "broker online — entering sleep-on-idle event loop");
    ui::run(broker, cmd_rx, status_rx);

    log_info!("MAIN", "event loop over — reaping decoder");
    let _ = decoder_tx_shutdown.try_send(DecoderCmd::Shutdown);
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
        true.into()
    } else {
        false.into()
    }
}
