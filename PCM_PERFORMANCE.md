# PCM handoff measurements — 2026-10-14

The handoff now uses bounded atomic PCM queues and an atomic EOF mailbox.
Decoder capacity/command/drain waits use `atomic-wait`, without periodic waits,
spinning or another permanent thread. This removes Crossbeam's callback waiter
registry, but the measurements below do **not** demonstrate lower CPU use,
RAM savings or consistently smoother playback.

## Method

Two baseline runs and two final runs used the same saved local source:
`04. Masayoshi Minoshima — 遠野幻想物語.ogg` from the folder saved in `state.cfg`.
Volume was 100%; saved EQ was enabled with gains
`1,3,4,2,3,0,-2,-3,-3,-5` dB. Each run used the production decoder, DSP,
resampler, PCM pool and actual WASAPI stream, at native 48 kHz stereo with
`BufferSize::Default`. Buffer capacity and priming were unchanged.

The release test harness reads configuration without saving it or opening the
overlay. It allows two seconds for startup, then observes 20 seconds. Process
kernel + user CPU comes from `GetProcessTimes`, divided by observation wall
time: **100% means one fully occupied logical core**, not the whole-machine
percentage shown by Task Manager. This includes the decoder/WASAPI/test process;
normal application UI, save and log workers are outside this harness.

Callbacks capture only raw start/end `Instant`s, CPAL callback timestamps and
frame counts in a fixed, preallocated record array. Records are read, sorted,
analyzed and printed after stream destruction joins the callback worker. Timing
instrumentation is test-only and adds small measurement overhead. CPU accounting,
printing, allocation and timing analysis occur outside the callback.

Run the same diagnostic with:

```text
cargo test --locked --release saved_track_release_pcm_performance -- --ignored --nocapture --test-threads=1
```

The baseline retained Crossbeam PCM/recycle/drain channels; only this diagnostic
was added before measurement. Final runs use the completed atomic handoff.
These are observations on a shared Windows machine, with two samples per version,
not a controlled statistical test or a subjective listening assessment.

## Results

| Metric | Baseline 1 | Baseline 2 | Final 1 | Final 2 |
| --- | ---: | ---: | ---: | ---: |
| Observation (seconds) | 20.000 | 20.000 | 20.000 | 20.000 |
| Process CPU (% of one core) | 1.2500 | 1.2500 | 2.6562 | 1.7187 |
| PCM sample allocation (bytes) | 622,592 | 622,592 | 622,592 | 622,592 |
| Preallocated sample blocks | 19 | 19 | 19 | 19 |
| Decoder underrun frames | 0 | 0 | 0 | 0 |
| Callback records | 1,986 | 1,983 | 1,993 | 1,994 |
| Callback duration p50 (µs) | 1.200 | 1.000 | 1.300 | 1.200 |
| Callback duration p99 (µs) | 26.700 | 24.100 | 26.100 | 20.100 |
| Callback duration maximum (µs) | 76.500 | 78.300 | 68.600 | 40.200 |
| Callback timestamp interval p99 (ms) | 11.937 | 11.290 | 11.378 | 11.505 |
| Callback timestamp interval maximum (ms) | 49.804 | 77.220 | 32.065 | 36.946 |
| Gaps > twice previous callback's frame duration | 7 | 5 | 4 | 5 |

PCM memory here is the actual sample storage: nineteen allocations of
4096 × 2 × 4 bytes, totaling **608 KiB**. Eighteen ready slots cover 1.536 seconds;
the nineteenth block is the existing extra rendering/production block. Priming
remains 500 ms. The metric excludes queue/spare bookkeeping, decoder working
memory, CPAL/OS storage and test-only timing records; it is not process RSS.
No PCM capacity reduction or overall RAM saving is claimed.

The underrun counter measures missing PCM after priming and before EOF; silence
while priming and after drain is intentional. Callback intervals independently
show delayed output servicing. Both versions had zero decoder starvation while
still showing long callback gaps. A larger PCM queue or different queue library
cannot make a delayed WASAPI callback run earlier.

Final CPU was higher in these runs. Maximum callback durations and gaps were
lower in these small samples, but gaps persist and p99 intervals are comparable.
This does **not** demonstrate consistently smoother playback. Windows scheduling
and address-based wake primitives offer no hard real-time guarantee.

## Automated coverage

Offline tests cover ring capacity/FIFO/wraparound (including integer rollover),
competing claims and exactly-once destruction, interrupted publication, full
buffer command/disconnection/device-loss interruption, paused stale reclamation,
seek/load fencing, short EOF tails, empty silence, and zero sample-callback heap
allocations/deallocations through conversion, recycling, drain and disconnection.
Notification tests enumerate all publication/registration/check/wait orderings,
exercise real waits and verify coalescing/no OS wake while running. Existing
production-loop recovery tests replace rate/layout repeatedly and retain the
same partial RAM download with repeat on/off, then check Stop/Quit cleanup.

All required gates passed without warnings:

```text
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked                 # 184 passed, 9 opt-in/fixture tests ignored
cargo build --locked --release
```

The opt-in release WASAPI recovery diagnostic also passed four actual stream
replacements using the saved selection. Each restored position in about 0.01 s,
released the retired pool and retained the decoder thread. Its healthy playback
intervals had zero decoder underrun frames. The offline partial-download recovery
regression verifies original compressed-buffer/downloader retention; local-file
hardware recovery does not itself exercise network caching.

The workspace's existing `ARCHITECTURE.md` is ignored by `.gitignore`; its local
ownership and wake-up documentation was updated as requested. No commit, push,
UI-input simulation, manual testing request or release packaging is involved.
