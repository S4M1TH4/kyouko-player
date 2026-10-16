# Local CPAL modifications

This directory contains a modified CPAL 0.18.2, originally from RustAudio/cpal
revision `e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7`. It remains under Apache-2.0;
the project's root MIT license does not replace this license.

Changes include WASAPI output-worker startup observation, Windows/Linux-only
platform and feature declarations, removal of unsupported-platform helpers,
and replacement of the upstream README with local scope/build instructions.
Files changed from the published crate carry a prominent modification notice:

- `Cargo.toml`
- `README.md`
- `src/error.rs`
- `src/host/latch.rs`
- `src/host/mod.rs`
- `src/host/wasapi/mod.rs`
- `src/host/wasapi/stream.rs`
- `src/lib.rs`
- `src/platform/mod.rs`
- `src/sample_format.rs`
- `src/traits.rs`

See OUTPUT_STARTUP_PATCH.md, startup-observation.patch and PLATFORM_SCOPE.md for
implementation details and earlier change provenance. Those documents and this
notice must accompany a redistributed copy of this modified source directory.
The upstream crate has no separate NOTICE file; its LICENSE is retained intact.
