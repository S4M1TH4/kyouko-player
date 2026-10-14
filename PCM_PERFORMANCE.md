# Output worker priority evaluation — 2026-10-14

CPAL realtime promotion is enabled by default. These samples demonstrate more
regular output-worker servicing. One traced healthy pair also recorded **nine
attributable render-starvation notifications disabled versus zero promoted**.
This supports improved output behavior in that pair; it does not establish
universally smoother audible playback or hard real-time operation. **General
CPU savings were not demonstrated**: healthy CPU ranges overlap. CPU was lower
in all three promoted replacement observation runs, a narrower observation
that excludes the replacement operation itself. PCM sample memory is unchanged.

## Implementation and startup verification

The pinned CPAL 0.18.2 WASAPI backend already has a maintained `realtime` feature.
`audio_thread_priority` 0.35.1 dynamically loads avrt.dll and registers MMCSS's
`Audio` task through `AvSetMmThreadCharacteristicsW`. On failure CPAL tries
`SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL)`. It does not additionally
call `AvSetMmThreadPriority`. The upstream public API hides which path succeeded.

The repository source patch in `vendor/cpal` exposes that direct result, native
TID and `GetThreadPriority` snapshot. Each output worker initializes before its
callback latch and sends one result through a bounded startup channel; the
stream constructor receives it outside callbacks. The owner reports requested
mechanism, observed outcome and priority once per current epoch. Missing results
are explicitly unverified, fallback includes its MMCSS error, and total failure
includes both errors. Promotion denial is reported without destroying available
output. Epoch and error/advisory bits share a single CAS, fencing retirement
races; startup publication occurs only on the stream owner.

Every measured promoted worker (15 low-overhead + 5 traced) directly reported
MMCSS success and `GetThreadPriority=13`. Disabled workers reported disabled
promotion and priority 0. These are actual API return values, not an inference
from absence of errors. ETW additionally verified kernel switch-in priority
**21 promoted**, versus **8/9 disabled** (ordinary Windows dynamic boosts).
`GetThreadPriority` is a startup relative-priority snapshot; the scheduler can
change priority later. See Microsoft's [GetThreadPriority documentation](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getthreadpriority).
All measured processes reported `NORMAL_PRIORITY_CLASS=32`; playback and builds
were unelevated. No process/global priority, affinity, power plan, driver,
registry, overlay, hotkey, playlist or format changes were made.

See [vendor patch notes](vendor/cpal/OUTPUT_STARTUP_PATCH.md) and
[startup diff](vendor/cpal/startup-observation.patch). Cargo selects the pinned
source copy through `[patch.crates-io]`; installed registry source was not edited.

## Source, device and procedure

The fresh saved selection was
`10. nachi — Against, Perfect Cherry Blossom..ogg` under `坂上なち`.
`last_folder` was empty. Volume was 100%, EQ enabled with gains
`1,3,4,2,3,3,-2,-3,-3,-5` dB, and repeat disabled. The previous handoff report
used another track/settings and is not a valid priority baseline.

All comparisons used the same concrete default render endpoint
`{0.0.0.00000000}.{c78a8174-0922-4446-8d6c-713f6dea26ec}`, native 48 kHz stereo
F32, WASAPI shared mode, and CPAL `BufferSize::Default`. The diagnostic asserts
identical endpoint/rate/layout/format after every replacement and rejects an
unplanned replacement during observation. Traces report a 10 ms device period
and 1056-frame native WASAPI buffer; neither was overridden.

The release test harness uses the production broker restoration, decoder,
DSP/resampler, PCM pool, output construction and recovery. It reads `state.cfg`
without installing a saver or opening the overlay. Each process begins from the
saved source/settings. No mouse/keyboard input or manual playback was used.

The initial promotion-disabled release baseline was captured **before enabling
promotion**. Then three healthy pairs and three replacement pairs alternated
**disabled, enabled, disabled, enabled, disabled, enabled** per scenario. All
builds finished before observations. Healthy runs warm for two seconds and
observe twenty seconds. Replacement runs observe four workers for five seconds
each, with two seconds of warm-up per worker and three real stream replacements.
Recovery retains position and the one decoder thread. Per-run replacement CPU
is weighted by the four observation durations; interval distributions merge
segments without constructing intervals across worker boundaries.

