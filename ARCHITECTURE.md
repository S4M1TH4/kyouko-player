# kyouko-player — Architecture & Developer Guide

> Primary context source for developing or migrating `kyouko-player` to a new
> (human or AI) maintainer. Everything here reflects the code as it exists on
> disk; file names and symbol names are literal. When this document and the
> code disagree, the code wins — then fix this document.

---

## 1. Executive Summary & Core Pillars

### Project Goal

`kyouko-player` is a minimalist, zero-resource, event-driven desktop media
player for Windows, written in Rust (edition 2024). It presents a small
transparent floating window (80 px wide) and Windows global hotkeys,
streams audio from local files and from web platforms (YouTube,
Bilibili, Niconico) through `yt-dlp`, and **consumes 0% CPU whenever nothing
is happening** — paused, stopped, or idle.

There is no GUI toolkit. Every pixel is software-rendered through GDI into a
layered window; every interaction is a Win32 message; every state change is a
command message through a channel. Release builds use the Windows GUI subsystem
and launch without a console. Debug builds retain console logging; neither
profile has an interactive terminal command interface or stdin-reader thread.
There is no system tray presence. Alt+Shift+/ hides/shows the overlay while
global hotkeys remain active; Alt+Q or the header X quits gracefully.

### The Four Architectural Pillars

1. **Zero-CPU Idle State.** The UI thread parks in `GetMessageW` (kernel
   sleep). The 1 Hz repaint timer exists *only* while `Phase::Playing` —
   pause/stop kill it (`sync()` in `ui/win32.rs`). A pending stream download
   continues real network work while paused; after its buffer completes,
   cached/paused playback has no scheduled wake-ups. The decoder registers and
   rechecks work predicates, then parks in an atomic address wait. The WASAPI
   output stream is stopped whenever
   the phase is not `Playing`. There is **no polling loop anywhere**; every
   wake-up follows a useful atomic notification or posted message. Active network work may
   own a one-shot deadline, cancelled at completion; this never schedules idle
   wake-ups. Sleep-loops and background polling violate this pillar.

2. **Single-Process Purity & Process Hygiene (`ChildGuard`).** Web streams
   are produced by `yt-dlp` child processes. Exactly **one** yt-dlp process is
   alive in the audio pipeline per load. Title prefetch and audio streaming are sequential,
   and the child rides in a shared `Arc<Mutex<Option<Child>>>` slot
   (`ChildGuard`): Stop / Skip / Shutdown kill it mid-read (the parked pipe
   read unblocks with EOF) and it is reaped immediately. No orphans, ever —
   including on debug-console Ctrl+C (`SetConsoleCtrlHandler` in `main.rs`).
   Audio/title children use `SourceChild` with a kill-on-close Windows job:
   interrupts/deadlines terminate inherited ffmpeg/JS helpers too.
   A playlist may additionally own one flat-metadata process and one parked
   deadline watcher. Its separate lifetime is handled by `PlaylistJob`, which
   terminates/reaps and joins workers on replacement, Stop, Quit and Drop.

3. **Central Broker Pattern.** `src/broker.rs` is the **single owner of all
   state**. Every control surface — global hotkeys, window mouse
   handlers, OLE drag-and-drop — is a *producer* that dispatches a
   `Command` enum variant to the broker. Native UI input uses a thread-local
   inbox drained after message dispatch; background/startup producers use
   bounded channels and post a wake. Nobody outside
   `broker.rs` mutates player state. Presentation-only state (window hidden,
   rendering geometry, timer flag) lives in the UI and is excluded from this
   rule.

4. **Sequential No-Leak Lifecycle.** Loads are single-shot and ordered: one
   title resolution (`--print title`, tracked in the child slot) → one audio
   stream spawn → decode loop → EOF advance/replay/stop. Interrupts
   (`DecoderCmd::Load/Stop/Shutdown`) are honored *between* stages, so a
   superseded load never spawns a doomed child. PCM backpressure parks on the
   decoder's atomic address; status backpressure uses a bounded channel.
   The broker's decoder sends never block: under saturation, the dispatcher
   coalesces to the latest Load/Stop plus a combined seek (at most two slots).
   Shutdown is irreversible and supersedes the backlog. A short producer-only
   mutex serializes this compaction; the decoder and audio callback never take it.

### Dependency Profile (deliberately small)

| Crate | Role | Note |
|---|---|---|
| `windows` 0.62.2 | Win32/OLE bindings, feature-gated | see `Cargo.toml` feature list |
| `windows-core` 0.62 (direct dep) | `#[implement]` COM macro support | keeps notification/OLE interface implementations bound to the matching `windows` version |
| `symphonia` 0.5.5 | local-file decode/probe | |
| `cpal` 0.18.2 | WASAPI output | pinned local source patch exposes worker startup; default `realtime` feature requests MMCSS Audio, then TIME_CRITICAL fallback; joins retired workers |
| `crossbeam-channel` | cross-thread message arteries | bounded, no async runtime |
| `atomic-wait` 1.1 | atomic wait/wake | Windows `WaitOnAddress` / `WakeByAddressSingle`; no spinning |
| `serde_json` | escaped playlist title decoding | keeps multiline/Unicode titles at their original playlist index |
| `winres` 0.1.12 (build only) | Windows PE icon/version resource compilation | uses SDK `rc.exe` for MSVC or `windres` for MinGW; compilation failures are fatal |

---

## 2. Codebase Architecture & Module Breakdown

### File-name mapping note

Some historical/task names do not exist as separate files. The mapping is:

| Concept | Actual location |
|---|---|
| "decoder" / stream routing | `src/audio.rs` |
| "source.rs" (platform URL detection) | `src/broker.rs` (`Source`, `is_*_url`, `default_format_for`) |
| "state.rs" (persistence) | `src/config.rs` |

### `src/main.rs` — entry point & orchestration

1. Claims an OS-held `instance.lock` in the state directory before logging,
   restoration, audio or UI setup. Duplicate launches exit without changing
   the running player's state/log/window. The lock releases after normal exit
   or forced termination; the file remains to preserve lock identity.
   `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`
   selects the GUI subsystem for release and the default console subsystem
   for debug. `logging::init()` runs in both; UTF-8 console output and the
   Ctrl+C handler are installed only in Windows debug builds.
2. `config::load()` → restore volume / EQ gains / EQ bypass / loop into `SharedState`
   atomics *before* anything can play; read `last_track` and `last_folder`
   and `last_playlist_index` for the broker's startup command.
3. Creates the core bounded channels and installs the global `CMD_TX`
   (`ui::CMD_TX`, a `OnceLock` — see `ui/mod.rs`).
4. Initial load: optional startup file/URL argument → autoplay; otherwise `Command::RestoreState`
   rescans a valid persisted `last_folder` into the full newest-first local
   queue and selects `last_track` by resolved path (or the first entry if
   absent). Missing/empty folders fall back to the existing `last_track`
   load, including per-platform URL resolution. Restoration is **staged
   paused** — buffered by the decoder, output stopped, 0% CPU until resumed.
   Streaming playlist restoration also retains the saved one-based entry
   instead of always starting at entry 1.
5. Selects WASAPI's native rate/layout before allocating the PCM buffer pool,
   then spawns the decoder (`audio::spawn`) and opens its paused output stream
   with `AudioOut::managed`. Missing devices use a provisional 48 kHz stereo
   PCM pool and suspend through recovery rather than losing the decoder's
   consumer. No stdin reader,
   command parser, help menu, or terminal worker is started.
6. Builds `DecoderLink` (decoder command sender + interrupt kill-switch hook)
   and an owned `SaveWriter`, installing snapshot and final-flush callbacks
   on the broker, then blocks in `ui::run(...)`. The writer parks between
   changes and coalesces to a single pending snapshot. If thread creation
   fails, the application retains synchronous atomic saves.
7. The header X, Alt+Q and Windows session shutdown drive graceful teardown.
   Debug-only `on_console_ctrl` also posts `Command::Quit` on Ctrl+C.

### `src/broker.rs` (~2200 lines) — the central state engine

The largest module. Owns, via `Broker` + `Arc<SharedState>`:

- **`Command` enum** (the complete control vocabulary): `Load{source,paused}`,
  `RestoreState{last_folder,last_track}` (startup only),
  `TogglePause`, `SeekRelative(f64)`, `NextTrack`, `PrevTrack`,
  `LoadDropped(Vec<String>)`, `YtQueueResolved{url,titles}`,
  `JumpToTrack(usize)` (1-based), `Stop`, `SetVolume(f32)`,
  `EqGain{band:Option<usize>,gain_db}` (±12 dB clamp; `None` = all bands),
  `ToggleEq`, `ToggleLoop` (replay-at-EOF; UI-accessible
  via the row-5 `repeat` chunk — refreshes the panel and persists to
  `state.cfg`),
  `ToggleOverlay` (global show/hide), `VolumeDown` (-10 percentage points),
  `VolumeStep(i32)` (signed percentage points, clamped to 0..100%), `Quit`.
- **Queues**: `local_queue: Vec<Source>` + `queue_index` (folder drops /
  multi-file drops, newest-first by mtime); `yt_queue: Vec<String>` +
  `yt_queue_url` (flat playlist fetch). `navigate(dir)` implements
  Next/Prev for both queue kinds; EOF advances playlists/folders, honors
  `ToggleLoop` (replay), and stops gracefully at queue end.
  `TogglePause` from Stopped restarts the selected local queue entry at
  00:00, retaining the full queue and folder context. With no local queue,
  it reloads the last opened stream with its exact format and playlist
  index. EOF remains idle until a command arrives; the Alt+/ pause hotkey
  and floating-window right-clicks outside X use this same restart path.
  Alt+Shift+/ exclusively toggles overlay visibility.
  `queue_index` is zero-based for either queue type. `track_list()` exposes
  an immutable `TrackList { titles, current, loading }` snapshot to the GUI.
  Valid `JumpToTrack` commands update selection, start playback immediately,
  refresh the panel, and persist the selected source/streaming entry before
  decoder `Opened` arrives. The selected title is shown during Loading;
  control characters become spaces so multiline names cannot alter panel
  geometry. Invalid jumps leave selection and persistence
  unchanged. Streaming lists and jumps remain usable after Stop/EOF.
