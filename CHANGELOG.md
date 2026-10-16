# Changelog

## 0.1.0 — Unreleased

Planned initial Windows release. The version is defined in `Cargo.toml`.

### Features

- Compact floating overlay, global hotkeys, drag-and-drop loading and a tracklist
  popup with direct track selection.
- Local audio and folder queues, plus YouTube, Bilibili and Niconico playback
  through `yt-dlp`.
- Ten-band equalizer, volume control, seeking and single-track repeat.
- Saved settings and track/folder selection, restored paused at startup.
- Seeking and repeating downloaded streams from a bounded RAM buffer.
- Audio-device change and output-error recovery, preserving playback intent and
  position where seeking is supported.
- Embedded Windows icons, file logging and single-instance protection.

### Performance and reliability

- Event-driven playback with bounded PCM buffering and no allocation, decoding
  or file I/O in the audio sample callback.
- Optimized EQ processing with bit-for-bit sample/history regression coverage.
  Matched measurements showed roughly 50–53% lower player CPU across the tested
  EQ-on workloads on one computer with two output devices. See
  [performance measurements](PCM_PERFORMANCE.md) for methods and limits.
- Command and load-generation handling that keeps stale decoder events from
  overriding newer playback requests.
- Atomic settings persistence and shutdown cleanup for owned streaming and
  playlist processes.
- Large-playlist metadata handling with preserved entry positions and a bounded
  310-second fetch deadline.

### Build and license

- Rust 1.99.0 minimum, with Windows CI checking formatting, Clippy, offline tests,
  release compilation and executable resources.
- MIT license for the project's original code; dependencies retain their own
  licenses.

### Requirements and limits

- Windows 10 or 11. Network playback requires `yt-dlp` and its runtime dependencies;
  see the [README](README.md) for setup and supported formats.
- Linux/Wayland, Opus, ALAC and full HE-AAC reconstruction remain deferred.