Process kernel + user CPU comes from `GetProcessTimes`; **100% means one fully
occupied logical core**, not whole-machine Task Manager usage. The measurement
includes decoder, WASAPI worker and diagnostic broker/test harness. It excludes
normal overlay/save/log workers, initialization, warm-up, replacement/recovery
work, later analysis and CSV export. Observation wall time was about 20 seconds
per run (small event-wait deadline overshoot is retained in raw logs).

## Corrected diagnostic boundaries

Only raw monotonic `Instant` entry/end times, absolute QPC ticks, CPAL callback
and predicted-playback timestamps, and frame counts enter a preallocated
8192-slot array inside the callback. There is no callback allocation, sorting,
formatting, file I/O or logging. Overflow is counted; no records were dropped in
these runs. Records are read only after all stream writers have been destroyed
and joined. Analysis uses nearest-rank quantiles. Device-clock regressions are
reported rather than silently converted to zero intervals; none were observed.

- **Actual entry intervals** use consecutive monotonic application entries.
- **CPAL/device-clock intervals** use CPAL's `timestamp.callback`: WASAPI's
  `IAudioClock::GetPosition` associated QPC timestamp, not the application's entry.
- **Application callback wall duration** includes PCM copy/conversion, bounded
  claims/recycle/drain, useful decoder notification and timing capture. It can
  include preemption. It excludes final record publication and CPAL's surrounding
  waits/commands, padding queries, GetBuffer, equilibrium fill, clock calls,
  typed-data/monotonic-playback wrappers, integer-backend adjustment and
  ReleaseBuffer. It is not exclusive on-CPU instruction time or the entire
  WASAPI service duration.
- CPAL's predicted playback timestamp includes its buffered-position/latency
  estimate and is clamped nondecreasing by CPAL. It is exported for inspection;
  it is not a physical measurement of when sound was heard.
- PCM underruns count missing PCM after priming and before EOF. They do not count
  engine/device starvation when the output worker cannot run. Long entry gaps do
  not prove audible glitches, and zero PCM underruns do not prove glitch-free
  device output. The previous report's timestamp gaps were device-clock gaps;
  they cannot be relabeled as actual entries or output-glitch counts.

## Fresh baseline and low-overhead results

The initial disabled baseline observed 20.006 s: CPU **3.2803% one core**, PCM
sample storage **622,592 bytes**, 19 blocks, zero PCM underrun frames, 1,984
records. Callback wall-duration p50/p95/p99/max was
**1.600 / 14.800 / 24.500 / 97.900 µs**; actual entry intervals were
**10.026 / 10.641 / 15.357 / 51.064 ms**; CPAL/device-clock intervals were
**10.027 / 10.641 / 15.357 / 51.062 ms**. Worker TID was 3144,
`GetThreadPriority=0`, promotion disabled. This baseline is reported separately
from the later paired runs rather than selecting it as a favorable comparison.

All distribution cells below are **p50 / p95 / p99 / maximum**.
All twelve paired runs had **zero PCM underrun frames**, **zero record overflow**,
**622,592 sample bytes** and **19 sample blocks per current pool**.

### Healthy playback — three alternating pairs