- **Playlist metadata**: the one-shot flat fetch prints the original index
  plus a JSON-escaped title using UTF-8. `parse_playlist_titles` decodes each
  title as one item, preserving multiline/Unicode names and filling gaps
  with unavailable-track placeholders so click indices cannot drift. yt-dlp's
  literal `Unavailable track` fallback bypasses JSON formatting; the parser
  accepts that exact unquoted sentinel while still rejecting other malformed
  JSON. Both
  success and failure enter a one-slot result mailbox and wake the UI pump;
  a paused open drawer therefore updates without adding a timer.
  Each job has a request ID independent of its URL, so an A/B/A switch cannot
  accept the first A's late result. Production explicitly enables fetching;
  offline broker fixtures leave it disabled and inject controlled results.
- **`Source` enum** — the routing heart:
  - `File(String)`
  - `Youtube { url, format, playlist_index: Option<usize> }` — despite the
    name this variant covers *every* yt-dlp platform.
  - `from_raw(&str)`: any `http(s)://` becomes `Youtube`. YouTube URLs with a
    nonempty, exact `list=` query key (including watch/mobile/short links) are
    normalized to `https://www.youtube.com/playlist?list=...`, discarding the
    video, tracking parameters and fragment, and become advancing sources at
    entry 1. Host matching excludes lookalike domains; empty values, other query
    keys and fragments do not trigger normalization. Everything else is a local
    file. `raw()` returns the normalized source (persistence stores it).
  - `default_format_for(url) -> &'static str`: per-site yt-dlp format id —
    YouTube `140`, **Bilibili `30232`** (`bilibili.com/`, `b23.tv/`),
    **Niconico `bestaudio[format_id^=audio-aac-]`** (`nicovideo.jp/`, `nico.ms/`)
    selects the best available AAC-only rendition. A fixed 128 kbps ID fails
    on videos offering 192/64 kbps instead. The same selector goes to title
    resolution and audio download without another probing process. All
    entry paths (startup argument, OLE drop, `.url` shortcut, state restore) funnel
    through `from_raw`, so the format override is automatic.
  - `is_stream_url` = YouTube ∪ Bilibili ∪ Niconico; used by
    `classify_dropped` to give streamable `.url` shortcuts priority in a
    mixed drop (`DroppedBatch::YouTube(url)` wins over media files).
- **Drop classification**: `classify_dropped` + `collect_media_from_paths`
  (folder expansion via shallow scan, `MEDIA_EXTENSIONS` filter
  `[mp3 flac wav m4a aac mp4 mkv webm ogg]`, newest-first merge) +
  `parse_url_shortcut` (`.url` Internet Shortcut `URL=` parser).
  Final merged queue sorting caches each modification-time key once; disk
  metadata reads no longer run inside every sort comparison. Ordering remains
  newest first, with undated entries last.
  The same extension filter applies to direct startup loads and restored file
  history. AIFF, CAF, MP1, MP2 and Opus extensions are rejected because their
  readers or decoders are disabled. Recognized containers can contain unsupported
  codecs (for example ALAC in M4A); decoder construction rejects those tracks.
- **Panel text**: `render_panel(&SharedState, Option<&TrackMeta>)` — the
  five diagnostic text rows (header/time/track/vol/dual
  `eq` + `repeat` state row). Band gains are deliberately *not* text; the
  Braille strip in `render.rs` draws them. The volume and dual EQ/repeat
  status rows retain their existing text formatting and are display-only;
  global hotkeys control those settings.
  The floating renderer replaces only the diagnostic header with KYOUKO
  branding plus a compact phase token; full state words remain useful for logging.
- **Seek bounds**: `bounded_seek_delta` clamps the requested target to zero
  and the open track's known duration before emitting `DecoderCmd::SeekRelative`.
  Unknown-duration streams retain the zero lower bound. No track, non-finite
  deltas, and seeks that cannot move the playhead do not wake the decoder.
  Playing and paused seeks use the same path; the decoder remains authoritative
  about the actual packet position and downloaded-stream availability.
- **Persistence triggers**: `persist()` fires the saver closure on `SetVolume`,
  `EqGain`, `ToggleEq`, and `ToggleLoop` (and on folder-ingestion/opened-track/quit
  paths) with `persist_snapshot()`. Volume hotkeys delegate to `SetVolume` so
  each step clamps, queues a snapshot and refreshes immediately, without decoder commands.
  EQ bypass persists as `eq_enabled`, separately from unchanged band gains.
  Quit enqueues its final snapshot, interrupts the decoder and waits on the
  writer's revision barrier before returning. This also covers WM_ENDSESSION;
  ordinary controls never wait for disk writes. Main joins the writer before
  logging shutdown and releasing the instance lock.
- **`SharedState`** (atomics only, lock-free reads from any thread): volume
  bits, `eq_gains_cdb` (centi-dB `AtomicI32` ×10), `eq_enabled`, `eq_dirty`
  (audio thread rebuilds biquads when taken), position/duration, phase,
  loop. `EQ_MAX_GAIN_DB = 12.0`, `EQ_BAND_HZ` = 31…16k.
- Status handling (`handle_status`): `Opened` / `Progress` / `Finished` /
  failures; phase machine `Stopped → Loading → Playing ⇄ Paused`.
  A current-generation `Failed` retries the exact source once (two attempts
  total). `retry_used` survives `Opened`, so a subsequent stream drop cannot
  grant another retry. A fresh load/navigation/replay resets the budget.
  After the second failure, the broker dispatches `NextTrack` when there is
  a successor, preserving queue context and paused intent. Standalone items
  and exhausted queues stop, even with repeat enabled. Resolved streaming
  playlists stop at their known end; when metadata is unavailable, one
  following entry is probed, then another double failure stops advancement.
  This bound resets after successful EOF or explicit input. Failure status
  carries the attempt's audio generation and full `Source`; obsolete errors
  after retry, Stop, Skip, or Quit are ignored.
  Opened/Finished/Seeked/SeekFailed additionally carry the Load command's
  identity. This identity is captured when queued and survives PCM seek
  generation changes; loads/retries, Stop, Quit and output loss invalidate it.
  The decoder skips obsolete loads and seeks and wakes the UI after its status
  channel closes. Late events cannot resurrect stopped playback or advance a
  newer track, including repeated loads of the same source.
- **Output recovery policy**: `audio_output_lost` captures current play/pause
  intent and position, invalidates old PCM and suspends decoding without clearing
  the queue/folder/selection. User loads and navigation defer while output is
  unavailable; pause, Stop and Quit override automatic recovery. The UI builds
  a replacement and calls `audio_output_prepared(epoch)`. Only matching
  `Status::OutputReady` acknowledges the new PCM pool and reloads the latest
  selection (the history restored from state.cfg remains available). Old
  Opened/Finished events cannot resume/advance while disconnected. Recovery
  loads stage paused until a position-restoring seek completes; `Seeked` or
  `SeekFailed` restores the intended phase, with unseekable sources restarting
  at zero. A previously paused track stays paused. These transient recovery
  fields are not persisted; ordinary atomic track/settings saves remain intact.
  `SuspendOutput` retires only obsolete RAM readers, waking blocked decoding
  without cancelling yt-dlp or its download worker. The decoder retains the
  active source (including incomplete bytes), title and process ownership.
  `RefreshOutput` installs/acknowledges the new PCM pool; `RecoverOutput`
  opens a fresh reader from the same buffer and restores position, even with
  repeat OFF. Normal Load/retry, Stop and Shutdown still cancel/reap the old
  source and release its bytes. New selections while disconnected send a real
  Stop immediately. Saturation coalescing keeps Stop authoritative and preserves
  the output acknowledgement ahead of any pending seek.

### `src/audio.rs` — decoder thread, yt-dlp, output

- **Decoder thread** (`audio::spawn`): parks on command interest; one load at a
  time. `play_source(...)` implements the sequential lifecycle:
  1. `fetch_title_blocking(url, format, playlist_index, child_slot)` —
     `yt-dlp --print title -f {format} [--playlist-items N | --no-playlist]`,
     single-shot, tracked in the shared child slot so interrupts kill it.
  2. Honors a pending `DecoderCmd` (Stop/Shutdown/Load) *before* spawning
     audio (`PlayExit::Stopped/Shutdown/NewSource`).
  3. Local files: symphonia probe/decode. Streams: `yt_dlp_args(format,
     playlist_index, url)` → `yt-dlp -f {format} -o - url` → one transient
     download worker → `StreamBuffer` → symphonia's `MemoryReader`.
  4. Decode loop with `SeekRelative` support, chunk emission
     (`AudioChunk`), `SendOutcome` handling, EOF → `Finished` status.
     Empty/no-decodable-audio sources and stream resets emit `Failed` rather
     than waiting forever for an output-drain event. Stream EOF checks
     yt-dlp's exit code before and after draining the audio tail; a detected
     non-success exit emits `Failed`. Error paths reap the old child before
     reporting failure, then the decoder parks on command interest until
     the broker sends the retry or next-track load. Retry scheduling adds no
     timers, polling, or background workers.
- **`ChildGuard`**: RAII wrapper; Drop kills + reaps. The interrupt hook
  (`DecoderLink::with_interrupt(tx, controls.hook())`) is the kill switch
  that rides with every Stop/Load/Shutdown so a stalled pipe read can never
  defer a command.
