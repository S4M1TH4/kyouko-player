# Local WASAPI startup observation

Upstream: CPAL 0.18.2, crate source revision
`e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7`.
The repository already contained this source copy. Cargo now selects it through
`[patch.crates-io]`; the installed Cargo registry is never edited.

The output worker executes CPAL's existing realtime policy before its callback
latch: `audio_thread_priority` 0.35.1 calls
`AvSetMmThreadCharacteristicsW(L"Audio")`; on failure CPAL falls back to
`SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL)`. No process priority changes.
Input-stream promotion keeps upstream behavior.

The extension distinguishes disabled, directly observed MMCSS registration,
fallback (including the MMCSS error), and total failure (both errors). It records
native thread ID and `GetThreadPriority`, retaining unknown priority as `None`.
A bounded one-result startup channel delivers this result to the constructor;
`Stream::output_worker_startup()` exposes it to the owner before playback.
Missing publication is `None`, never inferred success. Priority failure is
reported by the player owner and leaves playback available at the inherited
priority. The original fallback policy is unchanged. The upstream code discards
the returned realtime handle; registration remains associated with the worker
and retirement joins/terminates that thread, as before.

`GetThreadPriority` is Windows' relative-priority API, not a continuously sampled
kernel scheduling priority. The trace's CSwitch priorities supply the latter
where captured. Windows can adjust scheduling dynamically and supplies no hard
real-time guarantee. There is no callback-side initialization, formatting,
logging, allocation, blocking startup publication, or additional worker.

Modified upstream files: `src/host/wasapi/stream.rs`,
`src/host/wasapi/mod.rs`, `src/platform/mod.rs`, `src/host/mod.rs`.
The last file only suppresses an upstream Windows-only unused helper warning
that becomes visible for a local path dependency. `startup-observation.patch`
records the changes against the unmodified pinned registry source for review.