| Configuration/run | CPU (% one core) | Records | Callback wall duration µs | Actual entry interval ms | CPAL/device-clock interval ms |
| --- | ---: | ---: | --- | --- | --- |
| disabled-1 | 1.9520 | 1984 | 1.700 / 17.100 / 25.300 / 58.900 | 10.026 / 10.792 / 13.643 / 59.836 | 10.026 / 10.790 / 13.645 / 59.837 |
| enabled-1 | 1.8741 | 2001 | 1.200 / 11.700 / 20.800 / 32.300 | 10.030 / 10.378 / 10.904 / 11.468 | 10.030 / 10.376 / 10.905 / 11.467 |
| disabled-2 | 1.2490 | 1972 | 1.400 / 15.200 / 22.600 / 1278.500 | 10.027 / 10.823 / 17.289 / 71.109 | 10.027 / 10.823 / 17.288 / 71.111 |
| enabled-2 | 1.9518 | 2001 | 1.600 / 14.600 / 22.200 / 108.300 | 10.028 / 10.462 / 11.054 / 11.575 | 10.028 / 10.462 / 11.049 / 11.574 |
| disabled-3 | 2.3432 | 1963 | 1.700 / 16.200 / 28.800 / 2047.800 | 10.021 / 10.976 / 21.396 / 65.870 | 10.021 / 10.971 / 21.396 / 65.869 |
| enabled-3 | 2.0297 | 2001 | 1.600 / 15.800 / 22.600 / 73.600 | 10.026 / 10.470 / 11.049 / 11.924 | 10.027 / 10.467 / 11.047 / 11.922 |

Healthy CPU ranges were 1.2490–2.3432% disabled and 1.8741–2.0297% promoted;
median CPU was about 1.9520% and 1.9518%, respectively. The ranges overlap;
there is no demonstrated healthy-playback CPU saving. Entry p99 and maxima
improved in each pair, while application-duration outliers varied significantly.

### Repeated output replacement — three alternating pairs

| Configuration/run | CPU (% one core) | Records | Callback wall duration µs | Actual entry interval ms | CPAL/device-clock interval ms |
| --- | ---: | ---: | --- | --- | --- |
| disabled-1 | 2.2604 | 1993 | 1.600 / 15.700 / 27.900 / 98.700 | 10.025 / 10.754 / 13.545 / 57.822 | 10.026 / 10.739 / 13.528 / 57.822 |
| enabled-1 | 1.7158 | 2003 | 1.600 / 14.900 / 23.400 / 105.600 | 10.028 / 10.420 / 10.959 / 11.685 | 10.028 / 10.420 / 10.956 / 11.681 |
| disabled-2 | 2.3396 | 1990 | 1.500 / 15.200 / 23.500 / 129.000 | 10.018 / 10.869 / 14.294 / 51.522 | 10.020 / 10.868 / 14.294 / 51.523 |
| enabled-2 | 1.7949 | 2001 | 1.700 / 17.000 / 23.900 / 34.800 | 10.024 / 10.616 / 11.138 / 11.535 | 10.024 / 10.616 / 11.138 / 11.534 |
| disabled-3 | 2.1049 | 1994 | 1.600 / 17.200 / 24.200 / 770.200 | 10.018 / 10.872 / 14.968 / 40.589 | 10.018 / 10.873 / 14.967 / 40.591 |
| enabled-3 | 1.6389 | 2002 | 1.600 / 14.500 / 23.600 / 80.900 | 10.024 / 10.442 / 11.019 / 11.556 | 10.025 / 10.443 / 11.018 / 11.553 |

Replacement-window CPU ranged 2.1049–2.3396% disabled versus 1.6389–1.7949%
promoted. This is consistently lower in these samples, but the windows measure
steady playback between replacements, not the cost of tearing down/rebuilding.
All nine recoveries per configuration preserved position and decoder identity.
Time to restored Playing ranged 75.906–124.700 ms disabled and
79.520–103.889 ms promoted; the subsequent two-second warm-up is separate.

PCM storage remains **608 KiB**: nineteen 4096 × 2 × 4-byte allocations.
Eighteen ready slots cover 1.536 seconds, preserving the 1.5-second capacity
policy, with one existing extra production/render block. Priming remains
500 ms; compressed RAM stream cache remains capped at 60 MiB. This metric
excludes queue bookkeeping, decoder/CPAL/OS storage, transient replacement
ownership and diagnostic records; it is not process RSS or a RAM-saving claim.

## Elevated traces and attribution

Only one-shot tracing helpers ran as administrator through
`Start-Process powershell.exe -Verb RunAs -WindowStyle Hidden`. Builds, tests,
playback, the trace controller and offline analysis remained unelevated. The
helper interface was inspected before use. It checked WPR status and active ETW
sessions before starting, refused other WPR recordings and existing output
paths, used the unique instance `kyoukoPriority20261014` with an ownership
marker, and stopped only that instance to explicit normal-readable paths.
No other recording was cancelled/overwritten. The temporary helper was removed
following final verification; no service, scheduled task, background monitor,
polling loop or persistent privilege/configuration change was installed.

