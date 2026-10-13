# kyouko-player

## Synopsis

A high-performance, minimalist Win32 media player designed for global hotkey
operation and resource efficiency. A compact floating overlay provides playback
status, track selection, and equalizer adjustment.

Execution is event driven. Idle and paused playback park the worker threads and
stop the audio output; no polling or animation loop runs. An unfinished network
download continues until its stream buffer is complete.

## Architecture

The broker owns playback state, queues, and persistence. The Win32 UI receives
native messages and dispatches commands. A dedicated decoder thread performs
file and stream reads, Symphonia decoding, equalization, resampling, and volume
processing before handing prepared samples to CPAL/WASAPI.

The real-time sample callback uses preallocated PCM blocks and nonblocking
operations on bounded rings. It performs no heap allocation or deallocation,
file I/O, logging, or application mutex acquisition. Playback controls and
counters use lock-free atomics. The PCM pool holds at least 1.5 seconds of audio
and primes 500 ms before playback; the device uses its native rate and layout.

Each load carries an identity separate from PCM seek generations. Late decoder
events cannot resume a stopped track or alter a newer selection. Native input
is dispatched after its Win32 message handler returns, with Quit taking priority.
Decoder command delivery never blocks the UI; a saturated pipe retains the latest
load/stop request and combines pending seeks.

The overlay renders through a persistent GDI memory DC and bitmap, presenting
the completed image with `UpdateLayeredWindow`. Identical visual snapshots skip
rendering. The elapsed-time timer runs at 1 Hz only while playing.

Release builds select `#![windows_subsystem = "windows"]` conditionally:

```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
```

The release process runs without a console window. Debug builds retain console
logging. The application has no terminal command reader or system tray icon.

## Features

- Fixed 80 px overlay with KYOUKO branding, a compact status indicator,
  elapsed time, track information, volume, EQ/repeat status, and a close button.
- Interactive 10-band Braille equalizer, with gains from -12 to +12 dB.
- Eleven global Win32 hotkeys, active even when the overlay is hidden.
- Tracklist popup with the current selection highlighted and direct track jumps.
- Local audio and audio tracks from supported video containers; YouTube,
  Bilibili, and Niconico streams through `yt-dlp`.
  Niconico selects the best available AAC audio rendition, accommodating
  videos that offer 192 kbps rather than 128 kbps.
- Local folder queues sorted newest first. The last folder and selected track
  are restored at startup, staged paused until playback is requested.
- One automatic retry per failed track, then playlist advancement when a next
  entry is available.
- One compressed stream buffer in RAM, limited to 60 MiB. Seeking uses downloaded
  data; repeating a completely cached stream avoids extraction and download.
- Audio output error recovery and headphone jack hot-swap handling. Recovery
  preserves selection, playback intent, and position where seeking is supported.
  Streaming recovery retains the existing RAM bytes and download worker, avoiding
  new extraction or downloads. Interactive seeks remain limited to bytes already
  in RAM; a recovery seek can wait for bytes from the original producer.
- Atomic persistence of volume, EQ, repeat, folder, and track selection.
- Owned playlist metadata jobs, cancelled on playlist changes or shutdown, with
  a 45-second process deadline and bounded output. Non-finite configuration
  values use defaults; non-finite runtime controls leave settings unchanged.
- Build-time icon generation at 16, 32, 48, and 256 px, embedded as Windows PE
  icon resource ID 1.

## Global hotkeys

| Shortcut | Action |
| --- | --- |
| `Alt + ,` | Seek backward 5 seconds |
| `Alt + .` | Seek forward 5 seconds |
| `Alt + /` | Pause/resume; restart the selected track after playback ends |
| `Alt + Shift + ,` | Previous track |
| `Alt + Shift + .` | Next track |
| `Alt + L` | Toggle repeat |
| `Alt + ;` | Toggle equalizer |
| `Alt + Shift + /` | Hide/show overlay |
| `Alt + Q` | Quit gracefully |
| `Alt + -` | Lower volume by 10 percentage points |
| `Alt + =` | Raise volume by 10 percentage points |

Hotkeys use native `RegisterHotKey`. A binding already reserved by another
application cannot be registered; debug logs report individual conflicts.

## Usage

Launch the executable, then drag a local file, folder, or supported stream link
onto the overlay. An optional startup argument opens a file or URL immediately:

```powershell
.\target\release\kyouko-player.exe "C:\Music\track.mp3"
.\target\release\kyouko-player.exe "https://www.youtube.com/watch?v=DdUoGjniJ7s"
```

Scroll over an equalizer column to adjust that band by 1 dB. Left-click the track
information line to open the tracklist, then select an entry to start it.
Right-click the overlay outside X to pause/resume. Drag the text area to move the
window. Click X to quit; the full click is consumed before shutdown.

Local files use the same extension filter for drops, folder scans, startup
arguments, and restored history: `.mp3`, `.flac`, `.wav`, `.m4a`, `.aac`, `.mp4`,
`.mkv`, `.webm`, and `.ogg`. Other extensions are ignored. Container recognition
does not guarantee codec support: ALAC in M4A/MP4 and Opus in OGG/WebM are
rejected by the decoder. AIFF, CAF, MP1, and MP2 support is not enabled.