- **`AudioOut` / `build_output_stream`**: cpal WASAPI stream on the default
  device; the stream is *stopped* unless `Phase::Playing`
  (`AudioOut::set_playing`) — the zero-CPU paused guarantee. EQ biquads are
  rebuilt inside the decoder when `eq_dirty` is taken.
- **Hot-swap ownership and bounds**: CPAL error callbacks and Windows endpoint
  callbacks only latch bounded events and wake `WM_APP_OUTPUT`; rebuilding and
  destruction occur on the stream's owning UI thread. Stream epochs reject
  late startup results, errors and device advisories from retired workers. Epochs
  are multiples of four; failure and device-advisory flags share the same atomic
  word. A single epoch-qualified CAS prevents an old advisory/error from
  flagging a new stream even when it races retirement. The owner re-queries the current eConsole
  render default, resolves a concrete CPAL endpoint, then selects its native
  rate/layout. Binding a concrete endpoint avoids default auto-follow activation
  changing the UI's COM apartment; the app's own notifications handle routing.
  A fresh preallocated PCM pool is installed through one mutex-protected
  pending slot, consumed only by the decoder at its parked command boundary.
  Rapid replacements keep only the latest pending pool, retaining the original
  consumer until the producer acknowledges, so sink disconnection cannot kill
  the decoder. The old pool is freed after acknowledgement. One decoder thread
  is reused; retiring a callback returns its partial block to its pool.
  Restoring the source uses `DecoderCmd::RestorePosition` for an absolute
  playhead. This producer-thread seek may wait on the stream's condition
  variable for fresh bytes; Stop/Skip/Quit cancel and wake it. Ordinary
  `SeekRelative` commands still refuse undownloaded targets immediately.
  Superseded pool destruction occurs after releasing the pending-slot mutex.
  Cancellation upgrades the buffer's Weak handle under its slot mutex, then
  cancels outside it. Child cleanup takes ownership under the child-slot mutex
  and kills/reaps outside it; blocking process waits never hold that mutex.
  A failed replacement does not create an error-driven rebuild loop: recovery
  waits for another endpoint event, or grants a new attempt after at least
  500 ms of successfully rendered PCM. No retry timer/polling loop exists.
  DeviceChanged advisories probe the default; Xrun/RealtimeDenied advisories
  do not tear down functioning audio.
- **Output priority startup**: the default `output-realtime` Cargo feature enables
  CPAL's maintained `realtime` policy. The pinned `audio_thread_priority` 0.35.1
  implementation dynamically loads avrt.dll and calls
  `AvSetMmThreadCharacteristicsW(L"Audio")`; it does not additionally call
  `AvSetMmThreadPriority`. Registration failure falls back to CPAL's
  `SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL)`. Only output workers are
  affected by this application feature; no process priority, affinity or system
  configuration changes. Windows remains a soft scheduling environment.
  The vendor extension initializes on each new output worker before the callback
  latch, then sends exactly one result through a bounded startup channel. The
  stream constructor waits for that publication outside callbacks. Native TID,
  `GetThreadPriority` snapshot and direct disabled/MMCSS/fallback/failure outcome
  are exposed by the WASAPI stream. Error strings are prepared during startup;
  missing publication is explicitly unknown. `OutputSignals::publish_startup`
  logs on the owner once per current epoch. Priority failure remains observable
  and nonfatal; available audio is not rebuilt because promotion was denied.
  `GetThreadPriority` is the relative-priority API, not a continuous kernel
  priority measurement; ETW CSwitch events verify actual scheduling priorities
  where captured. Registration follows the upstream worker lifetime; stream
  retirement joins that worker. Sample callbacks contain no scheduling setup,
  startup-result publication, log formatting or added blocking work.
  See `vendor/cpal/OUTPUT_STARTUP_PATCH.md` and `startup-observation.patch`.
  The source subset retains Windows WASAPI and Linux ALSA/PipeWire/PulseAudio,
  with shared helpers and license/provenance. Removed platform/feature dispatch
  and manifest entries are documented in `vendor/cpal/PLATFORM_SCOPE.md`.
  These audio backends do not depend on X11 or Wayland; future Linux UI work
  targets Wayland separately. Linux backend implementations were retained intact.
- **PCM handoff**: two preallocated bounded atomic rings (`src/pcm_queue.rs`) circulate a fixed
  pool of 4096-frame blocks at the device rate/layout. Capacity is rounded up
  to at least 1.5 seconds at every supported rate; a 500 ms watermark primes
  each load/seek. Short tracks release priming at EOF. The dedicated decoder
  does all file/stream reads, Symphonia decoding, EQ and volume work. It parks
  on free buffers/ready slots or commands using `src/decoder_wake.rs`.
  The callback only reads bounded rings/atomics, copies or converts directly
  to the device slice, and returns blocks for reuse. No sample allocation,
  freeing, file I/O, logging or SharedState mutex occurs on the audio callback,
  including first use, integer devices and stale-generation discard.
  Each queue operation makes one strong CAS attempt; contention returns to the
  caller without retry/backoff. There is no waiter registry or queue mutex.
  The callback has a finite claim budget based on output length and pool depth.
  CPAL uses `BufferSize::Default` at the native device format. Only rendered audio
  advances the playhead; starvation is counted atomically and never signals
  EOF. EOF drain notifications are armed explicitly after the final chunk.
  Loads/seeks reclaim old queued blocks on the producer even while paused.
  EOF drain uses a generation-qualified atomic mailbox, once per generation.
  Capacity and EOF are combined into one notification per callback, with at
  most one OS wake and none while the decoder is running/uninterested.
  Error/endpoint callbacks also use atomic flags and a coalesced UI wake; error
  logging happens on the stream owner. No callback path sends a Crossbeam event.
- **Decoded PCM format is authoritative**: `AudioBufferRef::spec()` supplies
  the source rate and channel stride for EQ, resampling, channel mixing and
  `Status::Opened`. MP4 sample-entry rates are provisional hints: HE-AACv2
  may advertise 44.1 kHz while Symphonia emits its 22.05 kHz mono AAC core.
  Treating that core as 44.1 kHz doubles speed/pitch. Each decoded format is
  validated, and format changes reset EQ/resampler/interleaving buffers.
  Container `time_base` remains separate for duration and seek timestamps;
  `SharedState.sample_rate` remains the output device rate. EQ bands at or
  above the decoded rate's Nyquist frequency are bypassed to avoid unstable
  filters. Symphonia 0.5 decodes the AAC core; this timing correction does
  not add HE-AAC SBR or parametric-stereo reconstruction.
- Title sidecar: `.title` file next to a local media file overrides the
  display name.
- **Stream cache**: decoder-owned `StreamCache` retains exactly one successful
  completed compressed buffer and title for the exact `Source` (URL, format,
  playlist index). On looping EOF, cache ownership is installed before
  `Finished`; the broker's repeat load opens a fresh reader at byte zero and
  bypasses both title resolution and yt-dlp. The cache is cleared before a
  different track, retry after failure, Stop, Shutdown, or repeat-off EOF.
  The broker explicitly sends Stop at final EOF to cover a repeat-toggle
  race. There is no cache on disk and no cache of whole decoded PCM tracks.
  Device loss uses a separate `SuspendedTrack` owner for the active reader,
  bytes, title, child guard and downloader. Recovery opens a fresh RAM reader
  and transfers those owners after successful probing. Partial downloads
  continue on the original worker; no additional track buffer or source process
  is created. This retention is independent of the repeat setting.
- **Download teardown**: `ActiveTrack` drops its child guard before its
  `DownloadWorker`, which joins the pipe reader. `AudioControls` also cancels
  an incomplete buffer and wakes blocked RAM reads on Load/Stop/Shutdown.
  The control slot holds only a `Weak` reference, so it cannot retain audio
  bytes. Old bytes are released before any new source is opened.
- **Seeking**: global `Alt+,`/`Alt+.` retain ±5-second
  `SeekRelative` dispatch. Container timestamps map to local cached byte
  offsets; changing a memory-reader offset is O(1), while time-to-offset
  lookup follows the container's index. During a user seek, reads and offsets
  beyond downloaded data fail immediately instead of waiting or requesting
  HTTP ranges. The first target packet is prefetched under this restriction,
  then consumed by the decoder. Successful seeks reset codec/EQ/resampler
  state and audio generation and report the actual packet timestamp. Seeks
  remain active during the final audio-drain wait.

### PCM ownership and notification protocol

The ready ring has one publisher (decoder) and two competing consumers: the
renderer and decoder stale reclamation. A maintained SPSC queue such as `rtrb`
cannot provide that second consumer; general MPMC retry loops do not meet the
bounded callback requirement. `PcmQueue` therefore implements a small bounded
attempt ring with per-slot sequence stamps, one claim CAS, and Release/Acquire
value handoff. Non-power-of-two capacities and integer counter wrap are tested.
A stale head snapshot returns `Contended`, rather than falsely reporting empty.
The callback returns silence on contention instead of retrying. Rings are never
cleared or reset concurrently.

There are exactly `depth + 1` blocks; only `depth` can be queued. Every block is
owned by exactly one ready slot, recycle slot, decoder spare/publication, or
renderer current block (including a claim in progress). The spare list is
preallocated to the whole pool size and is touched only by the decoder.