WPR **Audio.Verbose.File** captured audio-glitch tracing plus CpuConfig,
ProcessThread, CSwitch, ReadyThread, ThreadPriority, DPC, Interrupt and WDF
DPC/interrupt data, with CSwitch/ReadyThread stacks. A supplemental profile
explicitly enabled **Microsoft-Windows-Audio**
`AE4BD3BE-F36F-45B6-8D21-BDD6FB832853` and **Microsoft-Windows-MMCSS**
`36008301-E154-466C-ACEC-5F4CBD6B4694`, level 5, all keywords. Elevated `logman
query ... -ets` verified active provider masks, including Audio/GlitchDetection
`0x0400000000000000`. Actual initialize/streaming/audio-engine events were decoded.
All four saved traces reported **zero lost events**. Analysis used Microsoft's
TraceEvent 3.2.8 library at normal privilege, retaining decoded exports and raw
QPC correlation. Trace diagnostics are separate from the CPU/timing tables.

The first supplemental attempt used brace-wrapped GUID names that produced an
empty event collector: elevated WPR status/stop returned **0xc5580612**,
"An Event session cannot be started without any providers." That failure occurred
through the elevated helper, not an ordinary-shell privilege denial. Its owned
instance was inspected and cancelled/cleaned; it supplied no comparison playback
data. Registered provider names fixed coverage. One subsequent start was refused
by the session-inventory guard between recordings; inspection confirmed no
remaining recording before the replacement traces proceeded. Neither failure
was substituted with callback-gap counts. Early direct GUI-test invocations
also produced no captured data; final comparison/traced runners used explicit
process waiting, redirected stdout/stderr and verified zero exit codes.

Counts below cover only the raw-record observation windows (~20 s each),
excluding warm-up/replacement/stop. Ready delays are worker runnable-to-running
intervals measured by ReadyThread/CSwitch, including preemption resumes.

| Trace | PID / output TIDs | Kernel switch-in priorities | Ready delay ms p50/p95/p99/max | Attributable render-starvation notifications | Other cross-process glitch notifications |
| --- | --- | --- | --- | ---: | ---: |
| healthy-disabled | 16416 / 3492 | 9, 8 | 0.0323 / 0.4143 / 4.6724 / 41.0322 | 9 | 0 |
| healthy-enabled | 18696 / 10844 | 21 | 0.0355 / 0.0836 / 0.1096 / 0.9285 | 0 | 0 |
| replacement-disabled | 18964 / 19240;19128;17500;18784 | 9, 8 | 0.0389 / 0.1855 / 0.8479 / 17.4218 | 0 | 0 |
| replacement-enabled | 10000 / 16800;12092;8744;18096 | 21 | 0.0351 / 0.0819 / 0.0925 / 0.3494 | 0 | 0 |

The disabled healthy worker's nine `Microsoft.Windows.Audio.CrossProcess/Glitch`
notifications were `SERVER_INPUT_STARVATION`, `numGlitches=1`. Their ContextId
`406d8157-2db4-42d7-9d71-92c2c8a9700e` exactly matches PID 16416's
`AudioClientInitialize.AudioClientGuidId`, stream 108, the same output endpoint;
service `StreamCreate` and `StreamVolumeChange` independently link the render
stream GUID and process. They were emitted by audiodg (PID 13416), not by the
player, but this context chain makes them attributable to its render stream.
The matching nine manifest event-33 rows are duplicate notifications of those
incidents; they are **not added again**. `FinishedGlitching` summaries are also
not counted as additional incidents. These are engine starvation notifications,
not nine independently verified audible pops or nine recording-wide glitches.
No unrelated cross-process glitch notifications occurred inside these windows.
Other system audio/driver events remain system-wide unless explicitly attributed.

