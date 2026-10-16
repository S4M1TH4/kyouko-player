> Modified for kyouko-player: WASAPI startup observation and/or the
> Windows/Linux platform subset. See MODIFICATIONS.md in the CPAL root.

# CPAL 0.18.2 — local Windows/Linux subset

This is the pinned CPAL source used by kyouko-player through Cargo's
`[patch.crates-io]` override, not a separate Git repository.

Retained audio backends:
- Windows: WASAPI.
- Linux: ALSA (default), optional PipeWire and PulseAudio.

These Linux backends are independent of the display server and are suitable for
the planned Wayland player. CPAL contains no Wayland or X11 presentation layer;
retaining audio backends does not implement the application's future Linux UI.

Retained features: `pipewire`, `pulseaudio`, `realtime`, `realtime-dbus`.
The root application currently enables only `realtime` through
`output-realtime`. Native Linux development libraries will be required when
building the Linux backends. No Linux backend is enabled for the Windows build.

The Windows startup observation extension is documented in
[OUTPUT_STARTUP_PATCH.md](OUTPUT_STARTUP_PATCH.md), with its original diff in
[startup-observation.patch](startup-observation.patch).
[PLATFORM_SCOPE.md](PLATFORM_SCOPE.md) records subsequent source pruning.
The original upstream revision is also retained in `.cargo_vcs_info.json`.

Upstream: <https://github.com/RustAudio/cpal>, version 0.18.2.
License: Apache-2.0; see [LICENSE](LICENSE).