| Transition | Ownership |
| --- | --- |
| Decode/publish | Decoder takes a spare/recycled block, fills it, rechecks commands, then publishes to ready. |
| Interrupted copy | Unpublished block returns to decoder spares, never the recycling ring. |
| Load/seek while paused | Decoder removes obsolete ready entries with bounded competing claims and keeps them as spares. No output callback is required. |
| Render/stale discard | The sole `PcmOutput` claims a ready block, copies/converts or discards it, then returns it to recycle. Construction enforces one rendering/recycling owner per pool. |
| Partial paused block | Remains owned by the renderer; next callback discards a stale generation, or stream retirement returns it. The existing extra pool block covers this owner. |
| Output replacement | UI joins the retired CPAL worker first; its partial block is recycled. Pending replacement keeps the oldest consumer guard alive through coalescing. Decoder installs the newest pool and drops retired owners before `OutputReady`. |
| Stop/error/shutdown | Decoder reclaims queued PCM before parking/exiting; a renderer-owned partial block remains with that renderer until retirement. Receiver guards keep recycle storage alive after decoder disconnection. Final pool destruction occurs outside sample callback invocation. |

The decoder has one `AtomicU32` interest word: zero while running; command,
capacity or drain bits while preparing to park. Idle waits register command;
capacity waits register command + capacity; EOF waits register command + drain.
For every wait it performs an acquiring sequentially consistent swap to register,
then rechecks commands (including disconnection), sink ownership and the relevant
queue/mailbox predicate. Slot and drain stores publish before notification.
Commands, including the last sender's destruction, publish channel state first.

A notifier clears its reason bits with a sequentially consistent RMW and calls
`wake_one` only if an interested bit was set. Proof of no lost wake: if publication
and notification precede registration, the acquiring registration RMW observes
the preceding notification and its published predicate; the recheck finds work.
If notification follows registration, it changes the compared word. A notification
between recheck and `WaitOnAddress` prevents sleeping; one after kernel registration
wakes the waiter. Spurious returns register/recheck again. Cancelling interest
also uses an acquiring RMW, preserving racing notifications' publication chain
through the next registration; a plain store must not replace it. No queue CAS
failure is retried in the callback. Tests enumerate all ten publication/notification
versus registration/recheck/sleep orderings and exercise real command/disconnection
interruptions while the PCM pool is full and no callback runs.

The atomic queue work is bounded; Windows scheduling and address wake primitives
do **not** provide hard real-time guarantees. Decoder starvation and delayed
output callbacks are independent failure modes. `src/audio_performance.rs` is
test-only: callbacks capture raw monotonic entry/end times, absolute QPC, CPAL
callback and predicted-playback timestamps, and frame counts in fixed storage.
The diagnostic separately analyzes actual entry intervals and device-clock
intervals after destroying/joining all writers. Sorting, printing, CSV export
and process CPU accounting stay outside callbacks. A clock regression is
reported, not silently counted as a zero interval; storage overflow is counted.
Application execution excludes CPAL/WASAPI waiting, padding, buffer acquisition,
equilibrium fill, clock calls and buffer release. Callback gaps are not output
glitch counts and PCM underruns are only decoder/handoff starvation evidence.
The opt-in harness uses broker state restoration and optional real worker
replacements with position restoration, without UI input or state saves.
Trace runs are separate from low-overhead runs; an elevated one-shot helper
controls only task-owned WPR recordings while builds/player/analysis stay
unelevated. No tracing helper or recorder remains installed or running.
See [PCM_PERFORMANCE.md](PCM_PERFORMANCE.md).

### Full-application release diagnostics

Full-application diagnostics are separately compiled with the opt-in
`release-diagnostics` feature. `src/ui/win32/performance.rs` runs a finite plan
alongside normal `main` initialization, the actual visible overlay, UI pump,
WASAPI, logger and state writer. A marker beside isolated state is mandatory.
Commands use the bounded production CMD_TX and WM_APP_BROKER path; UI inspections
and explicit drawer/controlled-metadata operations use private message 0x8007.
There is no synthetic mouse/keyboard input. Default builds exclude every hook.

The recorder distinguishes command dequeue, completed broker handling, first
UI sync/draw completion, matching decoder Opened/Seeked/SeekFailed status,
sync after that status, and first PCM copied into an application output slice.
Draw completion is an API-side boundary, not compositor presentation. A static
2048-slot atomic recorder captures at most one QPC timestamp per armed command;
slots are never reused, so a racing older callback cannot fill a newer slot.
Wrong generations and later deliveries read no clock; overflow is counted.
No callback allocation, lock, I/O, formatting, retry loop or analysis is added.

Only the finite controller may inspect repeatedly while awaiting a bounded
milestone. During low-overhead CPU windows, timestamp capture is disabled and
the controller sleeps; there is no application polling thread or idle timer.
GetProcessTimes and process-memory counters are read at window boundaries.
The generated 5000-entry list uses production parsing and presentation, with
source output deferred via the existing unavailable-output path. Its selection
timings are explicitly separate from network/PCM timings. The real metadata
worker may export indexed stdout in diagnostic builds only, outside callbacks.
The local-only `target/release-assessment/release_assessment.py` retains plans,
fixture recipes, logs, process measurements and nearest-rank analysis alongside
it. Python tools and raw artifacts are ignored, rather than versioned source;
the Rust controller and its Cargo feature remain part of the repository.
See README for commands and PCM_PERFORMANCE.md for the assessment.

### `src/stream_buffer.rs` — bounded progressive RAM reader

One `Arc<StreamBuffer>` owns a compressed `Vec<u8>` capped at **60 MiB**.
The first bytes allocate the capacity once, avoiding an old-plus-new Vec
allocation peak during growth. Oversized streams fail through the existing
two-attempt recovery path; they cannot grow memory indefinitely. Normal
reads block on a condition variable until the pipe worker appends bytes or
reports EOF/error/cancellation. Cancellation returns `ConnectionAborted`,
not `Interrupted`: exact-read/container helpers retry `Interrupted` forever,
which would strand the decoder and prevent subsequent streaming loads.
The RAM reader has no polling loop or timer. The independent download worker
owns the progress deadline described in `src/deadline.rs`.

MP4/MKV probing sees progressive input until its final length is known;
downloaded-only seek mode exposes local random access immediately. Completed
readers expose full length/seekability. Replaying creates another cursor on
the same allocation, with no byte copy or network operation. Dropping the
last reader/cache/worker owner deallocates the Vec.

### `src/ui/mod.rs` (100 lines) — UI façade & wake-up routing

- `CMD_TX` global (`OnceLock<Sender<Command>>`) — startup/background command
  pipe; `wake_broker()` posts the broker wake message. Native input instead
  stages in a thread-local inbox, drained at the outer message boundary before
  another input dispatch. Quit is latched with priority and clears that inbox;
  console shutdown also has an atomic request flag, independent of pipe space.
- Wake messages: `WM_APP_BROKER` 0x8001 (command waiting), `WM_APP_STATUS`
  0x8002 (status waiting), `WM_APP_OUTPUT` 0x8003 (output recovery event).
- `set_pump_hwnd` lets any thread post into the pump.
- `run()` dispatches to `win32::run` or falls back to `headless::run`
  (debug diagnostic pump) if window creation fails. Windows release failures
  send `Quit` and return, so no inaccessible background player is left running.
  `KYOUKO_HEADLESS` is a debug-only escape hatch on Windows.

### `src/audio_device.rs` — Windows endpoint notifications

`DeviceNotifications` retains an IMMDeviceEnumerator and IMMNotificationClient,
registering after OLE initialization and unregistering before the UI apartment
is released. Callbacks never enumerate devices, stop streams, or take a mutex.
They hash the OS-owned endpoint ID in place and post a coalesced wake. eConsole
render-default changes and current-endpoint state/removal/format changes force
recovery, including same-ID jack changes that might otherwise silently stall.
Other endpoint events only probe; unchanged healthy outputs do not restart.
Current-endpoint property updates also permit reconnecting drivers to wake a
disconnected player. The debug headless pump selects the same bounded event
mailbox with posted command/status/background wakes through an atomic address
mailbox, without a periodic wake or callback-side channel operation.

### `src/playlist.rs` — owned Windows metadata jobs

One worker owns and reaps the flat-playlist Child; cancellation uses a duplicated
owned process handle, so no child-slot mutex is held while waiting or killing.
The process belongs to a Windows job with KILL_ON_JOB_CLOSE. Cancellation and
deadlines terminate the whole job, including helpers; handles also enforce
cleanup on application exit.
An owned deadline thread parks on a condition variable until completion,
cancellation or 310 seconds (five minutes for paginated full-playlist metadata
plus ten seconds of margin), then terminates stalled work. This deadline is
independent of the 45-second single-track title deadline. Socket timeout/retries
are bounded, stdout is capped at 8 MiB, and indexed parsing caps 100,000 entries.
Each job returns at most one result through its own mailbox; wake notifications
coalesce without blocking on a full command pipe. A request ID and URL fence
results after replacement/cancellation. Drop closes both worker lifetimes and
all handles. No permanent thread, periodic poll, or idle deadline remains.

The supplied 4,960-entry playlist previously exceeded the 45-second metadata
limit: an unbounded diagnostic fetch completed in 98.739 seconds, including a
YouTube HTTP 503 retry, with 456,638 bytes of indexed metadata. The byte/entry
limits were not responsible. The longer deadline remains a finite network
budget, not a guarantee that every server/network condition will complete.
The same playlist also contained 181 missing-title fallbacks at indices
4,780–4,960. Their unquoted `Unavailable track` values previously made JSON
parsing reject the whole list even when the fetch completed; these now retain
their original positions as placeholders.
With both fixes, the production metadata loader accepted the supplied shared
watch link, canonicalized it, and loaded all 4,960 entries in 94.298 seconds
within the 310-second deadline. The captured indexed output also passed broker
selection/persistence/restoration checks at entry 4,960. Offline process tests
generate 5,000 entries including the same 181 literal fallbacks.

### `src/ui/win32.rs` — native presentation and input dispatch

Window: class `kyouko_echo`, `WS_POPUP`, ex-style
`WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE`,
positioned at the work-area top-right. `WS_EX_NOACTIVATE` means the window
never takes focus; interaction relies on hover routing (see wheel note).