All nine attributable notifications fall inside five delayed application-entry
intervals. The largest interval was **50.8952 ms**: the worker spent
**41.0322 ms runnable but unscheduled** and **9.7500 ms waiting**; the preceding
application callback took 1.1 µs. Other corresponding intervals had
27.9222–33.5714 ms of runnable delay. This directly points to CPU scheduling
latency as the dominant cause of those sampled failures, rather than decoding
starvation or merely delayed delivery of an audio wake. Promoted healthy-worker
ready delay max was 0.9285 ms; promoted replacement max was 0.3494 ms.

The disabled replacement trace had a 27.7665 ms entry interval with 17.5864 ms
total runnable delay and 9.9017 ms waiting, yet **no attributable render-starvation
notification**. This is concrete evidence that a long callback interval cannot
be equated with an output glitch. All traced windows had zero PCM underruns,
including the healthy disabled run that logged actual render starvation.

WASAPI return-event spans were also inspected: buffer-return to clock-return
maxima were 1.4988 ms disabled healthy, 0.1234 ms promoted healthy,
0.2456 ms disabled replacement and 0.1977 ms promoted replacement.
Clock-return to ReleaseBuffer-return maxima were 0.7647, 0.1716, 0.6552 and
0.7265 ms, respectively. These spans are not direct API-entry/exit measurements
and include intervening work/preemption. They do not show a tens-of-milliseconds
backend stall accounting for the large gaps. Waiting alone cannot identify
which device/driver/audio-event layer delayed a wake; that remains a limit.

ISR/DPC coverage was present throughout. Observation-window maximum DPC duration
ranged 1.1965–1.6085 ms and ISR duration 0.3156–0.3821 ms across the four traces.
The longest decoded DPC routine addresses mapped to `Wdf01000.sys`; this is a
framework wrapper, not an identified underlying hardware driver. These events
are system-wide and do not establish driver causation for the 41 ms runnable
delay. No driver, power-plan or scheduling-affinity changes were made.

## Verification and remaining limits

All requested gates passed with no warnings:

```text
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked                 # 188 passed, 9 opt-in tests ignored
cargo build --locked --release
```

Focused in-memory tests cover startup result publication once per epoch,
retired startup/errors/advisories, missing observations, fallback/total failure
without output teardown, independent monotonic vs device-clock calculations,
clock regression, nearest-rank/empty distributions and fixed-record overflow.
Existing allocation-free callback, PCM/wake, decoder/cache/downloader,
position-restoration and cleanup regressions also passed. Hardware fallback and
total priority failure were not forced; their result handling is synthetic.

Both configurations also passed the existing four-fault WASAPI recovery test:
actual stream retirement/reopening, retired PCM pool freed, position restored,
unchanged decoder identity and healthy zero-underrun playback. The offline
partial-download recovery tests retain the original compressed buffer/downloader
with repeat on/off and verify Stop/Quit cleanup. Local hardware diagnostics do
not themselves measure network download starvation.

These are three pairs per low-overhead scenario and one traced pair per scenario,
on one shared Windows device with changing background activity, not a controlled
statistical or listening study. Tracing adds substantial overhead and its CPU
results are not mixed with low-overhead CPU. Startup/API and switch-in priority
observations do not continuously prove every scheduling instant. Provider coverage
and zero lost records do not guarantee every audible driver/device failure emits
an event; no external physical waveform/listening measurement was performed.
A zero trace count means no captured attributable notification in that window.
Callback duration excludes the surrounding WASAPI work described above, and
process/sample-memory scopes are deliberately narrower than full-app CPU/RSS.

**Demonstrated here:** more regular callback delivery, directly observed MMCSS
promotion on all measured replacement workers, and fewer attributable render
starvation notifications in the healthy traced pair. **Not demonstrated:** a
general audible-smoothness guarantee, general CPU savings, PCM RAM savings or
hard real-time scheduling. Changes remain uncommitted; no push or packaging.

Raw run logs/CSV, `comparison.json`, ETLs, provider inventories, TraceEvent exports,
`trace-correlation.json` and analysis scripts are retained locally under
`target/priority-eval/` (ignored build/diagnostic artifacts). Source/settings,
worker identities, distribution variation and counter scopes are independently
reviewable there. `state.cfg` was read, not rewritten.
