//! Audio pipeline. Step 1 delivers only the scaffolding: the decoder thread's
//! life cycle and the chunk channel whose capacity *is* the backpressure
//! mechanism. Step 2/3 fills `run` with the real pipeline.
//!
//! Final pipeline (this file's shape is already final; only `run` changes):
//!
//! ```text
//! Source::File     → std::fs::File ─┐
//! Source::Youtube  → yt-dlp stdout ─┴→ symphonia packets → f32
//!        → linear resampler (only if src rate ≠ device rate)
//!        → 10-band biquad EQ (coefficients rebuilt only when eq_dirty)
//!        → volume scale → AudioChunk → bounded(16) chunk channel
//!
//! cpal/WASAPI callback (OS-scheduled, event-driven):
//!        try_recv chunk → memcpy to output; silence on underrun; never blocks
//! ```
//!
//! The decoder thread's zero-poll contract: when idle it parks in `cmds.recv()`;
//! mid-track it parks in `select!` between the command channel and the (full)
//! chunk channel — the OS wakes it when *either* has something. It never
//! `loop { try_something(); sleep(); }`s, ever.
//!
//! yt-dlp lifecycle: the child lives inside the decoder thread and is killed
//! (from the broker side via `DecoderCmd::Shutdown` reaching a watcher, plus
//! the decoder's own cleanup) on Stop/Shutdown, so no network-reading orphans.

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::broker::{DecoderCmd, SharedState, Status, CHUNK_CHANNEL_DEPTH};
use crate::log_info;

/// One decoded, EQ'd, volume-applied block of interleaved f32 samples.
/// `CHUNK_FRAMES` frames at the *track's* rate; rate changes travel via
/// `Status::Opened`, never inside chunks.
#[allow(dead_code)] // fields are consumed by the callback in step 3
pub struct AudioChunk {
    pub data: Box<[f32]>,
    pub frames: usize,
    pub channels: u16,
}

pub struct AudioHandles {
    /// Consumed by the cpal stream callback in step 3.
    #[allow(dead_code)]
    pub chunk_rx: Receiver<AudioChunk>,
    pub decoder: JoinHandle<()>,
}

pub fn spawn(
    _shared: Arc<SharedState>,
    cmds: Receiver<DecoderCmd>,
    status_tx: Sender<Status>,
) -> AudioHandles {
    let (chunk_tx, chunk_rx) = bounded::<AudioChunk>(CHUNK_CHANNEL_DEPTH);
    let decoder = thread::Builder::new()
        .name("kyouko-decoder".into())
        .spawn(move || run(_shared, cmds, status_tx, chunk_tx))
        .expect("spawn decoder thread");
    AudioHandles { chunk_rx, decoder }
}

fn run(
    _shared: Arc<SharedState>,
    cmds: Receiver<DecoderCmd>,
    status_tx: Sender<Status>,
    _chunk_tx: Sender<AudioChunk>,
) {
    log_info!("DECODER", "online — parked on command channel (zero CPU)");
    loop {
        match cmds.recv() {
            Ok(DecoderCmd::Load(source)) => {
                log_info!("DECODER", "load request: {source} — pipeline lands in step 2");
                let _ = status_tx.send(Status::Failed {
                    source,
                    reason: "audio pipeline not wired yet (step 2)".into(),
                });
                crate::ui::wake_status();
            }
            Ok(DecoderCmd::Stop) => log_info!("DECODER", "stop (nothing playing)"),
            Ok(DecoderCmd::Shutdown) | Err(_) => break,
        }
    }
    log_info!("DECODER", "offline");
}