- **OLE drag-and-drop** (replaces the legacy `WM_DROPFILES`, which browsers
  never trigger): `OleInitialize(None)` before window creation →
  `RegisterDragDrop(hwnd, &drop_target)` → `RevokeDragDrop` in `WM_DESTROY`
  → `OleUninitialize` after the pump (S_OK *and* S_FALSE both pair). The
  `DropTarget` COM object (`#[windows::core::implement(IDropTarget)]`) holds
  one `AtomicBool` (DragEnter verdict reused by DragOver; no per-move
  queries).
  - **`Drop` extraction priority**: `CF_UNICODETEXT` → `CF_TEXT` →
    registered `UniformResourceLocatorW` / `UniformResourceLocator` →
    `CF_HDROP` fallback.
  - Text payload → `url_from_drop_text` (last `http(s)` whitespace token
    wins; handles `title\nurl` and quoted links) → `Command::Load {
    source: Source::from_raw(url), paused: false }` — the format override
    for Bilibili/Niconico is applied inside `from_raw`.
  - `CF_HDROP` → paths via `DragQueryFileW` over the medium's HGLOBAL →
    `Command::LoadDropped(paths)`.
  - COM hygiene: every `GetData` hit is followed by exactly one
    `ReleaseStgMedium`; `GlobalLock`/`GlobalUnlock` bracket raw reads;
    `DragFinish` is *never* called on OLE-owned memory.
- **`WM_NCHITTEST` routing**: `renderer.client_input_at` marks the header X,
  third panel line (Track Info) and four-row Braille EQ strip as `HTCLIENT`.
  Remaining text, including volume and EQ/repeat status, is `HTCAPTION`, so
  Windows natively drag-moves the window with zero custom drag state.
  Left-clicks on the EQ strip itself are a no-op (tuning is wheel-driven).
  Caption hit-testing is retained for native left-button dragging;
  `WM_NCRBUTTONDOWN` is consumed directly for right-click pause/resume.
- **Interaction handlers** (dispatch through `post_command` → local inbox → broker):
  - `WM_LBUTTONDOWN`: X records a pending quit press and captures the mouse,
    retaining the unchanged surface until matching `WM_LBUTTONUP` is consumed.
    Release posts private `WM_QUIT_FINISH_CLICK` (0x8006); that later message
    sends the same graceful `Command::Quit` as Alt+Q. Capture remains until
    broker shutdown destroys the window, so neither half of the click reaches
    an app underneath. Further X presses/completions are suppressed; capture
    loss/cancellation before release cancels the press without quitting.
    Track Info toggles the frameless track-list drawer. Braille clicks are
    consumed without a command; the status rows remain display-only.
  - `WM_RBUTTONDOWN` / `WM_NCRBUTTONDOWN` post `TogglePause` anywhere inside
    the floating window except the quit X. Client coordinates are used for
    the former; signed screen coordinates are converted against the window
    bounds for the latter (including negative monitor coordinates). Right-up
    and `WM_CONTEXTMENU` are consumed without commands/default processing,
    so one click toggles once and no native/context menu opens.
    No button-state-dependent hit-testing is needed.
  - `WM_MOUSEWHEEL`: lParam is **screen** coordinates → convert against
    `GetWindowRect`. `renderer.eq_band_at(x, y)` → hovered 1/10-width column
    → `Command::EqGain { band, gain_db: ±1.0 }` (broker clamps ±12 dB and
    persists every step). Outside the strip, including the volume and status
    lines and bottom padding, wheel messages are consumed without a command.
    Master volume uses the global hotkeys. Wheel delivery to a never-focused window relies on
    the system "scroll inactive windows" setting (on by default).
  - Helper macros `GET_X_LPARAM`/`GET_Y_LPARAM`/`GET_WHEEL_DELTA_WPARAM` do
    **not** exist in windows-rs 0.62 — local equivalents (`x_lparam`,
    `y_lparam`, `wheel_delta`) live at the top of this file with exact
    sign-extension semantics.
- **Windows global hotkeys**: `WM_CREATE` registers eleven unique window-owned
  IDs with `RegisterHotKey`; `MOD_NOREPEAT` makes each fire once per keypress.
  `Alt+,` / `Alt+.` seek backward/forward 5 seconds; `Alt+Shift+,` /
  `Alt+Shift+.` select the previous/next track; `Alt+/` toggles pause/resume.
  `Alt+L` toggles repeat, `Alt+;` toggles EQ bypass, `Alt+Shift+/` shows/hides
  the floating window, `Alt+Q` requests graceful Quit, and `Alt+-` / `Alt+=`
  change volume by -10 / +10 percentage points. The added keys use `VK_L`
  (0x4C), `VK_OEM_1` (0xBA), `VK_OEM_2` (0xBF), `VK_Q` (0x51),
  `VK_OEM_MINUS` (0xBD), and `VK_OEM_PLUS` (0xBB); existing seek/navigation
  keys retain `VK_OEM_COMMA` (0xBC) / `VK_OEM_PERIOD` (0xBE).
  Bindings use `MOD_ALT` / `MOD_SHIFT` as specified. `WM_HOTKEY`
  maps its ID to the existing broker `Command` and stages native input.
  They remain registered when the echo is hidden. `GlobalHotkeys` tracks
  successful registrations, logs conflicts individually, and unregisters
  those IDs in `WM_DESTROY`; its idempotent Drop also covers early teardown.
  No hooks, polling, or additional worker/timer is introduced. Registration
  exists only in the Windows presentation module.
  The UI intercepts `ToggleOverlay` and uses `ShowWindow(SW_HIDE/SW_SHOW)`;
  hiding also closes an open track drawer. Alt+Shift+/ is the sole visibility
  toggle; playback and global bindings remain active while hidden.
- **Overlay-only presentation**: there is no system tray icon, notification
  registration, tooltip updater, callback message, or Explorer-restart handler.
  There are no context menu resources, menu command IDs or popup helpers.
  Volume, EQ bypass, repeat, pause/resume, seeking and track navigation use
  global hotkeys; overlay right-click also toggles pause/resume outside X.
  Braille gain scrolling and Track Info clicks remain available;
  X and Alt+Q provide graceful exit.
- **Track-list drawer**: a temporary owned `WS_POPUP` tool window uses
  `WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST`, without
  `WS_EX_TRANSPARENT`. `TrackListRenderer` paints a GDI surface at constant
  nonzero alpha (245/255), numbered Unicode titles,
  and a highlight on the selected entry. It shows up to 12 rows and scrolls
  with the wheel, including high-resolution wheel deltas. Placement and hit
  testing use pure `TrackListLayout` helpers and clamp to the owner's monitor
  work area. Clicking an item posts the one-based `JumpToTrack` through
  the native inbox and dismisses the drawer. The popup's `WM_NCHITTEST` explicitly
  returns `HTCLIENT` everywhere inside its screen rectangle, including on
  negative-coordinate monitors. Mouse capture detects outside clicks;
  activation/capture loss and Escape also close it. Only this temporary
  popup activates; the floating echo retains `WS_EX_NOACTIVATE` and native
  dragging outside its interaction zones.
  `WM_MOUSEACTIVATE` returns `MA_ACTIVATE` in the popup itself, so activation
  and the first click are not forwarded to its non-activating owner.
  Mouse-down records the item/dismissal and retains capture without changing
  opacity, visibility, or the native window. Matching button-up commits the
  one-based jump (once), consumes the mouse message, and posts a private
  `WM_TRACKLIST_FINISH_CLICK` message. Only that later message releases capture
  and destroys the popup, after the mouse-up handler has returned. No faded
  input surface or desktop backdrop is created. Further selections and
  scrolling are suppressed during the click and queued close; no timer exists.
  Unrelated button-up and the opening click's release cannot select or close
  the drawer. Drawer state and GDI resources are freed at `WM_NCDESTROY`.
  Capture is released after the click has returned and before destruction (or immediately
  on explicit close/activation/capture loss), and a generation token prevents a delayed
  close notification from clearing a newly opened drawer. Hiding or closing
  the echo destroys its drawer. Queue changes refresh an open drawer only
  through the existing broker wake/tick path; no new polling/timer exists.
- Teardown: `WM_DESTROY` unregisters hotkeys, kills the timer, revokes the
  drop target, closes the drawer and posts quit; the pump reclaims `UiState` and pairs
  `OleUninitialize`.

### `src/ui/render.rs` (~410 lines) — software GDI renderer

Renders the panel into a 32bpp premultiplied-ARGB DIB and composites with
`UpdateLayeredWindow` (explicit position+size — NULL/NULL is a silent no-op
on some builds). No `WM_PAINT` path exists.

The memory DC/DIB persist for the renderer's lifetime. A successful presentation
caches its text, phase and EQ snapshot; identical requests skip GDI drawing,
alpha conversion and `UpdateLayeredWindow`. Only successful presentations are
cached, so output failures remain retryable. Phase changes still update the
accent/header, and EQ-only changes repaint even when the text is unchanged.
The overlay stays at its existing dimensions; changed frames are composed
atomically from the small DIB rather than exposing partially repainted rows.
An open drawer compares its snapshot with borrowed queue titles before cloning
a replacement list, avoiding per-tick title allocations for unchanged queues.

- **Alpha trick**: GDI text never writes alpha → draw white text on a zeroed
  DIB → `GdiFlush` completes batched drawing before raw pixel access → one
  pass sets `alpha = luminance`, `rgb = accent × alpha`
  (premultiplied). Unlit pixels keep an **alpha-1 ground plane** — layered
  windows pass mouse input and OLE drops *through* alpha-0 pixels, so a
  truly empty surface would only accept input on glyph pixels. This was a
  real bug once; do not remove the ground plane.
