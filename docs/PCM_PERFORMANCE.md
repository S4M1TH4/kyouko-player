> Temporary assessment tools, raw artifacts and measured binaries were removed
> after completion. The results and methods below remain a historical record.
> Reproduction requires recreating the tools, fixtures and matched builds.

# Diagnostic memory-retention controls — 2026-10-16

The preceding 31-minute BT run showed private-commit first/last-window means of
3.46 → 3.67 MiB. Its diagnostic controller retained every full observation as a
heap-allocated JSON value until exit. This follow-up tests that measurement
effect before attributing the drift to ordinary playback.

## Matched paused controls

Three adjacent matched pairs, **six fresh processes**, alternate retained/streamed
order. Both modes use the **same release binary**, identical isolated state and
480-second low-level 440 Hz FLAC (48 kHz stereo), saved EQ gains/volume, EQ enabled
and repeat off. Each process stays Paused, warms for eight seconds and takes
60 observations separated by the controller's existing two-second settle.
Observation duration is zero: these are memory sampling points, **not CPU windows**.
No CPU saving or idle-CPU claim is derived from them. The predeclared comparison
is last-ten minus first-ten mean private commit, then retained minus streamed
growth within each pair. Three pairs support a descriptive local comparison;
no population confidence interval or universal leak claim is made.

Native output is unchanged BT SPEAKER at 48 kHz/stereo/F32, endpoint
`{0.0.0.00000000}.{16409415-e7e2-4e08-9290-b04bc91d5f2e}`.
Every paused sample kept the same generation, primed PCM watermark and rendered
frame counter, with zero underruns/recorder overflow and the same 608 KiB PCM pool.
All processes used normal process priority and verified MMCSS output priority 13.
Builds, tests and fixture generation finished before observation runs began.

| Pair | Storage mode | Private commit first → last means (MiB) | Growth (KiB) | Working-set growth (KiB) |
|---|---|---:|---:|---:|
| 1 | retained | 3.585 → 4.061 | +487.6 | +538.4 |
| 1 | streamed | 3.596 → 3.500 | -98.4 | -91.6 |
| 2 | retained | 3.625 → 4.125 | +512.4 | +566.4 |
| 2 | streamed | 3.584 → 3.492 | -94.4 | -32.0 |
| 3 | retained | 3.727 → 4.210 | +494.8 | +550.4 |
| 3 | streamed | 3.580 → 3.484 | -98.0 | -36.8 |

Mean private-commit growth: retained
**+498.3 KiB**, streamed
**-96.9 KiB**.
Paired retained-minus-streamed growth contrasts were
+586.0, +606.8, +592.8 KiB;
their mean was **+595.2 KiB**.

All three paired private-commit contrasts are positive. With playback paused
and no PCM delivered, retaining diagnostic observations produced more memory
growth than streaming them from the same binary under the same inspection plan.
This implicates the controller's retained JSON values in the measured drift.
It does **not** establish that every byte of the earlier 31-minute playback drift
had this cause, or exclude slower production, decoder, GDI, driver or device-related
growth. The new mode removes this specific accumulation of observation rows;
startup/command rows and the finite input plan still remain in memory.

Sampling is accelerated relative to the earlier one-observation-per-minute run.
Row count, elapsed time, allocator behavior and Windows residency all matter;
these results must not be extrapolated into an hourly leak rate. GetProcessMemoryInfo
private commit describes process allocation commitment; working set describes
resident pages. Neither is an exact sum of JSON object payload bytes.

## Streamed playback validation

One fresh process played continuously across **six 60-second observations**, with
the existing two-second settles between them and eight seconds of initial warm-up.
All six rows were flushed to JSONL, while the main result retained startup/command
rows and the streamed path/count. Endpoint/generation/pool stayed fixed, PCM
delivery was approximately 1× and PCM underruns/recorder overflow were zero.
No source repeat or output replacement was observed. This checks the export
protocol under real playback; it is not a multi-hour endurance test.

Private commit at the six observation ends ranged
**3.410–3.566 MiB**,
with first-to-last change **+28.0 KiB**.
Working set ranged
**15.004–15.238 MiB**,
with first-to-last change **+220.0 KiB**.
These remain instrumented-process boundary measurements, not a proof of flat
production memory or glitch-free audible output.

## Diagnostic change and verification

`KYOUKO_APP_OBSERVATIONS` optionally names a new JSONL destination. A bounded
buffered writer on the finite controller serializes and flushes each observation
after its measured boundary, then releases that row. No callback/decoder I/O,
production polling or normal playback-policy change is introduced. Without the
variable, observations remain in the result JSON as before. Creation uses
create-new semantics; a failure requests production Quit and reports a diagnostic
error. Unit coverage checks independent JSON lines, embedded newlines/non-ASCII
titles and write/flush errors. An actual existing-file launch confirmed graceful
failure without overwriting evidence.

Required checks passed: normal check/clippy/test/release build (191 tests),
diagnostic clippy/test/release build (196 tests), ten environment-dependent tests
ignored in each configuration. Measurement runs used the instrumented release
build; the ordinary release build was verified separately. Source/binary
hashes, the finite design, schedule, source recipes, plans, logs, raw JSON/JSONL,
isolated state and offline analysis were collected for the study.

Real state, current saved Ogg selection and source music were preserved. Generated
fixture and task-owned isolated state/processes were cleaned up after verification.
Comparable measurements on other machines, multi-hour playback, physical
audible-output latency/device glitches and compositor/input timing remain unverified. A longer
memory study should use streamed observations to avoid this retention confound.


---

# Longer EQ-bypass CPU comparison — 2026-10-16

This study follows the earlier BT SPEAKER bypass-control increase of +0.182
percentage points of one logical core. It reuses the **exact original two release
binaries**, rather than comparing one old binary against the newer callback-timing
diagnostic build. Baseline/candidate hashes were checked for every launch against
the original binary manifest. The EQ is disabled in both variants, so the
changed EQ loop is bypassed. The source of any whole-binary CPU difference cannot
be inferred merely from the source location of that loop.

## Predeclared procedure

Eight adjacent matched pairs, **16 fresh processes**, each with ten seconds of
warm-up and a **120-second CPU window**. Build order alternates AB/BA by pair.
The earlier windows lasted twenty seconds. Longer windows reduce the percentage
effect of coarse process-time accounting; they do not eliminate background or
CPU-frequency variation. Every run uses the actual visible overlay, UI pump,
decoder, output, logger and state writer. Callback timestamps and decoder-stage
profiling are disabled, and the finite controller sleeps throughout each window.
No compilation, fixture generation, tests or statistical analysis overlapped
these observations. Native output is frozen to **BT SPEAKER, 48 kHz stereo F32**,
endpoint `{0.0.0.00000000}.{16409415-e7e2-4e08-9290-b04bc91d5f2e}`.

The source is a generated low-level 440 Hz, PCM16/48 kHz stereo WAV. Its recipe
matches the preceding bypass fixture except duration is extended from 80 to 180
seconds, so each process can finish its warm-up/window before EOF without loops.
Both variants use the identical file bytes and isolated initial-state bytes.
Volume 100% and saved band gains are frozen; only isolated state disables EQ.
Repeat is off. The user's current saved track is a different local Ogg selection;
that selection and all real state were preserved, rather than restored to an
older assessment's saved MP3. No generated fixtures enter the user's library.

CPU uses GetProcessTimes kernel+user time divided by QPC wall time. **100% means
one logical core**; divide by 16 for this computer's total capacity. Process CPU
excludes Windows audio-engine/Bluetooth-service/driver work outside the player.
All tools/player runs are unelevated and process priority is NORMAL=32. Native
MMCSS registration and output-worker priority 13 are verified from startup logs.
No power plan, affinity, format, driver, registry, scheduling or keepalive changed.
The ten-second warm-up precedes each window and includes output wake-up; this is
steady playback CPU, not resume latency or acoustic latency.

The primary contrast is candidate minus baseline CPU percentage points, paired
by adjacent process launches. Positive means more CPU. The practical positive
bound **+0.10 points** was recorded before the first playback observation.
Material reproduction requires mean ≥0.10, paired-bootstrap lower endpoint >0,
and exact two-sided sign-flip p<0.05. An interval wholly below +0.10 is reported as
below that declared bound, without asserting exact equality. Otherwise the result
is inconclusive at that bound. This local bound is not a universal definition of
an acceptable regression.

## Results

| Matched pair | Order | Baseline CPU % | Candidate CPU % | Candidate − baseline, percentage points |
| --- | --- | ---: | ---: | ---: |
| 1 | AB | 0.7943 | 0.7682 | -0.0260 |
| 2 | BA | 0.6380 | 0.6901 | +0.0521 |
| 3 | AB | 0.6250 | 0.7682 | +0.1432 |
| 4 | BA | 0.5859 | 0.6380 | +0.0521 |
| 5 | AB | 0.6901 | 0.6250 | -0.0651 |
| 6 | BA | 0.6510 | 0.7031 | +0.0521 |
| 7 | AB | 0.5859 | 0.6901 | +0.1042 |
| 8 | BA | 0.6120 | 0.6120 | +0.0000 |

