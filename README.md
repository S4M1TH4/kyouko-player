# kyouko-player

A zero-resource, event-driven audio player and OS media broker written in Rust. `kyouko-player` runs as a background process with a lightweight Win32 system tray handle and a minimalist transparent status window, consuming minimal CPU and RAM.

The application contains no heavy UI framework overhead, no text rendering animations, and no album art decoders. When playback is paused or idle, execution threads yield completely to native Windows synchronization primitives (`MsgWaitForMultipleObjects` / `WaitMessage`).

## Technical Architecture

* **Event Loop:** Thread blocking relies strictly on native OS message queues rather than polling timeouts (`loop { sleep() }`).
* **Audio Pipeline:** Decodes media streams via native decoders directly into the `cpal` audio output buffer.
* **Stream Piping:** Streams remote media directly from `yt-dlp` stdout buffering without intermediate disk writes.
* **Signal Processing:** Implements an in-memory software DSP chain for multiband biquad equalization.

## Features

* Sub-1% CPU usage and sub-15MB RAM footprint on Windows 11.
* Direct local audio and video file decoding (extracts audio tracks from container formats).
* Headless YouTube stream piping via `yt-dlp` integration (`--ytdl-format=140`).
* Real-time ASCII signal and EQ status logging in terminal stdout for debugging.
* In-memory biquad equalizer controlled via terminal commands.
* Global media key shortcuts via Windows System Media Transport Controls (SMTC).

## Requirements

### Prerequisites

* **OS:** Windows 11 / Windows 10 (x86_64)
* **Rust Toolchain:** `rustc` 1.99+ and `cargo`
* **External Dependency:** `yt-dlp` (must be accessible in system `PATH` for remote stream support)

## Building from Source

Clone the repository and build using Cargo:

```powershell
git clone [https://github.com/S4M1TH4/kyouko-player.git](https://github.com/S4M1TH4/kyouko-player.git)
cd kyouko-player
cargo build --release
```

The compiled executable will be located at `target/release/kyouko-player.exe`.

## Usage

### Local File Playback

Pass local file paths directly via command-line arguments:

```powershell
.\target\release\kyouko-player.exe "C:\Path\To\audio.flv"
```

### YouTube Streaming

Stream audio directly from YouTube URLs:

```powershell
.\target\release\kyouko-player.exe "[https://www.youtube.com/watch?v=DdUoGjniJ7s](https://www.youtube.com/watch?v=DdUoGjniJ7s)"
```

### CLI Terminal Commands

When running in an interactive terminal, control playback state and DSP parameters using standard keyboard input:

* `space` — Toggle Play / Pause
* `s` — Stop Playback
* `eq <band> <gain_db>` — Adjust Biquad EQ band gain (e.g., `eq 60 +3.0`)
* `eq reset` — Reset all equalizer bands to flat (0 dB)
* `q` or `Ctrl+C` — Terminate process and release system resources

## Project Structure

* `src/main.rs` — Application entrypoint and signal orchestrator
* `src/audio/` — Decoders, ring buffers, and output stream engine
* `src/dsp/` — Biquad filter implementation for EQ processing
* `src/platform/` — Win32 API wrappers, system tray, and OS event loop
* `src/sources/` — File stream reader and `yt-dlp` stdin pipe wrapper