- **Accent per phase**: Playing = mint `#A8E0BE`; Paused/Stopped = magenta
  `#E436C8`; Loading = sky `#96C8EB` (`accent(Phase)`).
- **Layout** (all geometry constants live here; `win32.rs` imports them so
  draw and hit-testing can never drift):
  - `WINDOW_W = 80`; padding `PAD = 3`; Consolas `-9` em height. The
    interior fits ~14 Consolas cells, so every panel line is tuned to that
    budget (see `render_panel`) — keep new text lines within it.
  - Window height = `2*PAD + panel_rows*line_h + 4*line_h (EQ strip)`.
    The surface ends with 3 px padding below the Braille strip; no bottom
    playback bar, glyphs, separator or reserved hit region remains.
  - **Compact status header**: `PLY` = Playing, `PSD` = Paused, `STP` =
    Stopped, `LDG` = Loading. `status_token(Phase)` in render.rs uses ASCII
    so Consolas/GDI needs no symbol-font fallback. Line 1 restores `KYOUKO`
    at the top-left padding followed by the token (e.g. `KYOUKO PLY`), with
    the separate pinned X at the right. The entire branded header fits inside
    the reserved header rectangle without ellipsis or overlap. The current
    engine has four phases; buffering/errors do not have separate UI phases.
  - **Quit X**: an 8 px-wide box at `x=WINDOW_W-PAD-8..WINDOW_W-PAD`,
    `y=PAD..PAD+line_h` within the existing header row. `quit_button_rect`
    supplies both painting and hit-testing geometry; the centered X uses
    the normal phase accent. Header text clips/ellipsizes before this box
    so it cannot overlap the glyph. Width stays 80 px and height is unchanged.
  - **10-band Braille equalizer** (replaces the old text gain rows):
    - 10 evenly spaced columns (exact tenths of the width), no labels.
    - **Middle-out origin**: an invisible baseline sits between the two
      inner rows. Level 0 renders blank everywhere. Positive gains fill the
      rows *above* the baseline, negative gains the rows *below* — row
      rendering is strictly mutually exclusive (a +5 band leaves all lower
      rows blank; a −5 band leaves all upper rows blank).
    - **Physical GDI rows**, ordered top-to-bottom at
      `eq_top + physical_row * line_h`: row 0 = outer positive (+7..+12),
      row 1 = inner positive (+1..+6), row 2 = inner negative (−1..−6),
      row 3 = outer negative (−7..−12). The DIB is top-down. Unicode Braille
      masks are rasterized with GDI `FillRect` as six separate 2px dots with
      1px gaps, centered inside each physical cell. This avoids font fallback
      and merged dots at the 9px text em; dots cannot paint into neighboring
      rows or bands.
      Gains truncate toward zero for display so fractional values cannot
      activate a row before its ±1/±7 dB threshold.
    - **6-dot glyph progression per row**: positive levels +1→+6 fill
      bottom-up as `⠠` `⠤` `⠴` `⠶` `⠾` `⠿`
      (U+2820, 2824, 2834, 2836, 283E, 283F). Negative levels −1→−6 fill
      top-down as `⠈` `⠉` `⠙` `⠛` `⠻` `⠿`
      (U+2808, 2809, 2819, 281B, 283B, 283F), so the dots themselves also
      grow away from the center baseline.
    - **Multi-row extension**: magnitude 7–12 keeps the inner row full with
      `⠿` and repeats the side's progression in the outer row:
      +7=`⠠` … +12=`⠿`; −7=`⠈` … −12=`⠿`.
      `braille_row_glyph` computes `|level| − 6·row` and selects the
      bottom-up or top-down progression from the sign of `level`.
    - Pure helpers `braille_glyph`, `braille_row_glyph`, `eq_strip_glyph`,
      `eq_strip_row`, `braille_dot_rect`,
      `eq_band_index`, Track Info and client-input geometry are unit-tested
      (`#[cfg(test)] mod tests` at the bottom of the file)
      — keep new geometry in pure functions so it stays testable without
      GDI. An offscreen GDI DIB test also checks physical cell isolation and
      clearing across positive, negative, and zero gain redraws. Pixel tests
      require one separate visible dot component per gain level (1..6),
      catching merged-dot glyphs that still pass string and row-mapping tests.
      An exact-pixel regression checks downward negative growth through all
      twelve levels and retention of the saturated inner row from −7 onward.
  - **Dual status row** (panel row 5): `eq:ON  rep:OFF` — both states
    retain their fixed display columns, with the EQ state padded to 3 cells
    so `rep:` never drifts. The row is display-only; `Alt+;` and `Alt+L`
    toggle EQ/repeat. The volume row also keeps its text and native drag
    behavior; `Alt+-` / `Alt+=` adjust volume.
  - **10-band EQ column width at 80 px**: 8 px per band (`WINDOW_W / 10`);
    `eq_band_index` maps client x by exact tenths of the width, glyphs stay
    extent-centered in their 8 px column.
  - `Renderer::eq_band_at(x, y) -> Option<usize>`: the single source of
    truth for "is this client point over the EQ strip, and which band".
  - `draw(&mut self, hwnd, panel, phase, eq: &[f32; EQ_BANDS])`: full
    repaint; callers pass `shared.eq_gains()`.

### `src/ui/glyph.rs` / `src/ui/headless.rs`

- `glyph.rs`: pure mask math for the procedural mountain+echo executable icon,
  included by the build-time generator rather than the runtime UI.
- `headless.rs`: event-driven diagnostics for debug/non-Windows runs. Windows
  release GUI initialization failures shut down cleanly instead of entering
  a pump without an available user control surface.

### Executable icon resources (`build.rs` / `build/icon.rs`)

Windows builds generate `OUT_DIR/kyouko.ico` from `glyph.rs` geometry, with
4× supersampling and 32-bit alpha DIB frames at 16, 32, 48 and 256 pixels.
`winres` compiles this as the sole `RT_GROUP_ICON`, numeric ID 1, alongside
package version metadata. No external icon asset is needed when running.
Cargo reruns the resource build when `build.rs`, `build/icon.rs` or
`src/ui/glyph.rs` changes. Resource compiler failures abort the build rather
than silently producing an executable without its icon. Executable icon
embedding is independent of the overlay and needs no system tray. SDK discovery works
even when `rc.exe` is absent from PATH.

Tests validate ICO directory/bitmap/transparency structure and use native
Windows resource loading to verify all four embedded resolutions. Set
`KYOUKO_ICON_VERIFY_EXE` to an absolute executable path to run the same native
test against the release PE, without launching it. If a verified PE still
shows an old icon in Explorer, refresh the shell icon cache; cache state is
independent of resource compilation.

### Release launch and source-process policy

The interactive terminal module (`src/terminal.rs`) was removed completely,
along with help formatting, command parsing, queue text printing, and
`ShowQueue`, `DumpState`, and the terminal-only `EqEnabled` command.
Debug builds retain `println!`-based diagnostic logs and console Ctrl+C;
release builds run as pure Win32 overlay applications. Startup file/URL
arguments remain supported for launcher/file-association use.

`src/process.rs` creates source-tool commands with null stdin in both profiles.
On Windows release it adds `CREATE_NO_WINDOW` (0x08000000), preventing yt-dlp
title resolution, audio download, and playlist metadata children from opening
console windows. Existing stdout/stderr pipes and `ChildGuard` lifecycle
remain intact. Debug children retain normal console inheritance.

### `src/config.rs` — persistence ("state.rs" concept)

NaN and infinity are rejected per numeric configuration field. The broker and
atomic setters also reject non-finite runtime controls without saving or
changing existing settings.

`PersistedState` ⇄ `%LOCALAPPDATA%\kyouko-player\state.cfg` (fallback
`./state.cfg`), `key=value` lines: `volume`, `eq_gains`
(comma-separated 10 floats), `eq_enabled`, `loop`, `last_track`, `last_folder`,
`last_playlist_index` (positive, one-based streaming entry). Load is
per-key garbage-tolerant with clamping; save is atomic via the broker's saver
closure. The dedicated writer holds at most one in-flight snapshot and one
pending snapshot. It writes a PID/counter-specific file with `create_new`,
calls `sync_all`, closes the handle and renames within the same directory.
Failure removes its staging file and preserves the previous configuration.
Interrupted processes may leave staging files, which restoration never reads.
Shutdown waits for the final requested revision; worker exit/panic wakes any
barrier instead of stranding it. The OS file lock excludes writers from other
player processes without polling or stale PID detection. Rust 1.89+ provides
the locking API; CI pins the locally tested Rust 1.99 toolchain. `last_track` stores
`Source::raw()` so a restored URL re-parses through `from_raw` and
re-derives the correct per-site format.
GUI jumps persist immediately, including the selected streaming
playlist index. Configs without the new key continue restoring entry 1.

`last_folder` is the absolute path of the active local folder. A successful
startup or dropped folder scan queues it immediately through the same atomic
saver, before decoder metadata arrives. Folder navigation, repeat, and Stop
retain it; explicit standalone file or URL input replaces that context.
For mixed drops, the last successfully scanned folder in input order is
remembered; loose files still join the current merged queue but are not
persisted as a multi-file playlist. Older configs without the key remain
compatible. Startup scans once on the broker thread using the existing
shallow media filter and newest-first sorting; no watcher or idle polling
is introduced. The Track Info drawer lists the full restored queue.

### `src/logging.rs`

`log_error!/log_warn!/log_info!/log_debug!` retain monotonic timestamps and
`KYOUKO_LOG` filtering. Debug builds use one console write per record. Windows
release builds write `player.log` beside `state.cfg`, rotating to
`player.previous.log`; each file is capped at 1 MiB. UTF-8 records cap at 4096
bytes, and a bounded 256-slot queue limits memory and never blocks producers.
Dropped records are counted and reported when space returns or at shutdown.
An owned writer parks on recv, flushes each record, then drains/syncs/joins
when main's logging guard drops after decoder teardown. No disk I/O/logging
is added to the real-time callback. Concurrent log writers are denied rather
than racing file rotation; failed logging does not stop playback.