- Baseline mean CPU: **0.6478%** of one core,
  range **0.5859–0.7943%**.
- Candidate mean CPU: **0.6868%** of one core,
  range **0.6120–0.7682%**.
- Mean paired absolute change: **+0.0391 percentage points**.
- Descriptive paired-bootstrap 95% interval:
  **-0.0049 to +0.0830 points**.
- Exact paired sign-flip, two-sided p: **0.1484375**
  (256 sign assignments; no multiple workload contrasts).

Bootstrap resamples all eight whole pairs, 20000 resamples, fixed seed 20261015.
The small sample and uncontrolled system activity limit uncertainty estimates.
The contrast uses absolute points because relative changes can exaggerate small,
coarsely measured control CPU readings.

**Interpretation:** The earlier +0.182-percentage-point increase was not reproduced at the predeclared +0.10-point practical bound in this longer-window study. The descriptive interval's upper endpoint is below that bound. This does not prove exact CPU equality or the absence of a smaller effect.

The table includes every predefined valid observation, with no selected reruns
or omitted unfavorable pairs. This is a new local paired experiment, not a direct
controlled test proving why the earlier short-window samples were higher. Changed
window duration, elapsed position, longer source and different test time are
documented; the older measurement summaries and conclusions remain below. No compiler,
binary-code layout, counter-accounting, frequency or background-load cause is
established from CPU totals alone.

## Delivery and memory verification

Every window retained the same native endpoint/format, Playing phase and seek
generation, approximately 1× PCM delivery, NORMAL process priority and **zero PCM
underrun frames**. There was no output replacement or logged playback error.
PCM sample storage stayed at **622592 bytes / 608 KiB**, separate from process RAM.
The 1.5-second capacity, 500 ms priming and 60 MiB compressed-cache cap are unchanged.

| Variant | Boundary working set MiB | Boundary private commit MiB | Peak working set / process commit through observation MiB |
| --- | ---: | ---: | ---: |
| baseline | 14.75–14.87 | 3.29–3.45 | 14.88 / 3.49 |
| candidate | 14.77–14.89 | 3.31–3.48 | 14.90 / 3.52 |

These are process-memory boundaries/lifetime peaks through the observation, not
PCM-only counters or process-exit peaks. No memory-saving or leak-free claim is
made. Zero PCM underruns does not establish glitch-free Bluetooth/device output.

## Verification, reproduction and limits

Two offline calculation tests passed: 120-second CPU scaling/delivery with
underrun rejection, and the predeclared decision-boundary cases. Existing paired
analysis reused the original study's paired-comparison helper, whose calculation
tests remain documented in the earlier report. The application code and binaries were
not modified for this comparison; the preceding normal/diagnostic Rust check
results remain documented below. No speculative code fix was made.

Reproduction requires the finite isolated-run helper, paired-analysis tools,
fixtures and matched binaries described above. Use a fresh output directory
and revalidate the active default endpoint, settings and hashes.

The real state and source selection were preserved; isolated state, generated
fixture and task-owned processes were cleaned up. This covers one host, native
device format and bypass workload.
Other sources/settings/hardware, system-wide CPU, physical output
latency, device glitches and multi-hour memory behavior remain unverified.


---

# BT SPEAKER resume-delay investigation — 2026-10-16

This follow-up separates command handling, first application render-callback
entry and first copied PCM. It tests the user's observation that longer pauses
can be followed by Bluetooth wake-up lag, without treating that observation as
an application defect or assuming it identifies the cause of an individual delay.

## Reproduction

Six fresh processes used the same saved local 113.894-second MP3 and frozen EQ,
volume and repeat settings as the preceding study. Before each launch, the
task-owned player/output was closed for sixty seconds. Other applications'
audio and actual radio/device sleep state were uncontrolled; that gap does not
prove the entire endpoint or Bluetooth hardware was idle. Three processes
resumed as soon as restored Paused was observed; three held Paused for one extra
second first. Every process then resumed after nominal 1/15/60/120-second pauses,
with pause order rotated by round and two-second Playing settles between pauses.
This gives **30 resumes: six initial resumes and 24 resumes of an existing stream**.
The source remained well before EOF; no load/seek or generation change occurred
between each pre-resume snapshot and its matching delivery.

All launches verified the native BT SPEAKER endpoint
`{0.0.0.00000000}.{16409415-e7e2-4e08-9290-b04bc91d5f2e}` at **48 kHz stereo F32**.
The real state file was hashed before/after each process and remained unchanged.
The finite controller used production commands and isolated state; no mouse or
keyboard was injected. Player/tools ran unelevated at normal process priority,
with successful native MMCSS registration and output priority 13 in every log.
No power plan, device format, scheduling policy, affinity, driver, registry,
hotkey, PCM/cache capacity or downloader/recovery behavior changed. No keepalive,
silent playback, permanent sampler, polling loop or idle timer was added.

## Diagnostic boundaries

The opt-in release-diagnostics feature now has an independent fixed 2048-slot
first-callback recorder alongside the existing 2048-slot first-PCM recorder.
Each arena records at most one QPC timestamp per armed command/generation;
slots are never reused, wrong generations and later events read no clock, and
overflow is counted. The callback marker observes generation at render entry;
these resume samples require unchanged generation across the request. A separate
owner-thread snapshot reads the generation's existing priming watermark before
resume. It is a latched readiness flag, not an exact queue-depth measurement.

Each result distinguishes request → broker handled, request → first UI sync/draw,
request → first callback entry, callback entry → first copied PCM, and total
request → copied PCM. The output-worker's native IAudioClient::Start completion
is **not** separately timestamped: CPAL play queues a command for its WASAPI
worker. Broker-handled therefore does not mean native Start has completed.
UI draw completion is not compositor presentation. First application PCM is
neither Bluetooth transport latency nor physical audible-output latency.

The render callback reads at most two independent boundary clocks per armed command,
outside normal builds, with no callback allocation, I/O, lock, blocking channel,
sorting or unbounded recording. When timing capture is disabled it reads no
boundary clocks. This instrumented build is for latency diagnosis, not new CPU
comparison evidence. Source snapshots and binary hashes were recorded during
the study and later removed with its temporary artifacts.

## Results

Cells are **median / nearest-rank p95 (min–max), milliseconds**. With n=3 or n=6,
p95 is the sample maximum and is not a well-resolved population tail estimate.

| Condition | n | Primed before resume | Broker handled | First callback | Callback → PCM | Total to PCM | PCM ≥1 s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Fresh, immediate | 3 | 3/3 | 0.051 / 0.060 (0.042–0.060) | 980.087 / 1833.658 (573.527–1833.658) | 0.002 / 0.004 (0.002–0.004) | 980.089 / 1833.662 (573.529–1833.662) | 1 |
| Fresh, 1 s held paused | 3 | 3/3 | 0.078 / 0.208 (0.069–0.208) | 1508.711 / 1807.278 (794.489–1807.278) | 0.002 / 0.002 (0.002–0.002) | 1508.713 / 1807.280 (794.491–1807.280) | 2 |
| Pause 1 s | 6 | 6/6 | 0.077 / 0.109 (0.050–0.109) | 290.083 / 308.978 (270.968–308.978) | 0.001 / 0.003 (0.001–0.003) | 290.086 / 308.980 (270.969–308.980) | 0 |
| Pause 15 s | 6 | 6/6 | 0.104 / 0.341 (0.047–0.341) | 1059.314 / 1226.658 (795.606–1226.658) | 0.001 / 0.003 (0.001–0.003) | 1059.316 / 1226.660 (795.607–1226.660) | 3 |
| Pause 60 s | 6 | 6/6 | 0.058 / 0.582 (0.041–0.582) | 940.915 / 1496.450 (744.068–1496.450) | 0.001 / 0.003 (0.001–0.003) | 940.916 / 1496.450 (744.071–1496.450) | 2 |
| Pause 120 s | 6 | 6/6 | 0.065 / 0.083 (0.044–0.083) | 982.806 / 1472.299 (778.251–1472.299) | 0.002 / 0.003 (0.001–0.003) | 982.807 / 1472.301 (778.252–1472.301) | 2 |

| Nominal pause | Actual broker-pause → resume-request range, seconds |
| --- | ---: |
| 1 s | 1.001–1.005 |
| 15 s | 15.002–15.011 |
| 60 s | 60.002–60.033 |
| 120 s | 120.002–120.042 |

Priming was already marked ready before every resume: **True**.
Maximum broker-handled and first UI sync/draw latencies were
**0.582 ms** and **46.722 ms**.
The maximum callback-entry → first copied PCM interval was
**0.004 ms**.

All six processes retained the same native output key, PCM storage of 608 KiB,
and matching generation across each resume. There were **zero PCM underruns,
zero first-delivery recorder overflow and zero first-callback recorder overflow**,
with no unplanned output replacement or logged playback error.

## Interpretation and limits

The table localizes the observed request-to-PCM delay. For resumes with a ready
priming watermark and a negligible callback-to-PCM interval, most elapsed time
precedes invocation of the application's rendering callback. That does not point
to EQ/decoding, the 500 ms priming policy or UI command handling as the dominant
delay in those samples. It is consistent with output/backend/device resumption;
this study does not separate CPAL command scheduling, native Start, Windows audio
engine, driver, Bluetooth transport or hardware wake-up time.