Settings are stored in `%LOCALAPPDATA%\kyouko-player\state.cfg`, falling back to
`state.cfg` in the working directory when `LOCALAPPDATA` is unavailable.
Only one player can own a state directory. A second launch exits before
opening audio, logging, or windows. The OS releases the `instance.lock` file
lock after exit or a crash; the empty lock file is deliberately retained.

State changes enter a single pending snapshot slot. A parked worker coalesces
updates, writes a unique temporary file, flushes it, and atomically replaces
`state.cfg`. Quit and Windows session shutdown wait for the final snapshot.
A forced termination can lose pending changes; an interrupted temporary write
leaves the last committed configuration intact.

## Build

Requirements:

- Windows 10 or 11.
- Rust 1.99.0 and Cargo, the toolchain used by Windows CI. OS file locking
  requires Rust 1.89 or newer.
- For the MSVC target, Visual Studio C++ Build Tools and the Windows SDK,
  including the Resource Compiler (`rc.exe`).
- `yt-dlp` on `PATH` for network playback. Local playback has no external
  decoder-process dependency.

```text
cargo build --release
```

The executable is `target/release/kyouko-player.exe`. Icon resources are generated
from repository geometry during compilation; no external icon asset is needed.
Resource compilation failures stop the build. The MSVC build uses SDK `rc.exe`;
GNU targets require a compatible `windres` toolchain.

To retain diagnostic console output:

```text
cargo run
```

Set `KYOUKO_LOG` to `error`, `warn`, `info`, or `debug` to select log verbosity.
Debug builds log to the console. Windows release builds write
`%LOCALAPPDATA%\kyouko-player\player.log` and rotate to
`player.previous.log`, keeping at most 1 MiB in each file. Log producers use a
bounded queue; a full queue drops records and reports the count when space
returns. The writer sleeps between records and drains on graceful shutdown.

Streaming title extraction has a 45-second deadline. A download allows 60
seconds for its first bytes, then fails after 30 seconds without progress.
yt-dlp socket requests time out after 15 seconds with one internal retry.
Timeouts terminate the source process tree and enter the player's existing
two-attempt recovery policy. Completed RAM playback has no deadline worker.

## Verification

```text
cargo check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

`.github/workflows/windows.yml` runs these checks on a fresh Windows 2025
runner using Rust 1.99.0. It also checks the embedded executable icon, release
logging, and duplicate-launch protection. It builds for verification and does
not upload executables or publish packages. Audio tests generate their WAV
and AAC/MP4 samples in memory using Rust; no fixture files or FFmpeg are needed.

The default tests run without network access or an audio device. Diagnostics
that require saved local media, WASAPI output, or live streaming are opt-in
ignored tests.

To test a specific YouTube video or playlist in PowerShell:

```powershell
$env:KYOUKO_TEST_URL = 'https://www.youtube.com/watch?v=DzHqS7QW3V4'
cargo test live_youtube_decodes_audio -- --ignored --nocapture
cargo test saved_track_device_hotswap_cycles_recover_on_wasapi -- --ignored --nocapture
Remove-Item Env:KYOUKO_TEST_URL
```

These diagnostics allow 60 seconds per load, leave saved history unchanged,
and clean up source processes on completion or failure. The playlist diagnostic
loads its titles and advances to entry 2. The recovery diagnostic reopens real
WASAPI streams after four injected output faults; it does not unplug hardware.

For a single video from any supported platform, run
`cargo test live_stream_seeks_and_repeats_from_the_same_ram_buffer -- --ignored --nocapture`
with `KYOUKO_TEST_URL` set to its URL. This diagnostic allows 120 seconds for
download, seeks forward and back in RAM, decodes the entire track and repeat,
and verifies matching PCM, no repeat source process, and cache deallocation.
It consumes PCM without real-time output; the WASAPI diagnostic separately
checks playback timing and recovery on the actual audio device.
`KYOUKO_TEST_FORMAT` optionally forces an exact yt-dlp format for this
seek/repeat diagnostic; it does not change application settings.

## Source map

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | Startup and lifecycle orchestration |
| `src/broker.rs` | Commands, queues, playback state, and persistence policy |
| `src/audio.rs` | Decoding, DSP, PCM pool, and output stream ownership |
| `src/audio_device.rs` | Windows audio endpoint notifications |
| `src/stream_buffer.rs` | Bounded compressed stream cache and seeking |
| `src/config.rs` | State configuration and atomic saves |
| `src/process.rs` | External process launch policy |
| `src/deadline.rs` | Owned, cancellable source deadlines |
| `src/playlist.rs` | Owned Windows playlist metadata jobs and deadlines |
| `src/logging.rs` | Diagnostic logging |
| `src/ui/` | Win32 overlay, tracklist, rendering, and hotkeys |
| `build.rs`, `build/icon.rs` | Icon generation and PE resource embedding |