### `src/deadline.rs` — source-work lifetime bounds

An owned watchdog parks on a condition variable until completion, progress,
or expiry. Title extraction has a 45-second fixed deadline. The independent
download worker permits 60 seconds until first bytes, then resets an idle
deadline to 30 seconds on each successful append. yt-dlp also receives a
15-second socket timeout and one internal request/fragment retry. Expiry
publishes a terminal RAM-reader error and kills the matching process tree;
pipe EOF cannot overwrite that error with success. The normal broker failure
path retains its cap of two playback attempts. Drop cancels and joins the
watchdog immediately; local and completed cached playback own no watchdog.
Recovery seeks waiting for fresh bytes unblock on the same terminal error.

---

## 3. Data Flow & Command Pipelines

Windows CI (`.github/workflows/windows.yml`) runs check, Clippy with denied
warnings, offline tests, release compilation, native icon loading, release
logging and duplicate-release-launch verification on Windows 2025/Rust 1.99.
Checkout is pinned to a verified commit with read-only repository permission;
there are no upload, packaging or publication steps. WAV and AAC/MP4 samples
are generated in Rust test helpers, without fixture files or external encoders.
Persistence tests terminate actual helper processes mid-stage, check exclusion
and automatic lock release, and exercise coalescing/final-flush/panic wake-up.
No synthetic mouse/keyboard input or OS endpoint disabling is used.
The inherited-helper cleanup test launches an ignored Rust fixture from the
current test executable, with no PowerShell or ping dependency. A stdin permit
orders Windows job assignment before helper creation; an explicit readiness
marker has a bounded 30-second allowance for loaded CI hosts. Failures retain
stderr diagnostics and clean up processes before assertions.

### The one rule

**Producers only send `Command`s. `broker.rs` alone decides and mutates.**

```
 ┌─────────────┐   ┌──────────────────┐
 │  Hotkeys    │   │  Win32 window    │
 │ (WM_HOTKEY) │   │  (mouse/wheel/   │
 │             │   │   OLE drop / X) │
 └──────┬──────┘   └────────┬─────────┘
        │ map to Command    │ map to Command
        ▼                   ▼
 ┌─────────────────────────────────────────────────────────────┐
 │    native inbox; drained after message dispatch            │
 │    startup/background: bounded pipes + posted wake         │
 └──────────────────────────┬──────────────────────────────────┘
                            ▼
 ┌─────────────────────────────────────────────────────────────┐
 │  UI pump → broker.rs — the ONLY state owner                 │
 │  • mutates local_queue / queue_index / yt_queue / SharedState│
 │  • persist() → config save (volume, eq_gains, last_track)    │
 │  • emits DecoderCmd + panel refresh                          │
 └───────────┬──────────────────────────────┬──────────────────┘
             ▼                              ▼
 ┌───────────────────────────┐   ┌──────────────────────────────┐
 │ decoder_tx bounded(8)     │   │ take_refresh() → render.rs    │
 │ audio.rs: title prefetch  │   │ draw(hwnd, panel, phase, eq)  │
 │ → yt-dlp spawn (-f fmt)   │   │ UpdateLayeredWindow           │
 │ → symphonia decode chunks │   └──────────────────────────────┘
 └───────────┬───────────────┘
             ▼ status_tx bounded(16): tagged Opened/Seeked/Finished
             ▼ WM_APP_STATUS → broker.handle_status → phase machine
```

### Example: OLE drop of a Bilibili link

```
browser drag → IDataObject (CF_UNICODETEXT)
  → DropTarget::Drop (win32.rs)
      hglobal_string(CF_UNICODETEXT) → url_from_drop_text
  → post_command(Command::Load { source: Source::from_raw(url), paused:false })
      from_raw: is_bilibili_url → format "30232"
  → native inbox → message handler returns → broker.handle_command(Load)
      → DecoderCmd::Load → decoder thread
          fetch_title_blocking(..., format, ...)   # -f 30232
          yt-dlp -f 30232 -o - <url>               # -f 30232
      → Status::Opened → phase Playing → render.rs repaint
```

### Example: wheel over the Braille strip

```
WM_MOUSEWHEEL (screen lParam) → client coords → renderer.eq_band_at
  → post_command(Command::EqGain { band: Some(n), gain_db: old ± 1.0 })
  → broker: shared.set_eq_gain (clamp ±12) → persist() → state.cfg
  → eq_dirty → decoder rebuilds biquads; take_refresh → Braille repaint
```

### Threading model

| Thread | Parks on | Notes |
|---|---|---|
| main/UI | `GetMessageW` | all Win32 + OLE + broker drain happens here |
| decoder | atomic address wait | registered command/capacity/drain predicates; one yt-dlp child at a time |
| WASAPI (cpal) | audio event/commands | initializes observed priority before callbacks; bounded PCM claims, copy/convert, recycle, counters/drain and useful wake |

---

## 4. Rules & Guidelines for Implementing New Features

### 4.1 Adding a new command

1. Add the variant to `Command` in `src/broker.rs` (with a doc comment
   stating units/bounds, e.g. 1-based indices, dB, seconds).
2. Handle it in `Broker::handle_command` — **never mutate state outside
   `broker.rs`**. Presentation-only geometry and timer flags remain in the UI.
3. Wire a producer: global hotkey or mouse path in
   `ui/win32.rs` via `post_command(...)`.
4. If it changes user-visible state, make the broker emit a panel refresh
   (`take_refresh` path) and call `persist()` if it should survive restarts
   (extend `PersistedState` + `config.rs` round-trip + a test).
5. Add a broker unit test (`broker_with_sink()` harness observes
   `DecoderCmd`s and the persistence sink without disk I/O).

### 4.2 Supporting a new streaming platform

1. Add `is_<site>_url(&str)` in `src/broker.rs` (match the domain **and**
   the shortlink host) and include it in `is_stream_url` so `.url` shortcut
   drops take the stream-priority path.
2. Add the site's branch to `default_format_for` with the exact yt-dlp
   format id (measure it with `yt-dlp -F <url>` — the format must exist;
   YouTube's `140` yields an empty stream on Bilibili, which is the exact
   bug class this mechanism exists to prevent).
3. Both spawn passes pick the format up automatically because the format
   lives on `Source::Youtube`: `fetch_title_blocking` (`--print title -f`)
   and the audio child (`yt_dlp_args`). Do not introduce a second format
   constant anywhere in `audio.rs`.
4. Tests: mirror `bilibili_links_select_their_own_format` and
   `dropped_bilibili_shortcut_streams_via_yt_dlp`.

### 4.3 Modifying UI / rendering

1. Keep all geometry in `render.rs` as **pure, unit-tested helpers**; expose
   constants such as `WINDOW_W` and expose renderer input helpers so
   hit-testing can't drift from drawing.
2. If you move/resize Track Info or the EQ strip, update the shared
   `client_input_at` / `track_info_at` / `eq_band_at` geometry used by
   `WM_NCHITTEST`, `WM_LBUTTONDOWN` and `WM_MOUSEWHEEL` in the same change.
3. Never remove the alpha-1 ground plane in the pixel pass (layered-window
   click-through regression), and always pass explicit position+size to
   `UpdateLayeredWindow`.
4. Any new COM object uses `#[windows::core::implement(...)]` — remember
   `windows-core` must stay a direct dependency (see §1 table).

### 4.4 Universal constraints (violating these is a regression)

- **Zero-CPU idle**: no UI timers while paused, no polling loops, no busy
  waits. The only repeating UI timer is the 1 Hz `WM_TIMER` while
  `Phase::Playing`. Pending network work may own a cancellable one-shot
  deadline; completing that work cancels/joins it, including while paused.
- **One yt-dlp at a time**: new async work goes through the decoder's
  sequential lifecycle and the shared child slot.
- **Bounded channels**: new arteries must be bounded; senders may park,
  never spin.
- **`matches!` guard syntax**, `gen` identifiers (edition 2024), and
  Windows-path literals in Rust strings (`\\`) have bitten before — see
  Known Pitfalls below.

---

## 5. Verification & Testing Standards

### Strict compilation gate (mandatory for every change)

```
cargo check --locked              # zero errors AND zero warnings
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked               # offline regressions; environment diagnostics are opt-in
cargo build --locked --release    # zero warnings, clean release binary
```

Zero warnings is a hard rule, not a goal. Win32 type conversions in
particular (`LPARAM` halves, `WPARAM` high words, handle null checks) must
be exact — prefer the local helpers in `win32.rs` over raw bit casts at
call sites.

### Unit tests

Live in each module (`#[cfg(test)] mod tests`): broker queue/classifier
harness (`broker_with_sink`), config round-trips, hotkey mapping,
glyph math, Braille progression/column mapping. Tests avoid disk I/O except
explicitly-created temp files with unique per-pid directory names (parallel
test collisions have happened — keep the `-suffix` convention).

Failure recovery tests exercise retry→success, retry→next track, queue
exhaustion, paused restoration, stale generations, repeat bypass on failure,
and unresolved streaming playlist bounds. A real decoder-thread test feeds
corrupt and empty local audio through the broker, asserting exactly two
attempts per entry and graceful queue exhaustion without a UI/audio device
or network dependency. A subprocess test checks failed/successful stream
exit classification and child cleanup.