No product fix is justified merely by a long request-to-PCM reading, and no fix
or universal latency improvement is claimed here. Existing paused behavior still
stops WASAPI and permits workers to park. Keeping Bluetooth awake with silence
would change that behavior and was not used. Audible lag may extend beyond the
first application PCM; it was not measured or ruled out. These are six local
processes on one endpoint/machine, not an estimate for all Bluetooth hardware.
Physical sound capture, radio/power-state tracing and native Start timing remain
unverified. External playback and system scheduling were not controlled.

## Verification

Normal checks passed: cargo check, Clippy with warnings denied, 191 tests (10
ignored), and the normal release build. Diagnostic Clippy, 195 tests (10 ignored)
and the instrumented release build passed. A focused regression verifies that
first callback and first PCM are independent, while the existing generation,
one-shot, stale-entry and bounded-overflow regression remains in place. Two
offline calculation tests verify stage separation and reject stale/reversed
timestamps. No compilation overlapped playback observations.

```text
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
cargo clippy --locked --all-targets --features release-diagnostics -- -D warnings
cargo test --locked --features release-diagnostics
cargo build --locked --release --features release-diagnostics
```

The source music was read only; no generated media was needed.


---

# Second audio device: BT SPEAKER — 2026-10-16

The second-device study uses **Headphones (BT SPEAKER)** on the same computer.
The player directly reported endpoint
`{0.0.0.00000000}.{16409415-e7e2-4e08-9290-b04bc91d5f2e}`,
**48 kHz, stereo, F32**. This is the WASAPI application format, not the Bluetooth
transport codec. Windows endpoint inventory and every run's output key were
recorded during the study. An initial paused probe still selected Realtek; it was
excluded before any playback measurement. The user then selected BT SPEAKER as Windows output,
and two subsequent paused probes confirmed the intended endpoint.

## Procedure and boundaries

The original and optimized EQ release binaries are byte-identical to those in
the Realtek CPU study below, with the same locked compiler/dependencies/default
MMCSS policy and common diagnostic hooks. No application source changed for this
device study. Binary hashes are checked for every launch. The same saved MP3,
volume 100%, active EQ gains `1,5,4,2,3,3,-2,-3,-4,-5` dB, repeat-off settings
and fixture recipes were frozen. Generated fixtures were recreated with the same
FFmpeg: WAV, AAC and FLAC hashes match the previous fixtures; regenerated Ogg
Vorbis container bytes differ. Both builds use the identical frozen new Vorbis
file within this study. Its new/previous hashes and encoding recipe were recorded.

Six complete rounds cover six EQ-on workloads, plus bypass and paused controls:
**96 fresh full-application processes and 96 CPU windows**, each with five seconds
of warm-up and twenty seconds of observation. Case order rotates by round and
paired build order alternates. Native endpoint/format, source bytes/settings,
steady phase/generation, priority class, PCM delivery and underruns are checked.
No replacement stream was accepted. The visible production overlay, UI command
path, decoder, output, logging and persistence workers ran normally. Stage timing
and callback timestamps were disabled; the finite controller slept in CPU windows.
No build, fixture generation or statistical analysis overlapped those windows.

CPU is process kernel+user time divided by QPC wall time. **100% is one logical
core**; divide by 16 for this host's total CPU capacity. Measurements include
ordinary background/clock-frequency variation and process-time quantization.
OS audio-engine and Bluetooth service/driver CPU outside the player is excluded.
Player and tools ran unelevated, NORMAL_PRIORITY_CLASS=32; output startup logs
reported successful MMCSS registration and GetThreadPriority=13. No power plan,
affinity, driver, registry or audio-format setting changed. The real state file
was read only; every process used its own isolated state and production commands.

## Matched CPU results

Each row summarizes six adjacent matched pairs. Relative savings are calculated
per pair before averaging; percentage-point changes are candidate minus baseline.
Ranges retain all observations. Per-case statistics are exploratory.

| Workload | Original CPU % mean (range) | Optimized CPU % mean (range) | Change, percentage points | Mean paired relative saving |
| --- | ---: | ---: | ---: | ---: |
| saved_mp3 | 2.0832 (1.7187–2.4218) | 1.3541 (0.9375–1.6405) | -0.7291 | 34.9% |
| aac48 | 2.2135 (2.1093–2.4999) | 1.2500 (0.7031–2.1093) | -0.9635 | 42.6% |
| vorbis48 | 2.3302 (1.9505–2.8124) | 0.9244 (0.8593–1.0937) | -1.4058 | 59.6% |
| flac96_51 | 9.2183 (8.5935–9.6874) | 3.1639 (2.6561–3.7499) | -6.0544 | 65.6% |
| wav48 | 2.1744 (2.0312–2.2656) | 1.0286 (0.7031–1.4843) | -1.1458 | 52.5% |
| wav192 | 6.6665 (6.1715–7.1093) | 2.4348 (2.0312–2.8124) | -4.2317 | 63.3% |
| eq_off | 0.5338 (0.3125–0.7031) | 0.7161 (0.3906–1.0156) | +0.1823 | — (control) |
| paused | 0.0000 (0.0000–0.0000) | 0.0000 (0.0000–0.0000) | +0.0000 | — (control) |

Process-memory counters across the 96 CPU windows (not PCM-only storage):

| Build | Boundary working set MiB | Boundary private commit MiB | Peak working set / process commit through observation MiB |
| --- | ---: | ---: | ---: |
| baseline | 14.71–15.76 | 3.34–4.14 | 15.76 / 4.17 |
| candidate | 14.71–15.79 | 3.33–4.18 | 15.79 / 4.18 |

The predefined primary contrast equally weights the six EQ-on workloads within
each complete round, then compares the six round-level reductions. Mean relative
CPU reduction: **53.07%**; descriptive paired-bootstrap
95% interval: **50.04–55.45%**;
exact two-sided paired sign-flip p: **0.03125** (64 assignments).
All EQ-on workload means improved: **True**.
The predefined 5%/interval/p/consistency criterion passed: **True**.
Bootstrap resamples complete rounds, 20000 samples, seed 20261015. Six rounds
limit uncertainty resolution and do not represent a population of hardware/users.
Round reductions: 51.62 / 56.46 / 46.28 / 53.34 / 54.66 / 56.05%.

