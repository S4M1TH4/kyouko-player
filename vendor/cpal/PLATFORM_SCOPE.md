# Retained platform scope — 2026-10-15

This source subset retains Windows WASAPI and Linux ALSA, PipeWire and PulseAudio,
plus their shared interfaces, timestamp/sample conversion, error dispatch,
COM (Windows), equilibrium fill and callback-latch helpers. The retained backend
implementations are byte-identical to the pre-pruning tree; the existing WASAPI
startup instrumentation is preserved.

Linux audio is independent of Wayland/X11. The future player presentation layer
will target Wayland; this prune does not add a Linux UI or change audio routing.
ALSA remains CPAL's default Linux backend. Enabling `pipewire` prefers PipeWire
when available; enabling `pulseaudio` retains upstream PulseAudio selection and
ALSA fallback. Existing realtime/DBus promotion features remain available.

Removed: Apple/iOS, Android, browser/WebAssembly and null-platform backends;
unused ASIO, JACK and custom-host features; examples and example-only dev
dependencies; upstream CI, container/cross configuration, cache metadata and
maintenance documents. The ASIO-only build script was removed and package
`build = false` explicitly disables it. Cargo manifests and platform dispatch
no longer advertise those targets/features or reference removed files.

Retained provenance: `LICENSE`, `.cargo_vcs_info.json`,
`OUTPUT_STARTUP_PATCH.md`, and `startup-observation.patch`. The latter is the
original startup-only diff, not a complete diff of this later platform prune;
the repository Git diff records pruning separately.

No installed Cargo-registry source was edited. Windows behavior and enabled
features are unchanged. Linux backend compilation/runtime remains to be
verified on Linux with the corresponding native development libraries; this
Windows environment has no installed WSL distribution/toolchain.

Windows verification passed: `cargo check --locked`, Clippy for all targets with
warnings denied, 188 application tests (nine opt-in tests ignored), release
build, the promotion-disabled check, and all 15 CPAL library unit tests. Retained
backend files and license/original provenance were also checked against the
pre-pruning Git revision; manifest target/feature/file references were validated.