Stream-buffer tests cover offset arithmetic, progressive reads, cancellation,
bounded allocation, deallocation on track changes/repeat-off, downloader
joining, and two identical RAM replays without any source process. An in-memory
Rust generator builds AAC-LC silence and a fast-start MP4 container to verify playback and
seeking with only a partial download, immediate refusal of future bytes,
and seeking after completion. Tests require no network, audio device, FFmpeg,
or files in the ignored `tests/` directory. A decode validation checks all
twelve generated packets for 48 kHz mono silence, 1024 frames per packet,
and the complete 256 ms duration.

Cancellation tests also check that an incomplete `read_exact` returns promptly.
Reader-retirement tests distinguish output suspension from permanent download
cancellation. A production-loop regression swaps rate/layout eight times with
repeat ON and OFF, checks original partial-download ownership and completion,
restored position, retired-pool release, no source kill/spawn, and immediate
compressed-buffer deallocation at Stop. Saturated-pipe tests keep a real Stop
authoritative and deliver an output wake before coalesced seeks.
`cargo test live_youtube_decodes_audio -- --ignored --nocapture` explicitly
enables the network test: resolve and decode a YouTube stream, load another
URL on the same broker/decoder, then shut down and release the child/buffer.
For a playlist it also fetches the title list and advances to entry 2,
checking selection and the persistence snapshot. Each load allows 60 seconds.
`KYOUKO_TEST_URL` overrides the first URL, including a playlist source. This
test requires yt-dlp and its supported JavaScript runtime, but no audio device.

PCM tests verify capacity at 44.1/48/96/192 kHz, 500 ms priming and producer
stall tolerance, short tails, stale/paused generations, block reuse, integer
output with changing callback sizes, interruptible backpressure, correct
playhead accounting, load/callback race preservation, and zero callback allocations/deallocations using a
thread-local allocator counter. Decoded-format tests validate rates/channel
stride and invalid specs; one-second 1 kHz tones at 22.05/24 kHz retain
duration/pitch after resampling to 44.1/48 kHz. Low-rate EQ tests verify
unsupported bands do not destabilize audio, and broker tests keep the decoded
source rate separate from the device clock and container duration.
An offline AAC/MP4 regression rewrites only its sample-entry rate in RAM and
requires identical production-decoder PCM samples/frame counts, reported
decoded rate and container duration, without network or audio hardware.
Two opt-in local diagnostics read state.cfg
without saving it: `cargo test saved_folder_full_tracks_decode_through_the_pcm_pool
-- --ignored --nocapture` decodes complete saved-folder tracks through the
normal decoder/EQ/resampler/pool and checks duration/throughput;
`cargo test saved_folder_wasapi_output_has_no_underruns -- --ignored --nocapture`
plays short tracks (up to 30 seconds) to EOF, otherwise ten seconds per track,
on the actual device and checks its atomic underrun count and wall-clock timing.
Both diagnostics accept either the saved folder or a single saved local track.
The full-decode test independently sums decoded frames/rates and compares this
with device PCM duration, allowing compressed-packet padding relative to
container duration. These require local media (and an output device for WASAPI),
so normal unit tests stay independent of user files/hardware. Neither test
injects UI input or changes persisted state.

`cargo test saved_track_device_hotswap_cycles_recover_on_wasapi -- --ignored
--nocapture` restores the saved file/folder and controls, starts playback, and
injects four output-event faults after two-second playback intervals into the same sink
used by CPAL/Core Audio. Real WASAPI streams are destroyed and reopened; tests
cover unavailable/invalidated streams and same-ID silent-stall notifications.
The diagnostic services events during each interval, verifies playback
continues at 1x without underruns after startup PCM priming, checks restored position, unchanged decoder
thread identity, deallocation of retired pools, and OLE STA compatibility.
It disables no OS endpoints, simulates no UI input, and installs no disk saver.
`KYOUKO_HOTSWAP_TEST_FILE` may select an existing local file for deterministic
device timing without changing saved history. `KYOUKO_TEST_URL` selects a
specific network source and resets the playlist entry to 1. Initial load and
each recovery reload have a 60-second allowance. Failed assertions also shut
down/join the decoder and release its child processes. An offline partial-MP4
regression verifies that recovery waits for missing bytes and can be cancelled.
Offline tests cover stale/coalesced callbacks, notification filtering,
different device rates/channel counts, superseded-pool deallocation, partial
block recycling, repeated recovery, stale acknowledgements, queue/selection
preservation and manual pause/Stop/Quit or failed-seek overrides.
Hardening regressions cover same-source load identities after Stop/reload,
stale EOF/seek events, bounded command compaction, simultaneous saturated
status/command pipes, priority native Quit, non-finite settings, A/B/A metadata
request fencing, and real owned-child success/cancellation/deadline cleanup.

Track-list tests check title-row hit geometry, outside dismissal, scroll
bounds, last-entry selection, monitor-edge placement, actual GDI selected-row
pixels, hidden native popup creation/destruction and layered style/alpha,
full-surface client hit testing, click consumption through matching button-up,
unchanged opacity while pressed, and actual deferred destruction through the
private finish-click message (without synthetic mouse input),
and immediate jump
persistence/restoration. `KYOUKO_TRACKLIST_TEST_FILE` optionally supplies the
real indexed yt-dlp metadata output to the long-playlist broker test; normal
offline runs use a synthetic list. No simulated desktop mouse input is used.

Hotkey tests validate all eleven ID/command/modifier/OEM-key mappings and unknown
IDs. Quit-button tests check unchanged dimensions, every hit-box edge, GDI X
pixels and header clipping, plus the same Quit dispatch as Alt+Q.
Quit-click tests cover matching release, capture cancellation, single dispatch,
and a hidden native window that remains alive until the private completion
message queues Quit and the broker sends Shutdown, without synthetic mouse input.
Render tests also compare every branded compact header's real GDI pixels against its full
unclipped rendering, checking left alignment, fixed dimensions and pinned X.
Hotkey mapping tests retain the Alt+Shift+/ overlay visibility command.
Right-click tests check every client/caption pixel, X exclusion, negative
monitor coordinates, out-of-window rejection, release/context suppression
and retained caption dragging, without injecting mouse input.
A hidden native echo test executes `WM_CREATE` registration and
routes the visibility hotkey command through show/hide on an offscreen,
non-activating window, checking that its hotkeys remain registered. It then
routes Alt+Q while hidden, verifies decoder shutdown, performs `WM_DESTROY`
cleanup, and reacquires each available binding to verify its release without
injecting mouse or keyboard input. Broker tests cover
volume hotkey bounds, immediate redraw/save while stopped/paused/playing,
EQ bypass persistence without changing gains, and graceful Quit snapshots/
decoder shutdown. Config tests cover EQ ON/OFF round-trips and enabled defaults
for legacy or invalid keys. Broker seek tests cover both
bounds, zero/no-op/non-finite deltas, missing tracks, unknown durations, and
playing/paused command forwarding. Source-process tests check EOF stdin and
working output pipes. Build verification reads each Windows PE subsystem:
debug = 3 (console), release = 2 (GUI).

### Manual testing protocol (mandatory for UI/drag work)

**Automated synthetic drag/click/wheel scripts (PowerShell, Python, or raw
Win32 `SendInput`/`DoDragDrop` simulations) are strictly prohibited.** A
CCW-based `DoDragDrop` simulation was proven to hang (OLE never calls
`QueryContinueDrag`), and injected-input drags are focus-flaky. The protocol:

1. Build the correct profile (`cargo build --release` for shipped
   behavior — stale-binary confusion has burned whole debugging sessions;
   verify the binary actually rebuilt).
2. For diagnostic logs, launch the debug build with stdout redirected to a
   log file. Exit gracefully using the header X or Alt+Q; no stdin commands exist.
3. **Before** starting the observation pause, print explicit numbered
   step-by-step instructions into the chat so the user knows exactly what
   to do (drop a file, hover a column, scroll, right-click, etc.).
4. Allow the requested observation period (45–60 seconds by default) while
   the user performs the actions on their desktop. Split longer waits into
   chunks of at most 60 seconds so progress and logs remain observable.
5. Verify through the debug stdout log (`drop:`, `wheel eq[...]`,
   `phase:` lines) and, when relevant, the persisted `state.cfg`.

### Known Win32 / environment pitfalls (learned the hard way)

- **Layered-window click-through**: alpha-0 pixels pass mouse input and OLE
  drops to windows below. The 1/255 ground plane in `render.rs` fixes it.
- **`UpdateLayeredWindow` with NULL/NULL** position/size silently does
  nothing on some builds — always pass explicit `pptDst`/`psize`.
- **windows 0.62**: the `implement` cargo feature does not exist (the macro
  is `windows::core::implement`); `ReleaseStgMedium` needs
  `Win32_System_Com_StructuredStorage` + `Win32_Graphics_Gdi` features;
  `GET_X_LPARAM` & co. don't exist (local helpers).
- **`OleInitialize` S_FALSE** still pairs with one `OleUninitialize`.
- **Stale binary trap**: `cargo test` builds a test binary, not the app;
  always rebuild the exact profile before an E2E session and check log
  timestamps.
- **PowerShell 5.1 quirks** (for ad-hoc diagnostics only, never E2E):
  heredocs mangle `$`/backslashes, `[UIntPtr]` casts from `Int32` throw,
  `ComTypes.STGMEDIUM` exposes `unionmember` (not `hGlobal`), ComTypes enum
  members are underscore-named (`TYMED.TYMED_HGLOBAL`), `FindWindowW` may
  fail where `EnumWindows` succeeds, and piped `tail` buffers output.
- **windows-core binding**: keep the direct `windows-core = "0.62"` dependency
  aligned with `windows` so `#[implement]` binds notification/OLE interfaces
  correctly. CPAL 0.18 now uses the same Windows binding version.

---

*Maintainer's note for migrating agents: read `broker.rs` top-to-bottom once
before changing anything — it is the spec. The UI is an honest producer; the
decoder is an honest worker; everything else is detail.*