Bypass control mean change: **+0.1823 percentage points**,
paired-bootstrap interval **+0.0521 to
+0.3125 points**, exploratory sign-flip
p **0.06250**. No paused/bypass benefit is
assumed, and small control readings should not be interpreted using relative
percentages. The analysis included all raw pairs and the control uncertainty.
Five of the six bypass pairs increased. The positive bootstrap interval and
exploratory exact p of 0.0625 left a possible small bypass cost unresolved.
The [longer bypass comparison](#longer-eq-bypass-cpu-comparison--2026-10-16)
above subsequently did not reproduce the increase at its predeclared +0.10-point
bound; the cause of the short-window difference and smaller effects remain open.

This comparison estimates the EQ change on BT SPEAKER. Differences from the older
Realtek tables cannot isolate the cost of Bluetooth: the studies occurred at
different times, and system load/cache/frequency/thermal state were uncontrolled.

## Continuous playback soak

The optimized build played a generated 32-minute, low-level 440 Hz FLAC source
(48 kHz stereo) continuously, with the same EQ/volume settings and repeat off.
Thirty sixty-second windows cover **30.001 measured minutes**;
two-second inter-window settles plus warm-up make playback run about 31 minutes.
The longer source avoids EOF/repeat confounds. This is one bounded continuous
run on this endpoint, not a multi-hour endurance study or a long-run comparison
against the original build.

- CPU % of one core: mean **1.1267**,
  range **0.8854–1.5104**.
- Boundary working set: **15.01–15.56 MiB**;
  private commit: **3.40–3.74 MiB**.
- First five / last five window-end working-set means:
  **15.06 / 15.50 MiB**;
  private-commit means **3.46 / 3.67 MiB**.
- Peak working set / process commit through final observation:
  **15.56 / 3.75 MiB**.
- All thirty windows retained one native output key and one generation, phase
  Playing, approximately 1× PCM delivery and **zero PCM underrun frames**.

The first/last-five means increased by about 0.44 MiB working set and 0.21 MiB
private commit. This bounded observation does not establish flat memory use or
exclude a slower leak. The later
[memory-retention controls](#diagnostic-memory-retention-controls--2026-10-16)
above implicated retained diagnostic snapshots, without attributing every byte
of this earlier run's growth to that cause.

All 96 CPU windows also passed endpoint/format, phase/generation, PCM storage and
delivery checks with zero PCM underruns. Paused windows rendered no frames.
PCM sample storage remained **622592 bytes / 608 KiB**, separate from process RAM.
The 1.5-second capacity, 500 ms priming and 60 MiB compressed-cache cap are unchanged.
No physical output capture, listening validation or device-glitch trace was run;
zero PCM underruns does not establish glitch-free Bluetooth or audible latency.

## Separate startup and command timing

Three further warm processes used the saved MP3, with callback timestamp capture
enabled only for this separate check. Each performed twelve pause/resume and
forward/backward seek cycles, then Stop, restart and Quit. All three passes had
zero PCM underruns and recorder overflow. Next/previous, large lists and network
command timing were not repeated on this endpoint.
The first resume in each process is separated from subsequent warm resumes.

Startup begins immediately before process creation. Window-ready is the UI's
ready marker, usable UI is completion of the first production-thread inspection,
and restored-ready is observation of the restored Paused phase. These are warm
launches.

Command latency begins before the finite controller sends the production command.
Dequeue, completed broker handling, first UI sync/draw, matching decoder status,
sync after that status and first copied application PCM are distinct boundaries.
UI draw completion is not compositor presentation; application PCM is not
physical sound. Quit exit ends when the parent observes process termination.
Unarmed/stale timestamps before a request are omitted rather than called zero.
Cells are p50 / nearest-rank p95 (min–max; sample count), in milliseconds.

| Startup boundary | Distribution, ms |
| --- | ---: |
| window_ready | 486.951 / 499.459 (131.783–499.459; n=3) |
| ui_usable | 501.445 / 515.766 (152.299–515.766; n=3) |
| restored_observed | 502.044 / 516.507 (153.072–516.507; n=3) |

| Command | Dequeue | Broker handled | UI sync/draw | Decoder status | UI after status | First application PCM |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| initial_resume | 0.031 / 0.037 (0.027–0.037; n=3) | 0.049 / 0.057 (0.045–0.057; n=3) | 0.464 / 0.526 (0.404–0.526; n=3) | — | — | 46.265 / 1524.820 (32.751–1524.820; n=3) |
| pause | 0.036 / 0.100 (0.017–11.255; n=36) | 0.058 / 0.184 (0.030–11.279; n=36) | 0.637 / 1.262 (0.359–11.853; n=36) | — | — | — |
| resume | 0.068 / 0.207 (0.022–0.681; n=36) | 0.085 / 0.234 (0.036–0.693; n=36) | 0.542 / 1.002 (0.330–2.008; n=36) | — | — | 5.643 / 9.315 (0.860–9.405; n=36) |
| seek_forward | 0.055 / 18.643 (0.019–20.932; n=36) | 0.091 / 18.688 (0.043–20.966; n=36) | 0.119 / 27.881 (0.044–29.646; n=36) | 1.213 / 27.919 (0.222–29.834; n=36) | 1.542 / 30.106 (0.592–30.746; n=36) | 9.787 / 29.211 (3.967–32.261; n=36) |
| seek_backward | 0.072 / 11.500 (0.019–12.242; n=36) | 0.146 / 11.532 (0.039–12.351; n=36) | 0.148 / 11.533 (0.040–12.353; n=36) | 0.353 / 11.706 (0.161–12.515; n=36) | 0.905 / 12.306 (0.476–12.929; n=36) | 11.002 / 22.524 (5.328–22.607; n=36) |
| stop | 0.028 / 0.049 (0.020–0.049; n=3) | 0.053 / 0.092 (0.047–0.092; n=3) | 0.616 / 0.662 (0.484–0.662; n=3) | — | — | — |
| restart | 0.123 / 0.135 (0.033–0.135; n=3) | 0.185 / 0.188 (0.062–0.188; n=3) | 0.655 / 0.686 (0.435–0.686; n=3) | 1.092 / 1.121 (0.971–1.121; n=3) | 1.422 / 1.445 (1.396–1.445; n=3) | 6.303 / 6.876 (5.469–6.876; n=3) |
| quit | 0.037 / 0.044 (0.021–0.044; n=3) | 3.097 / 4.410 (2.903–4.410; n=3) | — | — | — | — |

Quit → observed process exit: **202.779 / 236.736 (156.602–236.736; n=3) ms**. The startup/Stop/restart/Quit samples have n=3, so their p95 is the maximum, not a well-resolved tail estimate.


Initial resume → first application PCM was **1524.820 / 46.265 / 32.751 ms** by process. The single 1.525-second initial-resume outlier is retained separately from the 36 warm resumes (0.860–9.405 ms). Its cause was not established; these observations do not identify Bluetooth transport latency or a reproducible application correctness defect.


## Reproduction, verification and remaining limits

The runs collected plans/results, isolated state, player logs, validated counters,
binary hashes, source recipes, endpoint inventory, frozen settings and schedules.
The local analysis reused paired-comparison and tested percentile helpers.
Reproduction requires those local tools, Python, FFmpeg and the same active default endpoint;
use a fresh output directory and revalidate hashes/settings.
Two offline calculation tests cover native rate/layout parsing, one-core CPU
scaling, PCM delivery at 16/44.1/48 kHz and endpoint mismatch rejection. Required
Cargo checks/build/tests were run after the measurements, with 191 tests passed
and 10 ignored. Three existing offline analysis tests also passed for the reused
percentile/CPU/evidence helpers. The following checks passed during that study:

```text
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

The original bit-identical EQ reference regression and previous check results
remain documented below. There was no new production-code optimization or fix.

The real state hash was unchanged, task-owned processes and isolated state were
cleaned up, and generated fixtures were removed after their hashes were checked.

Coverage now includes two audio endpoints on one computer and this bounded
continuous playback run. Comparable measurements on other computers, multi-hour playback,
physical audible-output latency, Bluetooth transport/device glitches, compositor
and hardware-input timing, network playback and other platforms remain
unverified. Do not generalize these local EQ results to all settings or hardware.


---

# Measured EQ CPU optimization — 2026-10-16

The EQ decoder path now keeps each band's channel history in local f64 variables
for a packet and stores it once afterward. It previously indexed and updated
the Vec history for every sample. Coefficients, arithmetic order, per-band f32
rounding, channel independence, gain rebuilds and packet-boundary history are
preserved. There is no change to audio scheduling, output priority, source
formats, buffer policies, downloader/cache/recovery or hotkeys.

This section supersedes the older assessment's lack of an optimization claim
for **this EQ change**. The historical sections below retain their original
sources, dates and scopes. This is not new CPU evidence for MMCSS promotion.

## Comparable before/after procedure

Baseline: the original EQ loop from `980ed99`. Candidate: packet-local EQ history.
Both include identical opt-in stage profiling hooks and native output identity
export, use the same locked dependencies, compiler and release settings, and
retain the default CPAL realtime feature. Matched source copies, source/binary
hashes and the build helper were recorded during the study and later removed.
Both builds, fixture generation and prerequisite checks finished before the comparison observations
began. The final release-mode equivalence check ran afterward.
Source hashes use UTF-8 with LF line endings; binary hashes cover raw bytes.

The full application ran with the visible production overlay, UI pump, decoder,
WASAPI, logger and state writer, plus a finite diagnostic controller. Commands
used production CMD_TX and UI dispatch. The controller slept throughout each
CPU window; UI/callback timestamp capture and decoder-stage profiling were
disabled. No mouse/keyboard input was injected. The normal player adds no timer,
polling thread or callback recording for this optimization.

Six complete rounds each compared adjacent baseline/candidate processes for six
EQ-on workloads and two controls: **96 fresh processes, 96 observation windows**.
The comparison ran across 15–16 October in the host's Asia/Colombo time zone.
Every process warmed for five seconds and observed twenty seconds. Case order
rotated by round; paired variant order alternated. Each pair used identical
source bytes, settings, phase, duration, device and output format. No power plan,
affinity, process priority, driver, registry or system-cache setting changed.
Normal background activity and CPU-frequency behavior were not controlled.

Host: Windows 11 Pro 26200, AMD Ryzen 9 270, 8 cores/16 logical processors,
Rust 1.99.0. All tools/player processes ran unelevated; measured process class
was NORMAL_PRIORITY_CLASS=32. Every run directly reported MMCSS registration
success and output-worker GetThreadPriority=13. Native endpoint, 48 kHz stereo
F32 and unchanged output identity were checked at both observation boundaries.
No unplanned output replacement was accepted.

The saved source/settings were read once and frozen in the manifest: a local
113.894-second MP3, volume 100%, EQ on with gains
`1,5,4,2,3,3,-2,-3,-4,-5` dB, repeat off. Generated 80-second, low-level 440 Hz
fixtures cover AAC-LC/48 kHz stereo, Vorbis/48 kHz stereo, FLAC/96 kHz 5.1,
PCM16/48 kHz stereo and PCM24/192 kHz stereo. The source hashes and complete
FFmpeg/ffprobe recipes were recorded during the study. Bypass and paused controls
use the same 48 kHz WAV; bypass changes only isolated temporary state. Network was excluded.

CPU is GetProcessTimes kernel+user time divided by measured QPC wall time:
**100% means one logical core**, not the whole machine. Divide by 16 for this
host's whole-machine capacity. Values include the full player and its sleeping
controller, excluding launch/warm-up, parent Python, analysis/export and build
work. Twenty-second windows are subject to process-time quantization and ordinary
background variation; 0.00% is not proof of absolutely no executed instructions.

Every observation checked steady phase/generation, the same native output key,
no PCM underruns and unchanged PCM storage. Playing windows delivered
approximately 1× application PCM; paused windows delivered no frames.
The existing 1.5-second capacity, 500 ms priming and 60 MiB compressed
cache cap remain intact. PCM sample storage remains 622592 bytes / 608 KiB on
this device, separate from total process memory.

## CPU results

All per-workload cells summarize six matched pairs. Percentage-point differences
are candidate minus baseline, so negative values mean less process CPU. Relative
savings are computed within each pair before averaging. Individual workload
statistics are descriptive/exploratory, not multiple-comparison significance
claims. The analysis included run-to-run variation.

| Workload | Baseline mean CPU % (range) | Candidate mean CPU % (range) | Mean change, percentage points | Mean paired relative saving |
| --- | ---: | ---: | ---: | ---: |
| saved_mp3 | 2.3046 (2.0312–2.8123) | 1.3932 (1.1718–1.5625) | -0.9114 | 39.5% |
| aac48 | 2.2785 (1.9531–2.6561) | 1.3021 (0.8594–1.7187) | -0.9765 | 42.8% |
| vorbis48 | 2.2395 (2.0312–2.4218) | 1.3802 (1.0937–1.7968) | -0.8594 | 38.5% |
| flac96_51 | 9.2966 (8.8277–9.8436) | 3.4762 (3.2029–3.8279) | -5.8204 | 62.6% |
| wav48 | 2.5520 (2.1875–2.9686) | 1.0414 (0.7812–1.1719) | -1.5106 | 58.5% |
| wav192 | 6.2498 (5.7808–6.5625) | 2.7083 (2.1093–3.5154) | -3.5415 | 56.5% |
| eq_off | 0.6119 (0.3125–0.8591) | 0.7422 (0.3125–1.1718) | +0.1302 | — (control) |
| paused | 0.0130 (0.0000–0.0781) | 0.0000 (0.0000–0.0000) | -0.0130 | — (control) |

| Workload | Baseline CPU % by pair 1–6 | Candidate CPU % by pair 1–6 |
| --- | --- | --- |
| saved_mp3 | 2.344 / 2.187 / 2.109 / 2.031 / 2.344 / 2.812 | 1.484 / 1.328 / 1.250 / 1.172 / 1.562 / 1.562 |
| aac48 | 2.187 / 2.344 / 1.953 / 2.266 / 2.656 / 2.265 | 0.859 / 1.406 / 1.484 / 1.172 / 1.719 / 1.172 |
| vorbis48 | 2.187 / 2.031 / 2.187 / 2.344 / 2.266 / 2.422 | 1.094 / 1.406 / 1.094 / 1.797 / 1.250 / 1.641 |
| flac96_51 | 8.828 / 9.609 / 9.218 / 9.844 / 9.062 / 9.219 | 3.828 / 3.436 / 3.203 / 3.828 / 3.203 / 3.359 |
| wav48 | 2.266 / 2.812 / 2.734 / 2.344 / 2.969 / 2.187 | 1.172 / 1.172 / 0.781 / 0.937 / 1.092 / 1.094 |
| wav192 | 6.484 / 5.781 / 6.094 / 6.562 / 6.484 / 6.094 | 2.578 / 3.515 / 2.500 / 2.578 / 2.969 / 2.109 |
| eq_off | 0.312 / 0.547 / 0.859 / 0.547 / 0.625 / 0.781 | 0.859 / 0.859 / 0.625 / 0.625 / 0.312 / 1.172 |
| paused | 0.000 / 0.000 / 0.078 / 0.000 / 0.000 / 0.000 | 0.000 / 0.000 / 0.000 / 0.000 / 0.000 / 0.000 |

The predefined primary comparison gives each EQ-on workload equal weight within
each complete round, then compares the six round-level relative savings:

- Mean equal-weight relative CPU reduction: **49.72%**.
- Descriptive paired-bootstrap 95% interval, resampling whole rounds:
  **47.75–51.31%**.
- Exact paired sign-flip, two-sided p: **0.03125** (64 possible sign assignments).
- Every EQ-on workload's mean improved: **True**.
- Round-level relative savings: **52.09 / 45.30 / 51.73 / 49.29 / 49.25 / 50.65%**.

This is one predefined primary contrast, with a 5% practical threshold, rather
than treating all workload p-values as independent discoveries. The bootstrap
uses 20000 resamples and a fixed seed; six rounds give limited uncertainty
resolution. The primary result describes this equally weighted local workload
set, not a population of users, codecs, settings or hardware.

EQ-off and paused results are controls. No CPU benefit is claimed when the EQ
path is bypassed, when all bands are inactive, or while playback is paused.
Control variation remains in the table rather than being discarded.

The bypass control averaged 0.612% baseline versus 0.742% candidate, a
**+0.130 percentage-point** difference. Its paired-bootstrap interval spans
**−0.130 to +0.378 points**, with an exploratory sign-flip p of 0.375; four
pairs increased and two decreased. These observations establish neither a
bypass improvement nor a reproducible regression. They also do not prove the
absence of a small regression. At this low CPU level, avoid interpreting
relative percentages. Paused readings were zero except for one baseline
0.078% window; no paused saving is inferred.

## Profiling and output equivalence

The separate heavier profiling runs timed successful decoder packet-read,
decode, sample-copy, EQ, volume, resampling and channel-mix/copy stages. These
are wall durations including preemption/allocation, not exclusive CPU instruction
time; PCM capacity waits and sample callbacks are excluded. Profiling was not
enabled in any of the 96 comparison CPU windows.

| Profiling case | Original EQ share of measured stage wall time | Original EQ µs/packet | Candidate EQ µs/packet |
| --- | ---: | ---: | ---: |
| saved_mp3 | 84.6% | 402.832 | 117.108 |
| aac48 | 91.1% | 339.915 | 99.504 |
| flac96_51 | 94.7% | 7521.884 | 2012.237 |

The focused regression retains the original sample-at-a-time implementation as
a test-only reference. It requires identical f32 sample bits and f64 history bits
across six rates (22.05/24/44.1/48/96/192 kHz), mono/stereo/5.1/7.1, three gain
patterns, varied packet boundaries, subnormal inputs, silence tails and gain/seek
history rebuilds. No f32/f64 precision reduction, coefficient change, filter
reordering, approximation or output-rate shortcut is used.

All 96 comparison windows had zero PCM underrun frames, unchanged output
identity and stable generation. Playing windows delivered approximately 1× PCM;
paused windows delivered none. This does not
establish glitch-free device output or physical audible-output latency. No
output-glitch trace or listening/waveform study was performed.


| Variant | Boundary working set MiB | Boundary private commit MiB | Peak working set / process commit MiB through observation |
| --- | ---: | ---: | ---: |
| baseline | 14.80–15.82 | 3.34–4.23 | 15.82 / 4.23 |
| candidate | 14.77–15.88 | 3.33–4.23 | 15.88 / 4.27 |

These are process-memory counters, including the common diagnostic plan/controller
and fixed storage. Peaks are process-lifetime peaks through that snapshot, not
window-only peaks or post-export process-exit peaks. No RAM-saving claim is made.

## Verification, reproduction and limits

Required Rust checks and diagnostic-feature checks passed:

```text
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked                              # 191 passed, 10 ignored
cargo build --locked --release
cargo clippy --locked --all-targets --features release-diagnostics -- -D warnings
cargo test --locked --features release-diagnostics # 194 passed, 10 ignored
cargo test --locked --release eq_packet_locals
```

The native endpoint/format export is an owner-thread diagnostic snapshot.
Decoder profiling and its counter test compile only with release-diagnostics;
it exports on track teardown and introduces no sampling worker or idle timer.
The original per-sample EQ helper exists only in tests. Existing callback
allocation, queue/wake, seeking, stream/cache/downloader/recovery, persistence,
playlist indexing and cleanup tests passed. The actual saved state was hashed
before/after and never written; isolated state, generated fixtures and task-owned
processes were cleaned up.

Reproduction requires recreating the local runners and fixtures, building
matched variants, finishing builds/tests, running the
comparison and then analysing/exporting. Never
overwrite an existing result directory; use a new task-owned output directory
for a new experiment. Source recipes and settings must be revalidated when
changing host/device/file. Current measurements exclude network fetches,
physical output glitches, long-duration playback, other devices/machines and
Linux/Wayland. Hardware-wide or all-settings CPU savings,
energy savings and scheduling improvements are not established by this study.


---

# Full-application release assessment — 2026-10-15

This section measures the complete Windows player with its visible 80×96 overlay,
production Win32 message pump, broker, decoder, WASAPI, asynchronous logger and
state writer. It adds no performance optimization and claims no improvement.
The older output-worker study below has narrower CPU/memory boundaries and
must not be substituted for these full-process measurements.

## Measurement method, scope and evidence

Source baseline: `c5161d5a22c92b242a0696fd5514678ba42fc68f`, plus the reviewable
diagnostic additions. The 310-second metadata deadline, literal missing-title
fallbacks and watch-link `list=` canonicalization remain intact. The assessment
made no codec, hotkey, scheduling, affinity or system configuration changes.

Host: Windows 11 Pro 26200, AMD Ryzen 9 270 (8 cores/16 logical processors),
about 15.3 GiB visible RAM, Rust 1.99.0. Output: Speakers (Realtek(R) Audio),
native 48 kHz stereo F32, default shared-mode buffer. Player and tools ran
unelevated; observed process priority was NORMAL_PRIORITY_CLASS (32).
The existing CPAL/MMCSS output-worker policy was preserved. No ETW tracing or
device-output glitch measurement was performed in this assessment.

The real saved selection was YouTube playlist
`PLE9NBypnb2JkRek6qiG0FPPiZYWa8r90F`, entry 1, with no saved local folder.
Its settings supplied every run: volume 100%, EQ enabled, gains
`1,5,4,2,3,3,-2,-3,-4,-5` dB, repeat off. Local scenarios replace only source/
selection in isolated temporary state. The real state file's before/after SHA256
was recorded in the temporary state-preservation log; it was never written by
this work.

Measurements ran sequentially after builds finished. Three fresh processes
repeated each scenario.
Idle/paused/playing/stopped CPU windows warm for two seconds and observe twenty
seconds. Folder and synthetic-list windows observe ten seconds after the same
warm-up; each codec observes eight seconds after two seconds of warm-up. Timing
runs have a finite controller which probes only while awaiting milestones;
`low` disables timing capture and sleeps throughout CPU observation windows.
No permanent sampler, polling worker or idle timer was added. Commands travel
through production CMD_TX, WM_APP_BROKER, broker handling and UI sync. No mouse,
keyboard or manual playback tests were used.

Generated media and temporary LOCALAPPDATA trees were removed after assessment.
FFmpeg 8.1.1 generated sixteen 60-second
440 Hz low-level fixtures: PCM16 (22.05 kHz mono, 44.1 kHz stereo, 48 kHz 5.1/7.1),
float PCM96 stereo, PCM24/192 stereo, FLAC96/5.1 and FLAC48/MKV, MP3/44.1,
Vorbis48 in Ogg/WebM, AAC-LC48 in M4A and AAC-LC44.1 in MP4, AAC-LC24 mono ADTS,
IMA ADPCM48 stereo and MS ADPCM44.1 mono. Probe output documents actual layouts.
The 5000-file folder uses task-owned hardlinks to generated WAV copies in groups
of 1000 (NTFS rejected a single group's 1024th extra link). Equal mtimes make
this a count/selection/scan workload, not a varied-storage corpus. A three-file
folder supplies next/previous tests. The 5000-entry indexed synthetic metadata
contains Unicode titles and literal missing-title entries 4780–4960.

## Measurement boundaries

CPU is process kernel+user time from GetProcessTimes divided by measured QPC
wall time: **100% means one logical core**. Divide by 16 for whole-machine
capacity on this host. CPU includes normal application workers plus the sleeping
diagnostic controller; it excludes child yt-dlp/ffmpeg processes, the parent
Python runner, warm-up, analysis and export. No tracing is mixed into these
windows. Short windows and process-time quantization limit small percentages;
0.00% does not prove that absolutely no instructions executed.

Working set is resident process pages (including shared pages); private commit
is PROCESS_MEMORY_COUNTERS_EX.PrivateUsage. Peak working set and PeakPagefileUsage
are **process-lifetime peaks through that snapshot**, not window-only peaks or
continuous samples. The latter is reported as peak process commit. Tables use
boundary snapshots; transients between them are represented by the OS peak
counters. No external-process memory is added to the player. The diagnostic's
static 2048-slot atomic recorder and plan/controller storage are included.

PCM sample storage is separate: nineteen 4096×2×4-byte allocations = **622592
bytes (608 KiB)** at this output layout, not total process RAM. The existing
1.5-second capacity (rounded to 1.536 s here), 500 ms priming, 60 MiB compressed
cache cap, lock-free PCM handoff and event-driven decoder wake remain unchanged.

All latency timestamps are QPC on the same machine. Request begins immediately
before the finite controller constructs/sends a command. Acceptance ends at
dequeue on the actual UI thread; application ends after broker handling returns.
First sync ends after phase/output/timer updates, overlay draw attempt and drawer
refresh. Matching decoder status and sync after that status are separately
recorded for Opened/Seeked/SeekFailed; stale load identities are rejected.
These are API-side UI boundaries, **not compositor/display scanout latency**.
First application PCM ends after nonempty current-generation samples have been
copied to an output slice, before the callback/backend returns. It includes
priming for loads/seeks, and is **not physical audible-output latency**.

The callback records at most one timestamp in each never-reused static slot,
using bounded atomics and one CAS, with no allocation, locks, logging, I/O,
sorting or retry. Wrong generations and subsequent deliveries read no clock.
Overflow is counted. Inspection completion is a separate controller upper bound
with nominal 10 ms waits; scheduler overshoot is retained. Exact callback/status
timestamps do not inherit that polling quantization. Quit application includes
final-save flush; process-exit timing additionally includes stream/UI retirement,
decoder/logger joins, diagnostic JSON export and parent process-wait return.

Launch starts immediately before CreateProcess via Python Popen. Window-shown
ends after the actual initial layered draw and ShowWindow return. Usable ends
when the UI pump services its first inspection after initial command drain/sync.
Restored-ready is the matching Opened status that stages the saved source paused;
the separately reported observed-ready includes controller delivery. It does not
promise that the PCM pool has already filled its entire capacity.
The first final-binary launch is separated from later warm launches. No
power-setting change was made.

All distribution cells below are **p50 / p95 / p99 / maximum**, nearest rank.
For three samples p95/p99 equal maximum; these are descriptive repetitions,
not high-confidence tail estimates.


## Full-process CPU and memory

| Workload (runs) | CPU % one core, range (median) | Working set MiB | Private commit MiB | Peak WS / commit MiB |
| --- | ---: | ---: | ---: | ---: |
| idle (3) | 0.0000–0.0000 (0.0000) | 14.39–14.43 | 3.16–3.18 | 14.43 / 3.22 |
| paused (3) | 0.0000–0.0000 (0.0000) | 14.76–14.88 | 3.30–3.47 | 14.88 / 3.53 |
| playing (3) | 1.5624–2.3436 (2.2655) | 14.85–14.99 | 3.35–3.56 | 14.99 / 3.56 |
| stopped (3) | 0.0000–0.0000 (0.0000) | 14.82–14.95 | 3.32–3.51 | 14.99 / 3.56 |
| folder_playing (3) | 1.8749–2.4998 (1.8749) | 18.34–18.81 | 6.10–6.60 | 18.81 / 6.70 |
| synthetic_paused (3) | 0.0000–0.1562 (0.0000) | 17.07–17.42 | 4.66–4.94 | 17.42 / 4.98 |

The `low` rows have timestamp recording disabled. Folder/synthetic rows are
separate instrumented workload observations. An open synthetic drawer can
receive ordinary window-system work even though playback/output is deferred.


The following peaks also include command/list work outside CPU windows, through
the last retained workload snapshot. Final diagnostic JSON serialization and
process exit are outside these memory boundaries.

| Scenario | Run-to-run peak working set MiB | Run-to-run peak process commit MiB |
| --- | ---: | ---: |
| idle | 14.39–14.43 | 3.19–3.22 |
| low | 14.86–14.99 | 3.38–3.56 |
| local | 15.95–16.06 | 4.65–4.77 |
| folder | 18.37–18.82 | 6.49–6.70 |
| synthetic | 17.10–17.45 | 4.66–4.98 |
| codecs | 16.95–17.14 | 4.98–5.19 |
| network | 22.33–22.53 | 65.57–65.61 |
| saved | 19.00–19.40 | 64.11–64.16 |

### Codec playback (three 8-second observations per fixture)

| Workload (runs) | CPU % one core, range (median) | Working set MiB | Private commit MiB | Peak WS / commit MiB |
| --- | ---: | ---: | ---: | ---: |
| pcm16-44100-stereo.wav (3) | 1.5623–2.1484 (1.9529) | 14.88–14.95 | 3.40–3.48 | 14.95 / 3.52 |
| float-96000-stereo.wav (3) | 3.9060–4.2966 (3.9061) | 14.94–15.03 | 3.43–3.53 | 15.03 / 3.53 |
| pcm24-192000-stereo.wav (3) | 5.6639–8.0072 (5.8592) | 14.99–15.09 | 3.46–3.60 | 15.09 / 3.60 |
| pcm16-22050-mono.wav (3) | 0.5859–0.9765 (0.9764) | 15.04–15.20 | 3.51–3.71 | 15.20 / 3.72 |
| pcm16-48000-51.wav (3) | 3.3202–5.2729 (4.0972) | 15.19–15.27 | 3.66–3.78 | 15.27 / 3.78 |
| pcm16-48000-71.wav (3) | 4.8825–7.2263 (6.8348) | 15.23–15.32 | 3.71–3.84 | 15.32 / 3.84 |
| flac-96000-51.flac (3) | 9.1788–11.1319 (9.5691) | 15.80–15.89 | 4.23–4.32 | 15.89 / 4.32 |
| mp3-44100-stereo.mp3 (3) | 1.7576–2.3435 (1.9528) | 15.54–15.62 | 3.96–4.07 | 15.90 / 4.34 |
| vorbis-48000-stereo.ogg (3) | 2.3433–2.9292 (2.3436) | 16.42–16.66 | 4.58–4.98 | 16.66 / 4.98 |
| aac-48000-stereo.m4a (3) | 2.1483–2.5387 (2.3436) | 16.19–16.46 | 4.44–4.68 | 16.66 / 4.98 |
| aac-24000-mono.aac (3) | 0.3906–0.9765 (0.7811) | 16.17–16.40 | 4.08–4.59 | 16.66 / 4.98 |
| ima-48000-stereo.wav (3) | 2.3427–3.1246 (2.3435) | 16.16–16.40 | 4.05–4.58 | 16.66 / 4.98 |
| msadpcm-44100-mono.wav (3) | 0.9765–1.7577 (1.5623) | 16.25–16.44 | 4.53–4.62 | 16.66 / 4.98 |
| vorbis-48000-stereo.webm (3) | 2.5390–2.7340 (2.5390) | 16.92–17.13 | 4.93–5.19 | 17.14 / 5.19 |
| flac-48000-stereo.mkv (3) | 1.7578–2.9212 (2.3436) | 16.49–16.97 | 4.89–5.11 | 17.14 / 5.19 |
| aac-44100-stereo.mp4 (3) | 1.9529–2.3437 (2.1481) | 16.50–16.92 | 4.65–4.91 | 17.14 / 5.19 |

These instrumented matrix observations include the actual overlay and source
rate/layout conversion; they are separate from the dedicated low-overhead
steady-playback rows. Eight seconds per file is not full-track/long-duration QA.

Fixtures run in the manifest's fixed order within each process; later memory
rows therefore inherit allocator state and lifetime peaks from earlier tracks.


## Startup and local command latency

| Startup workload/boundary | n | Distribution ms |
| --- | ---: | --- |
| codecs/window_ms | 3 | 118.086 / 855.657 / 855.657 / 855.657 |
| codecs/usable_ms | 3 | 130.312 / 867.702 / 867.702 / 867.702 |
| folder/window_ms | 3 | 121.374 / 137.604 / 137.604 / 137.604 |
| folder/usable_ms | 3 | 1051.769 / 1118.047 / 1118.047 / 1118.047 |
| folder/restored_ready_status_ms | 3 | 1052.861 / 1119.545 / 1119.545 / 1119.545 |
| idle/window_ms | 3 | 117.682 / 125.811 / 125.811 / 125.811 |
| idle/usable_ms | 3 | 133.249 / 138.064 / 138.064 / 138.064 |
| local/window_ms | 3 | 107.399 / 505.377 / 505.377 / 505.377 |
| local/usable_ms | 3 | 124.310 / 519.005 / 519.005 / 519.005 |
| local/restored_ready_status_ms | 3 | 124.360 / 519.124 / 519.124 / 519.124 |
| low/window_ms | 3 | 123.250 / 132.673 / 132.673 / 132.673 |
| low/usable_ms | 3 | 136.084 / 144.934 / 144.934 / 144.934 |
| network/window_ms | 3 | 109.364 / 127.830 / 127.830 / 127.830 |
| network/usable_ms | 3 | 147.690 / 157.149 / 157.149 / 157.149 |
| network/network_ready_status_ms | 3 | 45197.650 / 61780.494 / 61780.494 / 61780.494 |
| saved/window_ms | 3 | 109.820 / 122.713 / 122.713 / 122.713 |
| saved/usable_ms | 3 | 152.063 / 161.059 / 161.059 / 161.059 |
| saved/network_ready_status_ms | 3 | 16271.224 / 19692.371 / 19692.371 / 19692.371 |
| synthetic/window_ms | 3 | 126.527 / 145.213 / 145.213 / 145.213 |
| synthetic/usable_ms | 3 | 136.719 / 156.302 / 156.302 / 156.302 |

The first final-binary launch was local-1; local-2 and local-3 are its warm
repetitions. This distinction is observational; startup differences are not
attributed solely to caches. The detailed samples follow.

| Scenario/run | Window shown ms | UI usable ms | Restored status ms | Observed ready ms |
| --- | ---: | ---: | ---: | ---: |
| codecs-1 | 855.657 | 867.702 | — | — |
| codecs-2 | 111.157 | 123.114 | — | — |
| codecs-3 | 118.086 | 130.312 | — | — |
| folder-1 | 113.014 | 1030.407 | 1032.318 | 1034.268 |
| folder-2 | 121.374 | 1051.769 | 1052.861 | 1055.716 |
| folder-3 | 137.604 | 1118.047 | 1119.545 | 1121.079 |
| idle-1 | 115.412 | 128.957 | — | — |
| idle-2 | 117.682 | 133.249 | — | — |
| idle-3 | 125.811 | 138.064 | — | — |
| local-1 | 505.377 | 519.005 | 519.124 | 520.050 |
| local-2 | 106.017 | 121.099 | 121.151 | 121.628 |
| local-3 | 107.399 | 124.310 | 124.360 | 124.818 |
| low-1 | 119.164 | 134.113 | — | 134.624 |
| low-2 | 123.250 | 136.084 | — | 136.820 |
| low-3 | 132.673 | 144.934 | — | 145.405 |
| network-1 | 127.830 | 157.149 | — | — |
| network-2 | 109.364 | 147.690 | — | — |
| network-3 | 105.880 | 132.981 | — | — |
| saved-1 | 105.026 | 161.059 | — | — |
| saved-2 | 109.820 | 140.372 | — | — |
| saved-3 | 122.713 | 152.063 | — | — |
| synthetic-1 | 126.527 | 136.719 | — | — |
| synthetic-2 | 145.213 | 156.302 | — | — |
| synthetic-3 | 116.513 | 128.151 | — | — |

Local commands: three independent processes, twelve repeated pause/resume,
forward/backward seek and next/previous sequences per process, after a two-second
playback warm-up. Initial resume is also retained (39 resumes vs 36 other repeated
commands). Stop/restart/Quit have three samples each.

| Command | Acceptance ms | Broker applied ms | First UI sync ms | Status UI sync ms | First PCM ms |
| --- | --- | --- | --- | --- | --- |
| pause | 10.201 / 12.280 / 13.203 / 13.203 | 10.232 / 12.315 / 13.250 / 13.250 | 10.853 / 12.887 / 13.926 / 13.926 | — | — |
| resume | 0.065 / 0.228 / 1.140 / 1.140 | 0.088 / 0.251 / 1.164 / 1.164 | 0.514 / 1.067 / 2.523 / 2.523 | — | 5.895 / 10.302 / 30.659 / 30.659 |
| seek_forward | 10.229 / 16.983 / 23.331 / 23.331 | 10.270 / 17.017 / 23.363 / 23.363 | 10.272 / 27.224 / 33.023 / 33.023 | 10.984 / 27.650 / 33.527 / 33.527 | 23.606 / 28.234 / 34.336 / 34.336 |
| seek_backward | 0.041 / 0.096 / 2.088 / 2.088 | 0.067 / 0.154 / 2.130 / 2.130 | 0.069 / 0.157 / 2.132 / 2.132 | 0.834 / 1.065 / 4.509 / 4.509 | 11.925 / 17.241 / 19.017 / 19.017 |
| next | 0.045 / 0.104 / 0.202 / 0.202 | 0.078 / 0.169 / 0.235 / 0.235 | 0.632 / 1.214 / 1.504 / 1.504 | 1.520 / 2.466 / 6.869 / 6.869 | 14.247 / 21.206 / 28.622 / 28.622 |
| previous | 10.251 / 12.305 / 20.067 / 20.067 | 10.294 / 12.340 / 20.108 / 20.108 | 10.895 / 13.936 / 35.862 / 35.862 | 11.558 / 16.500 / 36.498 / 36.498 | 24.423 / 29.001 / 30.286 / 30.286 |
| stop | 10.693 / 11.098 / 11.098 / 11.098 | 10.732 / 11.136 / 11.136 / 11.136 | 11.360 / 11.730 / 11.730 / 11.730 | — | — |
| restart | 0.156 / 0.159 / 0.159 / 0.159 | 0.200 / 0.200 / 0.200 / 0.200 | 0.698 / 0.701 / 0.701 / 0.701 | 1.219 / 1.511 / 1.511 / 1.511 | 14.729 / 15.060 / 15.060 / 15.060 |
| quit | 10.293 / 10.785 / 10.785 / 10.785 | 14.150 / 14.321 / 14.321 / 14.321 | — | — | — |

Local Quit request → process exit: **41.507 / 47.029 / 47.029 / 47.029 ms**.


| Command | First-PCM p50 by run 1 / 2 / 3, ms | First-PCM p95 by run 1 / 2 / 3, ms |
| --- | --- | --- |
| resume | 5.732 / 5.895 / 6.228 | 10.026 / 30.659 / 10.302 |
| seek_forward | 23.606 / 23.848 / 20.492 | 28.234 / 34.336 / 28.032 |
| seek_backward | 11.925 / 10.843 / 11.825 | 16.382 / 19.017 / 17.241 |
| next | 15.169 / 11.768 / 14.209 | 21.206 / 19.366 / 28.622 |
| previous | 25.437 / 21.949 / 24.392 | 28.787 / 29.001 / 30.286 |

## Large folders and controlled streaming lists

All three folder runs restored **5000** entries with the saved **2500th**
source selected. Dropping the same folder rescanned it through production
classification/sorting; each run then made five jumps to each of 1/2500/5000.
Selected title, source filename and current index agreed. The real drawer was
opened and painted with UpdateWindow; jumps refreshed its selected-row snapshot.

| Operation | Distribution ms |
| --- | --- |
| Folder drop → broker applied | 449.033 / 449.668 / 449.668 / 449.668 |
| Folder drop → fresh PCM | 460.164 / 472.542 / 472.542 / 472.542 |
| Folder drawer creation/paint | 18.791 / 20.666 / 20.666 / 20.666 |
| Folder jump 1 → PCM | 23.742 / 32.094 / 32.094 / 32.094 |
| Folder jump 2500 → PCM | 24.036 / 30.367 / 30.367 / 30.367 |
| Folder jump 5000 → PCM | 23.082 / 28.626 / 28.626 / 28.626 |
| Synthetic parse + queue application + sync | 9.868 / 25.286 / 25.286 / 25.286 |
| Synthetic drawer creation/paint | 13.764 / 26.010 / 26.010 / 26.010 |

Folder scanning/restoration are synchronous UI/broker work: shallow scanning
does not make a large folder nonblocking. The roughly 0.45-second drop handler
and roughly one-second restore are a measured responsiveness limitation. No
unrelated asynchronous-scan refactor or optimization was introduced.

The synthetic list uses production indexed parsing (including 181 literal
missing-title fallbacks) and actual list presentation, with output deferred via
the existing unavailable-output path. No network/decode latency is implied. Each
run made twelve jumps to each of 1/2500/5000, checking selection and immediately
persisted playlist index. The parse figure includes queue setup and sync; it is
not isolated parser CPU time.

| Synthetic jump | Broker applied ms | UI sync ms |
| --- | --- | --- |
| 1 | 0.100 / 0.215 / 0.237 / 0.237 | 0.881 / 1.248 / 1.671 / 1.671 |
| 2500 | 0.086 / 0.297 / 3.133 / 3.133 | 0.934 / 1.618 / 4.187 / 4.187 |
| 5000 | 0.089 / 0.169 / 0.169 / 0.169 | 0.962 / 1.562 / 1.601 / 1.601 |

## Network workloads (separate from controlled results)

The supplied workload is [this 4960-entry YouTube playlist](https://www.youtube.com/playlist?list=PL_vapIQyT9HiGi47zcVlp9iFS3KuZgmIM).
Its first entry supplied actual playback; jumps to 1/2480/4960 verify selection,
title and persistence through the UI command path. Jumped-to network entries
were cancelled without waiting for their audio, so these jumps do not establish
that every entry is playable. The final entry is an unavailable-title placeholder.

| Workload (runs) | CPU % one core, range (median) | Working set MiB | Private commit MiB | Peak WS / commit MiB |
| --- | ---: | ---: | ---: | ---: |
| supplied playlist playing (3) | 1.7968–2.5781 (2.4218) | 15.57–17.06 | 63.83–63.99 | 17.06 / 64.08 |
| saved playlist playing (3) | 1.7968–2.2655 (1.8749) | 15.94–17.98 | 63.86–63.95 | 17.98 / 63.95 |

| Run | Restored paused status from launch s | Metadata observed ready from launch s |
| --- | ---: | ---: |
| network-1 | 45.198 | 164.897 |
| network-2 | 61.780 | 166.860 |
| network-3 | 28.722 | 164.214 |
| saved-1 | 15.720 | 38.114 |
| saved-2 | 19.692 | 42.078 |
| saved-3 | 16.271 | 38.680 |

Metadata ready is the first finite probe seeing loading=false and the complete
expected count. It includes fetch/pagination/retries, indexed JSON parsing,
mailbox application and UI-pump dispatch, plus probe overshoot. Playback and its
20-second observation precede the metadata wait; if metadata completes earlier,
that probe is only an upper bound. Production log timestamps retain the actual
resolved event separately. Network server/cache conditions were not controlled.


| Run | Indexed rows / missing titles | Metadata applied, seconds from logging init | Raw bytes | Selected indices match raw titles/persistence |
| --- | ---: | ---: | ---: | --- |
| network-1 | 4960 / 181 | 164.866 | 456638 | [1, 2480, 4960] / True |
| network-2 | 4960 / 181 | 166.835 | 456638 | [1, 2480, 4960] / True |
| network-3 | 4960 / 181 | 164.192 | 456638 | [1, 2480, 4960] / True |
| saved-1 | 42 / 0 | 6.403 | 2965 | [1] / True |
| saved-2 | 42 / 0 | 9.654 | 2965 | [1] / True |
| saved-3 | 42 / 0 | 8.361 | 2965 | [1] / True |

The log-side boundary begins at logging initialization (after diagnostic-plan
setup) and ends when the UI broker accepts the fully fetched/parsed title list.
Log timestamps resolve to one millisecond. It is a combined startup/fetch/parse/
application wall time, not isolated parser CPU time or a process-spawn interval.
These timing runs include the diagnostic stdout export before parsing. A staged
stream can report Opened before its PCM priming watermark is reached; network
resume samples include remaining download/priming time.
Synthetic parsing/application is separately controlled above. Actual indexed
stdout was replayed through the existing offline broker selection/restoration
regression after all observations.


| Network command | Broker applied ms | First UI sync ms | First PCM ms |
| --- | --- | --- | --- |
| network/network_resume | 0.057 / 0.169 / 0.169 / 0.169 | 1.026 / 1.574 / 1.574 / 1.574 | 382.604 / 3275.938 / 3275.938 / 3275.938 |
| network/network_jump_1 | 0.283 / 0.328 / 0.328 / 0.328 | 1.295 / 1.587 / 1.587 / 1.587 | — |
| network/network_jump_2480 | 0.061 / 0.067 / 0.067 / 0.067 | 2.668 / 3.104 / 3.104 / 3.104 | — |
| network/network_jump_4960 | 0.483 / 25.438 / 25.438 / 25.438 | 3.145 / 49.564 / 49.564 / 49.564 | — |
| network/network_stop | 0.531 / 0.876 / 0.876 / 0.876 | 1.608 / 3.026 / 3.026 / 3.026 | — |
| network/quit | 4.320 / 25.313 / 25.313 / 25.313 | — | — |
| saved/network_resume | 0.052 / 0.054 / 0.054 / 0.054 | 1.033 / 1.258 / 1.258 / 1.258 | 374.151 / 382.416 / 382.416 / 382.416 |

Supplied-playlist Quit → process exit: **43.990 / 53.249 / 53.249 / 53.249 ms**;
saved-source Quit → process exit: **51.623 / 59.322 / 59.322 / 59.322 ms**.


## Underruns, defects, verification and limits

Completed result files: **24**; failed final runs: **0**. Maximum cumulative PCM underruns in a run: **0 frames**; recorder overflow: **0**.


These counters cover startup priming rules, playback and command transitions
within the recorded runs; generation changes are not silently counted as EOF.
Zero PCM underruns does not prove glitch-free device output. Callback gaps were
not used as audible-glitch evidence. No physical waveform/listening validation,
compositor timing, hardware input delivery, output-glitch tracing, alternate
hardware, long-duration playback or Linux/Wayland
was assessed. Child-process CPU/memory is outside the process tables.

No reproducible product correctness defect requiring a fix was found in the
completed controlled workloads. A pilot wrongly expected Next on a one-file
queue to keep playing; production correctly stopped at queue end. The corrected
three-file workload was repeated from scratch. The failed pilot was excluded
from final statistics. The initial
fixture hardlink-limit failure was a generator issue, corrected with groups; it
supplies no player-performance data. No before/after improvement is claimed.

Verification passed with no Rust warnings:

```text
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked                             # 190 passed, 10 ignored
cargo build --locked --release
cargo clippy --locked --all-targets --features release-diagnostics -- -D warnings
cargo test --locked --features release-diagnostics # 192 passed, 10 ignored
cargo build --locked --release --features release-diagnostics
```

New offline tests cover command parameter validation and fixed first-delivery
storage: generation rejection, one timestamp, no clock read for later/wrong
deliveries, no slot reuse and counted overflow. Python tests cover empty/nearest-
rank distributions, one-core CPU scaling and invalid intervals. Existing
allocation-free callbacks, PCM/wake, stream-buffer/cache/recovery, persistence,
playlist index preservation, source canonicalization and cleanup tests passed.
Changes are diagnostic/reporting only; ordinary builds exclude the controller
and its hooks, and retain the existing decoder/downloader/recovery behavior.


---

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

See [vendor patch notes](../vendor/cpal/OUTPUT_STARTUP_PATCH.md) and
[startup diff](../vendor/cpal/startup-observation.patch). Cargo selects the pinned
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
per run, including the small event-wait deadline overshoot.

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
hard real-time scheduling.

`state.cfg` was read, not rewritten.
