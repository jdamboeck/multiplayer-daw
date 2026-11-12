# Slice 2: Optimizations

## Context

The first slice (`docs/plan.md`, milestones M0 to M6) is built. Two reviews on 2026-11-11 checked
it:

- **Review 1** covered the two latest commits (f7156ed, e9869b2) and how they fit the app.
- **Review 2** was an audio performance and best-practice review of the whole audio side.

This slice fixes what they found and makes the audio engine perform like a real DAW. It also picks
up what the first slice's plan promised but did not build.

Each milestone ends as before:

- the gates are green (fmt, clippy `-D warnings`, the tests);
- `docs/manual-tests/o<N>.md` is written;
- work pauses for the user's results.

Commits go on the working branch, and nothing is pushed unless asked.

This version of the plan is detailed down to files, types and numbers. Where the detail work
changed the outline, the section says so, and "Changes from the outline" at the end lists all of
them.

## Decisions

| Question | Decision |
|---|---|
| Pocket's sound changes (smoothing, voice stealing, bypass) | **Replace v1.** `DSP_VERSION` becomes 2 and v1 is not kept. Devices stored as v1 play as v2. The golden hash is renewed. All clients update together: `PROTO_VERSION` goes from 5 to 6, which also changes the store's format key; O4.3 lets the server open a store written under 5 as it is. |
| Protocol bumps | **One bump for the slice.** The O6 field for remote note times is added to `LiveEvent` in O4, with the Pocket bump, and only used from O6 on. |
| Plugin helpers on Windows | **Later slice.** Plugins stay inside mdaw on Windows; this remains a known gap. |
| Rendering ahead | **In this slice, and on by default.** Tracks that nobody plays live are rendered about 85 ms ahead of the playhead, so the callback never waits for their plugins (O5.4). It replaces the outline's pipelined mode. The shared block is built for it in O3.5. |
| How fast is fast enough | **As fast as it can be made.** Where two designs differ in speed, the faster one is taken unless it costs correctness or crash safety. Each one still has to show its gain in `bench-audio`. |
| Audio buffer size | **Device default stays.** A setting lets the user ask for a size (64 to 2048 frames), and the device's answer is shown. |
| Measuring | Every performance step is measured before and after against the baseline from O0. A change that does not measurably help is not kept. |

## The numbers this slice works against

At 48 kHz a buffer of 128 frames lasts 2.67 ms, and the shared plugin deadline
(`DEADLINE_PERCENT = 60` in `rt.rs`) is 1.6 ms. Inside that time the callback today does, for
each plugin in turn, one futex wake, one sleep and one wake-up of its own. On a desktop kernel a
wake-up of a normal-priority thread takes 50 to 200 µs, with outliers of milliseconds; a
`SCHED_FIFO` thread wakes in 10 to 30 µs. Eight helpers in sequence therefore spend 0.2 to 1.6 ms
on wake-ups alone before any plugin has computed a sample. That is the whole budget.

The targets, to be checked against the O0 baseline:

| Measure | Target |
|---|---|
| Callback p99 with 8 helper plugins at 128 frames | under 60% of the buffer |
| Time waiting for helpers, tracks played live | grows with the slowest plugin, not with the sum |
| Time waiting for helpers, all other tracks | none: their audio is already there |
| Plugin load that plays clean at 128 frames | limited by the cores, not by the buffer: 8 helpers at `WORK = 300` on 8 cores, none late |
| Late plugin blocks in a 10-minute run, 8 helpers, 128 frames | 0 with real-time priority; any that happen do not click |
| Clicks on gain, mute, bypass, plugin swap, seek, loop wrap | none (checked by a discontinuity test and by ear) |
| Jitter of live notes | under 1 ms, from up to one buffer today |
| Resync jumps in 20 minutes at 100 ppm drift | 0 |
| Allocations on the audio thread, DAW and helper | 0 in host code |

## O0: Baseline measurements (half a day)

Nothing in this milestone changes how audio is processed. It only adds counters and a tool.

### O0.0 Build settings

The workspace has no `[profile.release]` today, so release builds use sixteen code generation
units and no link-time optimization, and calls between `mdaw-dsp`, `mdaw-engine` and
`mdaw-plugin` are not inlined across crates. Before anything is measured:

- `[profile.release]`: `lto = "fat"`, `codegen-units = 1`, `debug = "line-tables-only"` (so
  `perf` can name functions).
- `[profile.dev.package.mdaw-dsp]`: `opt-level = 3`, so a debug build is usable by ear.
- Every number in this plan comes from a release build. `bench-audio` refuses to run in a
  debug build, and the hand tests say `cargo xtask client --release`.
- Not set: `target-cpu`. The binaries must run on any x86-64 machine, and nothing in the hot
  path is wide enough to need more than SSE2 (see O4.3).
- Profile-guided optimization is tried once at the end of the slice with `bench-audio` as the
  training run, and kept if the callback's mean drops by 5% or more.

### O0.1 Callback statistics

`Shared` in `crates/mdaw-engine/src/rt.rs` gets a `Meter`, written only by the audio thread with
relaxed stores:

- `run_hist: [AtomicU32; 42]`: a histogram of the callback's run time as a share of the buffer's
  length, in steps of 5% from 0 to 200%, and one bucket for everything above. A share, not
  microseconds, so that runs at different buffer sizes compare.
- `run_sum_micros`, `run_max_micros` (the existing `max_micros`), `callbacks` (exists).
- `over_80`, and `overruns` (exists; it counts runs over 100%).
- `buffer_frames`: the largest buffer the device asked for, and `lead_micros`: the last
  `stamp.playback - stamp.callback`. O6 needs both; they are shown here.
- `rt_policy`, `rt_priority`: the scheduling of the callback thread, read once in the first
  callback with `pthread_getschedparam` (O2.6 needs it).

The histogram is never reset on the audio thread. The reader keeps the last snapshot and
subtracts, so every 5 s window has its own mean, p50, p99 and max. `Stats` in `engine.rs` grows
by these fields; `Engine::stats_window()` returns the difference since the last call.

Each helper gets a `PluginMeter` (an `Arc` shared by `IsolatedInstance` and
`IsolatedProcessor` in `isolated/parent.rs`):

- `blocks`, `late` (deadline passed), `skipped` (helper still busy when the next block came);
- `wait_nanos`: the time from the wake to the result, summed.

To see where the wait goes, the shared block in `isolated/shm.rs` gets three time stamps from
`CLOCK_MONOTONIC`: `t_request` (set by the DAW before the wake), `t_start` and `t_done` (set by
the helper). They sit in `Data` first and move into the two sides of the stream block in O3.5. The meter sums three parts from them: wake-up latency (`t_start - t_request`), the
plugin's own work (`t_done - t_start`) and the way back (collected at `- t_done`). O2.6, O2.7
and O3 are judged by these three numbers. `Instance::meter()` hands a snapshot to the UI. The
in-process `ClapProcessor` fills the same meter with only the work part.

### O0.2 Where the numbers are shown

- A debug panel in the client, toggled with F12 (`crates/mdaw-client/src/ui/mod.rs`, a new
  `ui/stats.rs`): the callback histogram as a bar row, mean, p99 and max in µs and percent, the
  buffer size and output latency, the thread's scheduling class, and one row per plugin with
  wake-up, work, late and skipped.
- `--stats` on the client (`main.rs`, next to `--no-plugins`): prints one line per 5 s to stderr
  with the same numbers, for runs without looking at the window.

### O0.3 `cargo xtask bench-audio`

A new command in `xtask/src/main.rs` that builds and runs a new binary,
`crates/mdaw-client/src/bin/bench_audio.rs` (the client crate, because it already links engine,
dsp and plugin host).

- **Workload.** N tracks, each with one instrument and a four-note chord repeated every beat at
  120 bpm, so voices are always sounding. Instrument kinds: `builtin` (Pocket), `inproc` (test
  synth in the process), `helper` (test synth in `mdaw-plugin-host`).
- **A plugin that costs something.** The test synth is far cheaper than a real synth, so the
  wake-ups would be all that is measured. `mdaw-test-plugin` gets a fourth parameter,
  `WORK` (id 3, 0 to 2000): microseconds of deterministic extra work per 128 frames (extra sine
  partials that are computed and thrown away, calibrated once at activation). Runs are made at
  `WORK = 0` and at `WORK = 300`, about one Vital voice stack.
- **Pacing.** Not as fast as possible: that would never show a wake-up. A pacing thread asks for
  `SCHED_FIFO`, sleeps to each buffer's start with `clock_nanosleep(TIMER_ABSTIME)` and calls
  `RtCore::fill` through `Backend::Manual`. Whether it got real-time priority is written into the
  result.
- **Matrix.** N in {1, 4, 8, 16}; buffers of 64, 128, 256, 512 frames; the three kinds; two
  `WORK` values. 20 s per cell, the first 2 s thrown away. About 30 minutes in all;
  `--quick` runs N in {1, 8} at 128 and 256 only.
- **Output.** A markdown table per kind: mean, p50, p99, p99.9, max in µs and percent of the
  buffer; late and skipped blocks; the three wait parts. Written to
  `docs/perf/baseline.md` with the machine (CPU model, cores, kernel, governor) and the commit.
  Later milestones write `docs/perf/o2.md`, `o3.md` and so on with the same tool, so the files
  diff.

### O0.4 Manual baseline

`docs/manual-tests/o0.md`: the user runs the client with `--stats`, a project with Vital on 4
and then 8 tracks, 2 minutes of playback each, at the device's default buffer. The printed lines
are pasted into `docs/perf/baseline.md` under "Vital". The same runs are repeated at the end of
O2, O3 and O5.

**Done when:** the table exists, the F12 panel shows live numbers, and the three wait parts add
up to the measured wait within 5%.

## O1: Fixes from review 1 (1 day)

1. **Scan failures** (`scan_cache` and `IsolatedBackend::scan` in `isolated/parent.rs`).
   - `Entry.found` becomes an enum: `Found(Vec<PluginInfo>)`, `Crashed` (the helper started,
     took the file and died or answered with an error), `Unknown` (spawn failed, or the 20 s
     `SCAN` timeout ran out). Only `Found` and `Crashed` are written to the cache. `Unknown`
     files are scanned again at the next start.
   - The cache key gets the helper's build: `VERSION` goes to 2 and the file starts with
     `(VERSION, helper_stamp)`, where `helper_stamp` is the length and change time of the
     `mdaw-plugin-host` binary. A new helper build throws the cache away.
   - `Host::rescan()` exists; it gets a `force` flag that ignores the cache. A "Rescan plugins"
     button goes into the plugin picker (`ui/devices.rs`). The scan runs on a worker thread and
     the picker shows "Scanning…".
   - `docs/protocol.md` gets a paragraph on the cache file (`$XDG_CACHE_HOME/mdaw/clap-scan`),
     what is kept and how to delete it.
2. **Duplicate plugin ids.** `scan` builds one list in the order of `plugin_files()` and fills
   each slot from the cache or from the new scan. The first id in that order wins. Today cached
   results come before new ones, so the winner can change between two starts.
3. **Main loop while idle** (`Session::next_frame_in`, `Plugins::activity`).
   - While any instance runs in a helper, `next_frame_in` returns the time until the next
     keep-aside (`KEEP_EVERY` less `kept_at.elapsed()`), and at most 1 s for the hang check in
     `IsolatedInstance::idle`.
   - The helper's reader thread already has the waker. It also wakes the UI when
     `stuck_since` is older than `HUNG_MICROS`: the reader waits on its pipe with a 500 ms
     timeout (`poll`) instead of blocking for ever, and looks at `stuck_since` each time.
   - Test in `crates/mdaw-client/tests/plugins.rs`: a state loaded into the helper's instance
     is in `Plugins::kept` within 2.5 s while only `next_frame_in` drives the loop.
4. **Commit render off the UI thread** (`start_bounce`, `step_bounce`, `OfflineSource`).
   - For plugins in helpers the bounce moves to the job thread: `OfflineSource::make` uses
     `create_later`, so the instances are made on the worker, and `OfflineRender::step` runs
     there to the end. `Job::Plan` comes back as today. Progress goes through an
     `AtomicU32` (per mille) the dialog reads.
   - Plugins that run inside the process (`MDAW_PLUGINS_IN_PROCESS`, and Windows) must stay on
     the main thread. They keep `step_bounce` and its 12 ms slices.
   - `Plugins::capture` splits into `capture_start` (sends `SaveState`, returns at once) and
     `capture_poll`. The commit dialog shows "Saving plugin settings…" until every device has
     answered or 30 s (`LONG_ANSWER`) have passed. This needs the request queue of O2.5, so
     the two are built together: O1.4's capture half lands with O2.5.
   - `keep_aside` drops the `editable` shortcut: it saves an instance only when
     `instance.changed()` is true, or once every 30 s for editable tracks. The 30 s pass is
     there because Vital does not report preset changes.
5. **Small fixes.**
   - `tick` and `tick_heard_at` in `rt::Shared` are published under a sequence counter
     (`seq` odd while writing; the reader retries when `seq` changed or is odd).
     `Engine::position_heard` reads through it.
   - `Job::CommitFailed` becomes `CommitFailed { track, why }`; `poll_jobs` ignores it unless
     `self.commit` is for that track.
   - The helper closes the inherited memfd right after `mmap` (`child.rs`, the `Create`
     branch). The mapping keeps the memory alive.
   - `Plugins::close_all` also clears `starting` and `ready` and disposes what `ready` held.
   - In `Plugins::device`, a ready instance whose `take_processor()` returns `None` goes to
     `crashed` with "did not start". Today it falls through and starts another one.
6. **Cleanups.**
   - The scan cache is written to `clap-scan.<pid>.part`, then renamed.
   - Entries whose file is gone are dropped when the cache is saved.
   - `BlobWriter::finish` (`mdaw-server/src/blobs.rs`) removes the part file when the rename
     fails, and skips the rename when the blob already exists.
   - The doubled doc comment on `scan_one` and the "one file at a time" wording in
     `protocol.md` are fixed.
   - The timing asserts in the two new tests compare against generous bounds (10 times the
     expected time), or count events instead of measuring time.
7. **Missing tests.**
   - Scan cache: a hit does not start a helper; a changed stamp scans again; `Crashed` is
     kept and `Unknown` is not; the order is that of `plugin_files()`. The helper path is
     injectable, so the test uses a script that fails on demand.
   - Eight helpers created at once from eight threads all answer.
   - A ready instance for a device that is gone, or for another state, is disposed.
   - A `CommitFailed` for a track that is no longer being committed changes nothing.
   - `position_heard` under a writer that never stops returns pairs that belong together.
   - The MIDI rate limiter.
   - An upload over the network that is too long, and one that is too short.

**Hand test `o1.md`:** rescan with a broken `.clap` in the folder; two plugins with the same id;
change a preset in a plugin window and leave the app alone for 5 s, then kill the helper and
restart it; commit a track with Vital and watch that the window keeps drawing.

## O2: Audio thread hygiene (1 day)

1. **Denormals.** A new `crates/mdaw-dsp/src/denormals.rs` with `struct NoDenormals` (a guard
   that sets the mode and puts the old one back on drop):
   - x86-64: read and write MXCSR with `stmxcsr`/`ldmxcsr` in `asm!`, setting FTZ (bit 15) and
     DAZ (bit 6).
   - aarch64: `mrs`/`msr` on FPCR, setting FZ (bit 24).
   - elsewhere: nothing.

   The guard is taken in three places: at the top of `Graph::process`, so the callback, the
   offline render and the benchmarks all get it without each caller remembering; at the top
   of the helper's loop in `serve_audio`; and around the in-process plugin call. Two register
   writes per block cost nothing next to a block of audio.

   Pocket's output must not change in this milestone. It flushes its filter state by hand at
   `1e-20`, far above the denormal range, so the golden hash should hold; `golden.rs` proves
   it. If the hash does change, `Graph::process` does not take the guard in O2, and the mode
   becomes part of the Pocket v2 definition in O4, where the hash is renewed anyway.
2. **Plugin logging** (`HostLogImpl::log` in `clap/handlers.rs`). A plugin may log from any of
   its threads, so the queue has several writers: `crossbeam_queue::ArrayQueue<LogLine>` with
   256 places, where `LogLine { level: u8, len: u8, text: [u8; 254] }`. `log` copies and
   truncates the text and pushes; a full queue counts a dropped line. `ClapInstance::idle`
   drains it to stderr, prefixed with the plugin's name. In the helper the main loop drains
   it. No lock and no allocation on the logging thread.
3. **Allocation guard.**
   - In `ClapProcessor::run`, `alloc_guard::leave()` wraps only the call into the plugin
     (`processor.process(...)`) and `ensure_processing_started`. The event sorting, the
     parameter ring and the copy loops around it are host code and stay guarded.
   - `mdaw-plugin-host` installs `GuardedAlloc` as its global allocator in debug builds, and
     `serve_audio` calls `enter()` before its loop. A violation prints once with the plugin's
     name; it does not abort.
   - `events_out` becomes a fixed buffer (`OUT_EVENTS = 256`) that refuses a push when full,
     through clack's output buffer trait, and counts what it refused. Today the buffer grows
     on the audio thread when a plugin sends many parameter events.
   - `Pending` notes and `ParamChange` are already fixed in size; a test with 10 000 note
     events in one block proves that nothing allocates.
4. **Thread check.** `clap/mod.rs` gets `thread_local! { static AUDIO: Cell<bool> }` and a
   guard `AudioThread::enter()`. `ClapProcessor::process` takes it for the length of the call.
   `is_audio_thread` returns the flag; `is_main_thread` stays as it is. A plugin's own worker
   threads now get "neither", which is the truth, where today they are told "audio thread".
5. **Parameters.**
   - **No round trip.** `IsolatedInstance` gets a writer thread with a channel of frames. Every
     request goes through it, in order. `set_param` sends `Request::SetParam` and returns; the
     helper no longer answers it (`protocol.rs`: the reply is dropped). The writer thread
     merges: of several `SetParam` for one id waiting in the channel, only the last is
     written. A helper that does not read cannot block the UI thread any more; the pipe
     filling up blocks only the writer thread, and `idle()` reports the hang as today.
   - **A plugin that is not processing.** CLAP allows `params.flush` on the main thread only
     while the plugin is not active. So there are three cases, handled in
     `ClapInstance::idle` and `ClapProcessor`:
     1. Processing: the changes ride in with the next block, as today.
     2. Active but not called (bypassed, or asleep after O2.8): `ExternalDevice` gets
        `fn flush(&mut self) {}`. The graph calls it once per block on a device it does not
        process. `ClapProcessor::flush` calls the plugin's audio-thread `flush` when the ring
        is not empty or the plugin asked through `request_flush`; otherwise it only reads one
        atomic. Through a helper, a `FLUSH` event in the block's event ring carries it (O3.5).
     3. Not active (the processor is back on the main thread): `idle` drains the ring and
        calls the main-thread `flush`.
   - **Saving after a knob turn.** `ClapInstance` counts `sent` on each push to the ring; the
     processor stores `applied` after the block that took them. `save_state` first waits until
     `applied == sent`, pumping the main loop, for at most 100 ms; in case 3 it flushes
     itself. Through a helper the order of requests does the rest: `SaveState` is written
     after every `SetParam` before it.
6. **Helper priority** (`real_time()` in `child.rs`).
   - `Settings` gets `rt: Option<(i32, i32)>`: the policy and priority of the DAW's callback
     thread from O0.1. A helper made before the first callback is told later with a new
     `Request::Priority`.
   - The helper's audio thread tries, in this order: `pthread_setschedparam` with the same
     policy and priority, with `SCHED_RESET_ON_FORK`; then rtkit over D-Bus
     (`MakeThreadRealtime`, priority capped at rtkit's `MaxRealtimePriority`), after setting
     `RLIMIT_RTTIME` to 200 ms as rtkit demands; then rtkit's `MakeThreadHighPriority`
     (nice −11); then nothing.
   - The same priority as the callback is right, not a lower one: the callback sleeps in the
     futex while the helper works, so the helper stands in for it.
   - `RLIMIT_RTTIME` is also a watchdog: a plugin that spins for 200 ms at real-time priority
     without sleeping is killed by the kernel, and the DAW sees a crash instead of a frozen
     core. The `HANG` case of the test synth gets a test for it.
   - The D-Bus call is the only new dependency (`zbus`, blocking, Linux only, helper binary
     only). It runs on the helper's main thread for the audio thread's id, before audio
     starts.
   - What was reached (`Fifo(n)`, `Nice(n)`, `Normal`) goes back in `Reply::Created`, into the
     F12 panel, and once to stderr when it is not real-time.
   - Not done: `mlockall` in the helper. A sampler would pin gigabytes. The shared block is
     already locked.
   - **Sleeping cores.** A helper that waits for its next block lets its core fall into a deep
     sleep state, and waking from one takes 50 to 200 µs on its own, whatever the thread's
     priority. While audio runs, the DAW holds `/dev/cpu_dma_latency` open with a value of
     20 µs, which keeps the cores in shallow states. The request is the four bytes of an
     `i32` written once; it holds for as long as the file stays open, and it is made by the
     thread that opens the stream, never by the callback. The file is often writable by root only;
     if it cannot be opened, nothing happens and the F12 panel says so. It is released when
     the stream stops, because it costs power. Kept only if the wake-up part of the wait
     drops in `bench-audio`.
   - Not done: pinning helpers to cores. The kernel places real-time threads well, and a
     fixed pinning is wrong on machines with fast and slow cores.
7. **Short spin.** In `IsolatedProcessor::process`, between the wake and the futex wait: up to
   `SPIN = 30 µs` of `std::hint::spin_loop()`, checking `done` (`rendered` from O3.5 on). This can only help a plugin
   that finishes within the spin, which a heavy synth never does, and it holds a core at
   real-time priority. It stays only if the p99 at `WORK = 0` and at `WORK = 300` both improve
   by at least 5% in `bench-audio`; otherwise the commit is dropped and the result noted in
   `docs/perf/o2.md`. After O5.4 only tracks played live wait at all, so little rides on it.
8. **Process status.** `ClapProcessor::run` keeps the status the plugin returned.
   - `Sleep`: the plugin is asleep.
   - `ContinueIfNotQuiet`: asleep once its output was all zeros for 100 ms.
   - `Tail`: treated as `Continue` until O5 knows the tail.
   - `Continue`: awake.

   An asleep plugin is not called while it has no note events, no parameter changes and
   silent input (an instrument has no input; for an effect both channels are compared with
   zero, which is cheaper than any plugin). An effect's output is then zeros; an instrument
   adds nothing.

   Through a helper the decision must be made in the DAW, or the wake-up is not saved. The
   helper writes the status into the shared block; `IsolatedProcessor` keeps `asleep` and
   skips the round trip under the same rule. `set_param` raises a shared `poke` flag so that
   the next block goes through. A project with eight idle tracks then costs no wake-ups at
   all while nothing plays.

   With the stream block of O3.5 "skipping" means that the DAW does not raise `target`, and
   sends a time mark when it goes on. A device that renders ahead (O5.4) is not skipped this
   way, because its ring would lose its place: its helper is still woken, about 47 times a
   second, sees that the plugin sleeps and the piece has no events, and writes zeros without
   calling the plugin.

**Measure:** `bench-audio` into `docs/perf/o2.md`. Expected: the wake-up part drops to tens of
microseconds with real-time priority; the idle project shows no helper wake-ups.

**Hand test `o2.md`:** the F12 panel shows the helpers' scheduling class; turn a knob in the
generic parameter panel while the helper is stopped with `kill -STOP` (the window must not
freeze); turn a knob on a bypassed plugin, un-bypass, and hear the new value; save right after
a knob turn and reload.

## O3: Parallel plugin dispatch (3 days)

Today `TrackRuntime::process` runs each external device to the end before the next, and
`Graph::process` runs track after track into one shared pair of scratch buffers. With helpers on
other cores this is a queue where there could be a fan-out.

**Build order.** The stream block of O3.5 comes first, behind the unchanged `process`, and is
held against a recording of today's output. Then the interface (O3.1), the bell (O3.2), the
stages (O3.3) and the fades (O3.4). Each step is one commit that passes the tests alone.

### O3.1 The device interface

`ExternalDevice` in `crates/mdaw-dsp/src/external.rs` gets a two-step form next to `process`:

```rust
/// Hands over the events and the input of one block, and returns without waiting.
fn begin(&mut self, left: &[f32], right: &[f32], time: &BlockTime) {}
/// Whether the block begun is finished. Never waits.
fn ready(&mut self) -> bool { true }
/// Writes the result over the input. `Missed`: there is none for this block.
fn end(&mut self, left: &mut [f32], right: &mut [f32], time: &BlockTime) -> Outcome;
```

- `IsolatedProcessor`: `begin` hands over events and input and wakes the helper, `ready`
  asks whether the samples are there, `end` copies them out. O3.5 gives the exact form.
- `ClapProcessor` (in the process) and the mock devices: `begin` does nothing, `ready` is
  true, `end` does the work. They run one after another on the callback's thread as today.
- `process` stays as `begin` + wait + `end`, for the helper itself and for simple callers.
- `Outcome` is `Done`, `Silent` (nothing was added; the graph can skip the mix) or `Missed`.

### O3.2 One bell for all helpers

The callback has to sleep until *any* helper is done. Each helper has its own word that says
how far it is (`rendered`), and a futex waits on one word. So `IsolatedBackend` creates one
more small shared mapping, the **bell**: a single `AtomicU32`. Every helper gets it with
`Create`. A helper that finishes samples the DAW is waiting for adds one to the bell and wakes
it; a helper that is in front of the DAW does not (O3.5).

`PluginSource` gets `fn park(&mut self) -> Option<Arc<dyn Park>>`, and the graph keeps what it
returns:

```rust
pub trait Park: Send + Sync {
    fn token(&self) -> u32;                       // the bell's value now
    fn park(&self, token: u32, until: Instant);   // sleep while the bell still has that value
}
```

The token is read before the devices are polled, so a helper that finishes between the poll and
the sleep changes the bell and the sleep returns at once. No wake-up can be lost. The same
shape maps onto `WaitOnAddress` when helpers come to Windows.

### O3.3 Stages in `Graph::process`

Each `TrackRuntime` gets its own pair of buffers (`MAX_BLOCK` frames each, allocated when the
graph is built) and a cursor: the index of the external device in flight, if any.

1. **Start.** For every track: stems and built-in devices render into the track's buffer.
   Built-ins are cheap and deterministic and stay on the callback's thread. Then the track's
   first external device that is not bypassed gets its events and `begin`.
2. **Collect.** Loop:
   - read the bell's token;
   - for every track with a device in flight that is `ready`: `end` it, then give the next
     external device of the chain its events and `begin`, or mark the track done;
   - if no device is in flight, leave;
   - if the deadline has passed, `end` every device in flight as `Missed` and leave;
   - else `park` until the deadline.
3. **Mix.** Tracks are summed into the output in track order, with their gains.

The order of the sum is fixed, so the output does not depend on which helper was first. Offline
there is no deadline and `park` waits up to `OFFLINE_WAIT`.

All first stages get the full deadline at the same time, which is the fairness the outline asked
for. Later stages of a chain share what is left, and that cannot be avoided.

With N instruments on N cores the wait becomes the slowest plugin plus one wake-up. With fewer
cores than helpers the kernel queues them; the stats show it.

### O3.4 Late blocks without clicks

A late block has no samples, so there is nothing to fade. What the graph does have is the end of
the block before. The fix is the trick used for lost packets in voice calls.

Each external device slot in the graph gets a `Declick` (in a new
`crates/mdaw-dsp/src/declick.rs`), which works on the device's **contribution**: its output
minus its input. For an instrument that is its sound; for an effect it is the difference between
wet and dry. Each block, before `end`, the first and last 64 samples of the input are saved;
after `end`, the last 64 samples of the contribution are kept.

- **Missed block:** the buffer still holds the input (the dry signal). The kept 64 samples of
  contribution are added, mirrored in time and faded to zero. The first sample continues where
  the last block ended, so there is no step. An instrument fades out; an effect crossfades to
  dry.
- **First good block after a miss:** the contribution fades in over 64 samples.
- Cost per device and block: copying 192 samples per channel.

What `IsolatedProcessor` does with a late helper also changes:

- The result that came too late for block N is **kept and used for block N+1**. The helper
  is asked for nothing new in N+1; the notes of N+1 go out with N+2, at its first sample.
  From the plugin's side its audio is continuous. From the listener's side there is a gap of
  one block, faded on both edges, and then the sound goes on unbroken. Today the late block
  is thrown away and the next begins with a jump.
- In the stream block of O3.5 this is one time mark: "from this stream sample on, the
  timeline is `n` samples further". Blocks of different lengths need no special case,
  because the DAW takes what is there and asks for the rest.
- A helper that has still not delivered when the next block starts counts as `skipped` and
  gives `Missed` again.

### O3.5 The shared block becomes a stream

Today the shared block (`isolated/shm.rs`) holds one block of audio, its events, and two
counters, `request` and `done`: "here is block 17", "block 17 is done". The two sides take
turns. That form ties the helper to the callback's rhythm. It cannot express a helper that runs
ahead (O5.4), it cuts a device buffer of 1024 frames into two round trips, and late results
need special cases. The block is rebuilt once, here, as a **stream**: both sides count samples.
Nobody takes turns; each word and each ring has one writer.

**Layout**

```rust
pub const RING: usize = 16_384;   // frames, a power of two; 341 ms at 48 kHz
pub const PIECE: u32 = 1024;      // the most the helper renders in one call to the plugin

#[repr(C, align(64))]
pub struct DawSide {              // written by the DAW only
    pub target: AtomicU32,        // render up to this stream sample; the helper sleeps on it
    pub needed: AtomicU32,        // the DAW's current block reads up to here
    pub hint: AtomicU32,          // frames in the DAW's last block
    pub epoch: AtomicU32,         // raised to cancel; see below
    pub skip: AtomicU32,          // on cancel: the stream goes on from this sample
    pub heard: AtomicU32,         // on cancel: what the DAW had read up to
    pub ahead: AtomicU32,         // how far in front the helper is asked to stay (O5.4)
    pub quit: AtomicU32,
    pub t_request: AtomicU64,     // O0.1: when a block was asked for that the DAW waits on
}

#[repr(C, align(64))]
pub struct HelperSide {           // written by the helper only
    pub rendered: AtomicU32,      // output is valid up to this stream sample
    pub state: AtomicU32,         // AWAKE, ASLEEP (waiting on `target`), RESTARTING
    pub plugin: AtomicU32,        // the plugin's process status (O2.8)
    pub t_start: AtomicU64, pub t_done: AtomicU64,
}

#[repr(C)]
pub struct Event { pub kind: u32, pub at: u32, pub pitch: u32, pub vel: u32, pub key: u64 }
// kind: NOTE_ON, NOTE_OFF, NOTE_CHOKE, FLUSH. `at` is a stream sample, where today's
// `offset` counts from the block's start.

#[repr(C)]
pub struct TimeMark {             // "stream sample `at` is this place on the timeline"
    pub at: u32, pub flags: u32,  // PLAYING, CUT
    pub tick: f64, pub seconds: f64, pub bpm: f64, pub ticks_per_frame: f64,
}

#[repr(C)]
pub struct Block {
    pub daw: DawSide,
    pub helper: HelperSide,
    pub events: Ring<Event, 1024>,     // DAW writes, helper reads
    pub marks: Ring<TimeMark, 64>,     // DAW writes, helper reads
    pub input: [AudioRing; 2],         // DAW writes [rendered, target), effects only
    pub output: [AudioRing; 2],        // helper writes, valid in [consumed, rendered)
}
```

- `Ring<T, N>` is a plain single-writer, single-reader queue: `head` and `tail` as
  `AtomicU32` on separate cache lines, slots in between.
- `AudioRing` is `[f32; RING]` with no indexes of its own. Stream sample `s` lives at
  `s & (RING - 1)` in both rings, so input sample `s` and output sample `s` are the same
  moment.
- All stream counters are `u32` and wrap (after 24 hours at 48 kHz). Every comparison is a
  wrapping difference. The tests start the counters just below the wrap.
- The one rule that keeps the rings whole is the DAW's: `target - consumed <= RING - PIECE`,
  where `consumed` is the DAW's own read position.
- Size: 256 KiB per helper, locked in memory as today, and written once by the creating
  thread so that no page is mapped by a first access on the audio thread. The per-track
  buffers of O3.3 get the same treatment.

**The helper's loop** (`serve_audio` in `child.rs`)

```
loop:
    if quit: leave
    if epoch changed: cancel (below); continue
    todo = target - rendered
    if todo == 0:
        state = ASLEEP
        if target is unchanged: futex wait on target, at most 100 ms
        state = AWAKE; continue
    lead = rendered - needed                  # below zero: the DAW is waiting for us
    piece = if lead < 0 { -lead } else { max(lead, hint) }
    piece = min(piece, todo, PIECE, distance to the next mark with CUT)
    send the plugin the events with `at` in [rendered, rendered + piece)
    render `piece` frames: input ring to output ring
    rendered += piece
    if lead < 0 and rendered has reached needed: ring the bell (O3.2)
```

- **Piece size.** While the DAW waits, the helper renders exactly what it waits for. Once in
  front, a piece is never longer than the cushion already built (or one of the DAW's blocks,
  while the cushion is smaller than that). A plugin that can keep up with real time at all
  therefore finishes each piece before its cushion is used up, and the pieces double as the
  cushion grows. With `ahead = 0` the cushion stays zero and each piece is one of the DAW's
  blocks: the round trip of today.
- **Sleeping without a lost wake-up.** The helper stores `ASLEEP` and then reads `target`
  again; the DAW stores `target` and then reads `state`, and makes the futex call only if it
  says `ASLEEP`. Both use `SeqCst`. A helper that is busy sees the new `target` by itself
  and costs the DAW no system call.
- **The bell** is rung only when the DAW was waiting. A helper that is ahead never rings.
- **Time.** The DAW sends a mark when the stream starts, and whenever the timeline is not the
  continuation of the last mark: start, stop, loop wrap, jump, tempo change, a late block
  (O3.4). The helper computes each piece's transport from the last mark and
  `ticks_per_frame`, and ends a piece at a mark that has `CUT`.
- **Cancelling.** The DAW writes `skip` and `heard`, raises `epoch`, and counts its own
  `consumed` forward to `skip`. The helper drops the events and marks before `skip`, ends
  the notes it has sounding (O5.4 says how), and sets `rendered = skip` without calling the
  plugin for the samples between. It is always followed by new work, so the helper is woken
  by the next `target`.

**The DAW's side** (`IsolatedProcessor` in `parent.rs`)

- State: `consumed`, `ahead` (0 in O3), the `sounding` keys as today, and `has_input`, which
  the helper reports in `Reply::Created` from the plugin's audio ports.
- `note_on`, `note_off`, `release_where` push straight into `events` at
  `target + offset`. The `pending` array goes; the ring keeps the same reserve for note ends
  (a quarter of it is never used by note starts).
- `begin(left, right, time)`: write the input at `[target, target + n)` if `has_input`; send
  a mark if needed; store `needed = consumed + n` and `hint = n`; store
  `target = consumed + n + ahead`; wake if `ASLEEP` and either this block's samples are
  missing or a full `PIECE` waits to be rendered. `t_request` is written only when the
  samples of this block are not there yet, so the wait parts of O0.1 count real waits.
- `ready()`: `rendered - consumed >= n`.
- `end(left, right)`: copy `n` frames from the output ring, replacing the buffer if
  `has_input` and adding to it if not, as the trait's contract says; `consumed += n`.
  If the frames are not there: `Missed`.
- A helper that died: `dead` is checked first, as today, and the ring is not read.

**What falls out**

- One wake-up per callback at any device buffer size. `MAX_BLOCK` goes from 512 to 2048, so
  a callback of up to 2048 frames is one block for the graph; the plugin itself is called
  with at most `PIECE` frames and is activated with that maximum.
- An instrument's input is never written, and its output is added straight from the ring.
- Offline renders use the same stream with no deadline.
- The same stream fits a plugin that runs on a thread inside the DAW. That is how Windows
  gets parallel plugins later, without helpers; nothing is built for it here.

The helper protocol's version (`protocol.rs`) goes up with the block. A DAW and a helper of
different builds refuse each other as they do today.

### O3.6 Tests and measurement

- A mock `ExternalDevice` that becomes ready after a set time: with two tracks the wait is the
  longer time, not the sum.
- The stream gives the samples the block form gave, bit for bit: the test synth in a helper,
  rendered with DAW blocks of 1, 64, 128, 500 and 2048 frames, against a recording made
  before the change (`crates/mdaw-plugin/tests/isolated.rs`).
- The same with the counters started 1000 samples below the wrap of `u32`.
- `Ring` and the sleep protocol: two threads, a million items, random pauses; nothing lost,
  nothing twice, no thread left asleep with work waiting. The same two protocols, and the
  cancel, are also checked with `loom` (a dev-dependency), which tries every order of the
  two sides' steps. For that the block's atomics come through one type alias and the futex
  calls through two functions, so the test build can swap them.
- Piece sizes: a helper that needs 90% of real time builds its cushion without one missed
  block.
- Events keep their sample: a note placed at each offset of a block comes out at that
  offset, also when the helper renders the block in several pieces.
- A helper made late with `MISBEHAVE = SLOW`: no sample-to-sample step larger than the signal
  itself makes (a discontinuity test on a held sine), `late` counts it, and the notes after
  the gap still end.
- Cancel: after it no sample from before `skip` is read, and no note is left sounding.
- A helper killed at any point of the loop leaves the DAW with `Missed` blocks and no hang.
- `bench-audio` into `docs/perf/o3.md`, with 1, 4 and 8 helpers at 128 and 256 frames and
  both `WORK` values. Target: the wait with 8 helpers is within 1.3 times the wait with one,
  and the round trip of one helper is not slower than the block form was.

**Hand test `o3.md`:** Vital on 8 tracks at the device's buffer with `--stats`; then the same
while a CPU hog runs (`stress-ng --cpu`), listening for clicks.

## O4: Smoothing, declicking and Pocket v2 (2.5 days)

### O4.1 Gain ramps

A new `crates/mdaw-dsp/src/ramp.rs`:

```rust
pub struct Ramp { now: f32, target: f32, step: f32, left: u32 }
```

`set(target, frames)` aims at a new value from wherever the ramp is; `apply(buf)` multiplies a
block, per sample while `left > 0`, and with the old plain loop once it has arrived. The length
is 10 ms (`sample_rate / 100` frames).

- **Track gain, pan, mute:** `TrackRuntime` holds two ramps (left, right) with
  `compiled.gain` as target. `Graph::adopt_from` copies the ramps from the old track and then
  sets the new target. A track that is new starts at its target. Mute is already folded into
  the gain by `pan_gains`, so it fades with it.
- **Master gain:** one ramp in `Graph`, carried the same way.
- **Listener's volume:** one ramp in `RtCore`, with `shared.output_gain` as target, read once
  per callback.

An offline render never changes a gain, takes the plain loop and stays bit-identical; the golden
test guards that.

### O4.2 Source switches

**Changed from the outline.** The outline kept the old plugin processor in the graph until a
crossfade was done. That needs old tracks to live on inside the new graph, a way to hand them
back from the audio thread later, and helpers that linger. It also cannot cover the commonest
case, a crashed plugin, where there is no old processor to run. One mechanism covers all cases
instead: the mirrored fade of O3.4, at track level.

Every `TrackRuntime` keeps the last 64 samples it gave to the mix, after its gain. A track is
**cut** when what it plays stops being a continuation of what it played:

| Event | Where it is seen |
|---|---|
| The source changes (devices, stem, silent), a device is added, removed or replaced | `adopt_from` compares the old and new track |
| A plugin instance is renewed for a new state | the device is not adopted |
| The transport jumps (seek, loop wrap, resync) while the track plays a stem | `Graph::jumped()`, called from `RtCore::jump` and the loop wrap in `block()` |
| Playback stops while a stem plays | `RtCore::halt` |

A cut track adds its kept tail, mirrored and faded out over 64 samples, and fades its new sound
in over the same 64 samples. Bypass of an external device uses the device's own `Declick` from
O3.4: bypassing counts as a miss that lasts, so an effect crossfades to dry.

Built-in synths need none of this on jumps: their notes are released and decay through their own
envelopes.

Test: a held sine through each kind of switch, and the discontinuity test of O3.6.

### O4.3 Pocket v2 (`crates/mdaw-dsp/src/pocket.rs`)

All of it keeps the determinism rules: `f32`, `+ - * /`, `libm`, no fused multiply-add, no
reordering of sums, and nothing that
depends on the block size.

1. **Parameter smoothing.** `set_params` stores targets. Volume, cutoff, resonance, the filter
   envelope amount and the oscillator mix have a smoothed value that moves toward the target
   at each control step (`CONTROL_INTERVAL = 16` samples) with a one-pole of 5 ms; the
   coefficient is computed once in `Pocket::new` with `libm::expf`. Volume is also
   interpolated linearly inside the 16 samples, because a step of gain every 16 samples is a
   buzz at 3 kHz. `Pocket::new` starts every smoothed value at its target, so a render from
   the start has no fade-in. The control steps must tick on one sample counter of the synth,
   not per call, so that the block size still does not matter; a test renders with blocks of
   1, 7, 64 and 512.
2. **Filter coefficient.** The filter's `g` is computed every 16 samples with `tanf` and held.
   v2 interpolates it linearly between control steps. Fast filter envelopes lose their
   zipper noise. It rides along because the hash is renewed only once.
3. **Voice stealing.** `pick_voice` prefers: a free voice; the quietest voice in release; the
   oldest held voice. A stolen voice that still sounds is moved to its slot's **ghost**: a
   copy of the voice that goes on for `sample_rate / 500` samples (2 ms) under a linear
   fade-out. The new note starts clean in the slot, with its seeded phase and from level
   zero. Today the new note inherits the old level and jumps in phase and filter state, which
   clicks. Keeping phase and filter state instead (the outline's other option) would make a
   note's sound depend on the note before it, which the seeded phase exists to prevent. If a
   slot is stolen again within 2 ms, the older ghost is dropped.
4. **Bypass.** `TrackRuntime::render` on a bypassed Pocket releases all notes and **keeps
   calling `process`**. The voices ring out through their release and the synth goes quiet;
   it costs nothing once no voice sounds. Today the voices freeze and resume later.
5. **Version.** `DSP_VERSION = 2`. In `compile.rs` the match accepts `dsp_version` 1 and 2 for
   Pocket, so stored v1 devices play as v2; new devices are made with 2
   (`session.rs`, near line 673). `golden.rs` gets the new hash, and one new golden case that
   steals voices and moves the cutoff mid-note, rendered through `Graph::adopt_from` so the
   smoothing is under the hash too.
6. **Protocol and store.** `PROTO_VERSION = 6` (`mdaw-proto/src/lib.rs`). `LiveEvent` gets
   `at_micros: i64` now (O6.2 uses it; senders fill it from this release on, receivers ignore
   it until O6).

   The store's format key is `{STORE_VERSION}/{PROTO_VERSION}` (`Store::init` in
   `mdaw-server/src/store.rs`), so it changes too, and today a server that finds another key
   refuses to start and asks for the data directory to be moved away. That would cost every
   existing project its history for a change that touches nothing stored: `LiveEvent` only
   travels in live packets, and a device's `dsp_version` is a number the model already holds.
   So `init` gets a short list of formats that can be opened as they are (`1/5`); it accepts
   such a store and rewrites the key to `1/6`. A test opens a store written by the v0.1.0
   code. If the check shows that a stored type did change after all, the list stays empty
   and the release notes say that projects start anew.

### O4.4 Pocket's speed

What Pocket computes can only change together with its version, so everything that makes it
cheaper and changes a bit of output has to go in now. `bench-audio`'s `builtin` rows say how
far to go.

1. **Sine.** The sine wave calls `libm::sinf` for every sample of every oscillator; it is the
   most expensive line in the voice loop. v2 folds the phase to `x` in −1..1 (a quarter wave
   each way) and uses an odd polynomial of degree 9 for `sin(π/2 · x)`:

   ```
   x2 = x * x
   y  = x * (1.5707964 + x2 * (-0.6459638 + x2 * (0.07968992
            + x2 * (-0.0046741436 + x2 * 0.0001516717))))
   ```

   Evaluated in `f32` in this order, its largest error is 1.9e-7 (−134 dB), and it gives
   exactly 1 at the peak. A test holds it under −120 dB against `libm::sinf`. It is plain
   arithmetic, so it is as deterministic as the rest, and the fold is two compares, which
   become selects in step 3.
2. **Pieces of 16.** The voice loop asks `counter % CONTROL_INTERVAL == 0` and matches on the
   waveform for every sample. The loop is cut at the control steps instead: the control work
   runs once, then a tight loop of up to 16 samples per waveform pair, picked outside it.
   This changes no bit. It is done first, under the v1 hash, which proves it.
3. **Voices side by side**, only if 16 Pocket tracks cost more than 10% of a 128-frame buffer
   in O0. The voices are independent, so four can be computed in the four lanes of one SSE2
   or NEON register. Each lane does the same single-precision operations in the same order
   as the scalar code, so the output is the same bit for bit; what the determinism rules
   really forbid is fused multiply-add and reordered sums, and neither happens here. The
   voice state becomes arrays of four, the envelope's stage switch becomes a select, and the
   control-rate `libm` calls stay scalar. The golden test runs against both forms.

### O4.5 Stems

The fade-out over the last 10 ms is baked in when the stem is decoded (`Job::Stem` in
`session.rs`), not computed while playing. It costs the audio thread nothing. The stored FLAC is
not touched.

### O4.6 Output

`sanitize()` in `rt.rs` clamps hard at ±1 today. The new curve is the identity up to
`T = 0.75` and bends above it:

```
u = (|x| - T) / (1 - T)
y = sign(x) * (T + (1 - T) * u / (1 + u))        for |x| > T
```

It joins the straight part with slope 1, never reaches 1, and needs no `libm` call. Samples
below −2.5 dBFS pass unchanged, so a mix that does not clip sounds exactly as before. It is a
safety net, not an effect, and it is not oversampled. It sits after the listener's volume,
only in `RtCore::fill`. Renders, stems and samples never pass through it. The peak meter reads
before the curve, so overs still show, and a new `clipped` counter lights the meter's top
segment.

**Hand test `o4.md`:** drag a fader fast on a held bass note; mute and unmute; toggle bypass on
Pocket and on Vital; swap a plugin while it sounds; kill a helper while it sounds; seek and loop
on a stem track; play 20 fast notes into Pocket with long releases; sweep the cutoff while
holding a chord; push the master into the red.

## O5: Latency, tails and rendering ahead (4.5 days)

### O5.1 CLAP extensions

- `clack-extensions` gets the `latency` and `tail` features. `Exts` in `clap/handlers.rs` gets
  both; the host side implements `HostLatency::changed` and `HostTail::changed` as flags that
  `idle` picks up.
- `Instance` (`mdaw-plugin/src/lib.rs`) gets `fn latency(&self) -> u32` (frames) and
  `fn tail(&self) -> Tail` (`Frames(u32)` or `Endless`). Latency is read after `activate`, on
  the main thread, as CLAP asks. Through a helper both travel in `Reply::Created` and in
  `Reply::Status` when they change.
- `request_restart` sets a flag.
  - **In a helper** the restart stays inside the helper: its main thread asks the audio loop
    for the processor between two blocks, deactivates, activates, reads the latency and
    hands the processor back. Blocks that arrive meanwhile are answered as missed, and
    O3.4 fades them. Then a `Status` reports the new latency. The DAW's graph is not rebuilt
    unless the latency changed.
  - **In the process** the existing way round is used: the instance reports
    `wants_restart()`, `Plugins::idle` moves it out of `running`, the engine forgets the
    device, the retired processor comes back through its channel, `idle` deactivates and
    activates, and the next graph takes it again.
- `PluginSource` gets `fn latency(&mut self, track, device) -> u32`. The compiler asks it when
  it builds a graph, because an adopted device already lives on the audio thread and cannot
  be asked. A changed latency sets `Plugins::dirty`.

### O5.2 Compensation by reading ahead

**Changed from the outline.** The outline delayed every track to the longest latency with delay
lines and shifted the playhead shown. That adds the longest latency to everything, including
what the user hears after pressing a key, and it reaches into the transport, the playhead and
the placement of recorded notes.

A delay line is needed when audio that already exists has to wait. Here no track receives
audio: every track makes its sound from notes on the timeline. So a track with latency `L` can
simply be **fed its notes `L` samples early**. Its sound then leaves the plugin on time. No
other track is delayed, the playhead stays the heard position, and recording is untouched.

- `TrackRuntime` gets `latency: u32`: the sum over its external devices that are not bypassed
  (plus `AHEAD` per device that renders ahead, O5.4). Stems have none.
- `Graph::process(pos, …)` gives each track the position `pos + latency`, mapped through the
  loop. The events come from there, and so does the `BlockTime` its plugins see. Built-in
  devices before a late effect on the same track are fed early too, since they share the
  chain.
- **Loop.** The graph gets the loop range (`Graph::set_loop`). A track with latency reaches
  the loop end `L` samples before the playhead does. So the wrap is handled per track:
  the events of a block come as up to two ranges (before and after the wrap), and at the
  wrap the track releases its timeline notes and chases at the loop start. The whole-graph
  release and chase in `RtCore::block` shrinks to tracks without latency.
- **Starts.**
  - A start that is scheduled ahead (the shared transport always starts
    `START_LEAD_MICROS` ahead; `start_in` in `rt.rs`): tracks with latency start being fed
    `L` samples before the start frame. Exact from the first sample.
  - An immediate start or seek (the private transport): the track is chased at `pos + L`.
    Notes that begin within the first `L` samples sound up to `L` late on that track, once.
    This is what a seek costs in any host; it is not hidden by delaying everything.
- **A latency that changes** while playing counts as a cut of that track (O4.2), followed by a
  chase at the new position.
- **Notes played live** go to the plugin at once and sound `L` late. Nothing can be done
  about that, and nothing else is made late for it.
- **Limit.** A latency above 1 s is treated as 1 s and the device is marked in the Device
  tab.
- O5.4 builds on this: the position a track has been fed up to becomes a field of its own
  (`fed`), and a depth is added to the latency.

Delay lines come back only when a later slice adds sends, buses or audio input.

### O5.3 Offline renders (`crates/mdaw-dsp/src/render.rs`)

- The same read-ahead gives sample-exact stems with no extra code. The render starts at
  `start - max(pre_roll, longest latency)`, and positions before zero are allowed, so a
  range that begins at the start of the timeline is right as well.
- `TAIL_SECONDS` goes. After the range the render releases the timeline's notes and goes on
  until the output has stayed below −90 dBFS for 100 ms, for at least the longest reported
  tail and at most 10 s (`Endless` counts as 10 s). The silence check is deterministic, so
  built-in renders stay bit-identical; their length changes, and tests that expected one
  second are updated.

### O5.4 Rendering ahead

After O3 every plugin still has to be woken, compute and answer inside 1.6 ms, every 2.67 ms.
That is a hard real-time job done across processes, and it is only needed for sound that
answers to a hand on a key. Every other track plays notes that are known in advance. Its plugins
can run **ahead of the playhead** and leave their audio in the ring of O3.5, where the callback
finds it without waiting. The deadline is gone for those tracks; what limits the project is then
how much the cores can compute, not how fast eight processes can be woken. This is the largest
single gain in the slice.

**How it works**

The stream of O3.5 is a small timeline of its own. The DAW reads it in real time, `n` samples
per callback. Rendering ahead means only this: the DAW writes notes further into the stream's
future than it reads, and raises `target` to match. Nothing in the helper changes.

- `AHEAD = 4096` frames at 48 kHz (85 ms), scaled with the sample rate. `bench-audio` runs
  with 1024, 2048 and 4096 under load; the smallest depth with no missed block and no
  real-time priority is taken, because everything listed under "What it costs" grows with
  it.
- Each `TrackRuntime` gets `fed: i64`, the timeline position its devices have been given
  notes up to. O5.2 feeds a track at `pos + latency`. Now the goal is
  `pos + n + latency + depth`, with `depth` 0 or `AHEAD`, and each callback feeds from `fed`
  to the goal. `IsolatedProcessor::begin` gets `ahead = depth` and sets
  `target = consumed + n + ahead`.
- **Catching up is rationed.** A callback feeds a track at most three blocks of `n` frames,
  one after another through the track's buffers. Built-in devices in front of an effect are
  rendered by the callback itself, and without the ration a start would make it render
  4096 frames at once. At 128 frames the full depth is reached after 16 callbacks, or
  43 ms.
- **Wake-ups.** A helper that is asleep and in front is woken only when a full `PIECE` is
  waiting. At 128 frames that is one wake-up in eight callbacks, and the plugin runs over
  warm caches. In the steady state the callback's cost per device is two atomic stores and a
  copy from the ring.

**Starts, stops and jumps**

- **Immediate start** (private transport): nothing is rendered, the depth is 0, and the
  first callback waits as for a direct track (O3.3). From then on the helper renders faster
  than real time until it is `AHEAD` in front. The sound has no break, because stream and
  timeline stay one continuous mapping; only `target` runs away from `consumed`.
- **Scheduled start** (shared transport, 150 ms ahead by `START_LEAD_MICROS`): the track is
  fed while the lead runs down. The notes go to the stream samples that will be read when
  the start frame comes, and the helper has filled its ring before the first sample is due.
  A test keeps `AHEAD` under the lead at every supported sample rate. If the message
  arrives with less lead left than `AHEAD`, the cushion starts smaller and grows after the
  start.
- **Loop wraps** are known in advance: the feed splits at the wrap, sends a mark with `CUT`,
  releases the timeline's notes and chases at the loop start, all at their stream samples.
  Nothing is cancelled.
- **Jumps of the shared transport** are scheduled with the same lead and are handled like a
  loop wrap when they arrive in time. One that arrives after its place was already fed is a
  cancel.
- **Stop, local seek, resync jump:** what was rendered ahead must not be heard. The track is
  **cancelled** (below) and faded as a cut (O4.2).
- **Transport stopped:** there is no timeline to read ahead on. Every device runs with
  depth 0, and the sleeping rule of O2.8 applies in full. A start that is scheduled counts
  as playing from the moment it is known.

**Cancelling**

The DAW raises `epoch` (O3.5). The plugin has already played up to 85 ms that nobody heard,
and its notes must end in a way that fits what was heard:

- The helper keeps, for every sounding note, the stream sample it started at.
- A note that started after `heard` was never heard. It gets a CLAP **choke** event, which
  silences a voice at once without a release, and then a note end for plugins that ignore
  chokes.
- A note that started before `heard` gets a normal note end and rings out.
- The DAW's `IsolatedProcessor` forgets its `sounding` keys, since the helper has ended them
  all.
- The track sets `fed` back to the playhead and feeds again from there. The first block
  after a cancel waits like a direct track; it can be `Missed` if the helper was in the
  middle of a long piece, and is faded like any missed block.

**Edits while playing**

Today `Graph::adopt_from` releases all of a track's timeline notes whenever its note list
changed at all. For a track that renders ahead that would mean a cancel on every edit by
anyone, heard as a tick.

- `CompiledTrack` gets `changed_from: i64`: the first timeline sample at which the new note
  list differs from the one compiled before it, or `i64::MAX`. The `Compiler` has both lists
  when it rebuilds a track and finds it with one pass, off the audio thread. Graphs reach
  the audio thread in order through the command ring, so "the one before" is the one it
  holds.
- In `adopt_from`: if `changed_from >= fed`, nothing that was fed is touched, and the track
  only takes the new list. Otherwise the track is cancelled (depth above 0) or its timeline
  notes are released (depth 0), as today.
- A tempo change still releases and cancels everything.
- This also helps direct tracks: an edit further along the song no longer cuts the notes
  that are sounding.

**Direct tracks**

A track's depth is 0 while any of these holds:

| Reason | Known where |
|---|---|
| The local player's own track (played live, recorded into) | the session's live track; into `CompiledTrack.direct` |
| One of its plugin windows is open, or its Device tab is shown | `Session::plugin_editor_open`, the UI's selection; into `CompiledTrack.direct` |
| A live note arrived for it in the last 5 s (local or remote) | the audio thread: `TrackRuntime.live_until` |
| The transport is stopped | the audio thread |

- A live note for a track that is ahead makes it direct **on the audio thread**, at once,
  without waiting for a new graph: cancel, then the note. Remote notes come with a playout
  delay (O6.2), which covers the first block's wait.
- Going direct is a cut. Going back ahead is not: `target` is simply raised again, with the
  ration above.
- The first note a remote player plays on a track costs that track a cut. The 5 s keep it
  from happening again within a phrase.

**Who goes first when cores are short**

A helper that renders ahead has 85 ms of slack. The callback and the direct helpers have none.
With more busy helpers than cores, a direct helper must not wait behind one that is only
filling its ring: a piece of 1024 frames of a heavy plugin can take longer than a whole
buffer.

- The DAW writes each device's depth into `ahead` in the shared block.
- The helper's audio thread runs at the callback's priority `P` while `ahead` is 0 and at
  `P - 1` while it is not. It changes its own priority with one `sched_setparam` call when
  the word changes, which is when the track changes between direct and ahead. The kernel
  then moves a woken direct helper onto a core that runs an ahead one, at once.
- Linux lets a thread lower its real-time priority freely and raise it up to its `rtprio`
  limit. A helper that got its priority from rtkit has no such limit and could not come
  back up; it stays at `P`.
- Kept only if it shows: `bench-audio` gets `--cores N` (it pins itself and its helpers to
  `N` cores with `sched_setaffinity`), and the run is 8 helpers at `WORK = 2000` on 4 cores
  with one direct track. The late blocks of the direct track are the number.

**What is counted**

`PluginMeter` (O0.1) gets `cushion_min`, the fewest frames a device was in front at any
callback since the last snapshot, and `cancels`. The F12 panel shows both per device, and
which devices are direct. A cushion that keeps reaching 0 says the machine cannot keep up,
which is a different thing from a late block on a direct track.

**What it costs**

- A parameter changed on a track that is not direct is heard up to 85 ms later. That can
  only be another user's change, since the local user's track is direct.
- Bypass and device changes on such a track are cuts, as they are anyway (O4.2).
- Gain, pan and mute sit after the ring and act at once.
- A cancel throws away up to 85 ms of rendering.
- A plugin that ignores choke events can let the release of an unheard note through after a
  stop or seek. The hand test checks Vital.

**Other points**

- **Running short.** If the ring holds fewer than `n` frames, the block is `Missed` and
  O3.4 fades it. The DAW moves `consumed` on by `n` all the same, so the late samples are
  skipped when they arrive and the track stays in place on the timeline.
- **Chains.** Each device that renders ahead adds `AHEAD` to the track's read-ahead. The
  callback moves what one stage has rendered into the next stage's input ring, as far as
  the ration allows. Three stages read 256 ms ahead, inside `RING`.
- **Latency** (O5.2) and depth add up in `fed`. The clamp of 1 s on reported latency keeps
  the sum inside what the lead and the ring can hold.
- **Not ahead:** tracks made only of built-in devices or stems, which are cheap and stay on
  the callback, and plugins that run inside the process.
- **Offline renders** use the stream with no deadline and pieces of `PIECE` frames, which
  makes a commit faster than today's 512-frame round trips.
- **Setting.** `render_ahead` in `settings.toml`, on by default. It is there to compare and
  as a way out, not as a choice the user is expected to make.
- **Measured** into `docs/perf/o5.md`: 8 helpers at 128 frames with `WORK` 300 and 2000,
  with and without real-time priority, alone and under `stress-ng`. Expected: the wait is
  that of the one direct track, and late blocks are 0 while the plugins' total work fits
  the cores.

### O5.5 Tests

- The test synth gets two more parameters: `LATENCY` (0 to 4800 samples; an internal delay
  line it reports through the extension, asking for a restart when it changes) and `TAIL`
  (a decaying echo with a reported length).
- A stem of the test synth with `LATENCY = 1000` lines up to the sample with the same notes
  rendered at `LATENCY = 0`.
- Live graph: two tracks with the same notes, one with latency, cancel when one is inverted
  (after the first `L` samples of an immediate start, and from the first sample of a
  scheduled start). The same across a loop wrap.
- A tail longer than a second is not cut; a silent tail ends the render early.
- A helper that renders ahead gives the same samples as a direct one: from a scheduled start,
  from an immediate start, across a loop wrap and after a seek.
- After a seek or a stop nothing stale is heard: a mock device writes the timeline position
  it was given as its sample values.
- Cancel: a note that began in the unheard part is choked, one that began before is ended,
  and none is left sounding. The test synth counts the chokes it gets.
- A track that goes direct in the middle of a note fades, hangs no note, and plays the next
  live note within one buffer.
- An edit beyond `fed` changes nothing that sounds; an edit before it cancels once.
  `changed_from` is right for a note added, removed, moved and resized.
- A start with a built-in synth in front of an effect that renders ahead: no callback
  renders more than `3 * n` frames.
- `request_restart` in a helper: the latency reported changes and the sound comes back.

**Hand test `o5.md`:** a look-ahead limiter or linear-phase EQ after Pocket against a click
track; commit it and compare the stem with the live sound; a reverb tail at the end of a stem;
8 tracks of Vital with rendering ahead on and off, alone and under load; playing one track
live while seven render ahead; a knob turned in a plugin window is heard at once.

## O6: Timing (2.5 days)

### O6.1 Live notes with time stamps

Today `RtCore::fill` applies every waiting live note at frame 0. A note lands anywhere in the
buffer before, so its delay varies by one buffer: 2.7 ms at 128 frames, 21 ms at 1024. A steady
drum roll comes out uneven.

- `LiveNote::On` and `Off` get `heard_at: i64`: the time on `clock::now_micros()` at which the
  note should be heard, or 0 for "as soon as possible".
- The callback publishes `live_delay_micros`: one buffer's length plus the output lead
  (`lead_micros`), smoothed, and rounded up to whole frames. `LiveInput::note_on` takes the
  time the note was played and sets `heard_at = played_at + live_delay`.
- In `fill`, a note's frame is `(heard_at - play_micros)` in frames. Below zero: frame 0.
  Beyond this buffer: it waits in a fixed array of 64 in `RtCore` for the next one.
- `Graph::note_on` and `note_off` take the frame. External devices already accept an offset.
  For built-ins, each track gets a small fixed queue of live events that
  `TrackRuntime::process` merges into its split loop with the timeline's events, so a live
  note also starts on its exact sample.
- **Where the time comes from.**
  - MIDI: `mdaw-engine/src/midi.rs` stops dropping midir's stamp. The stamp counts from a start that
    depends on the system, so it is mapped onto the engine's clock with a running offset:
    the smallest `now - stamp` of the last 64 messages. That keeps the device's own spacing
    and removes the jitter of the thread that delivers it. A stamp that is zero or goes
    backwards falls back to `now`.
  - Computer keys: stamped in `Live::press`. The 16 ms grid of the UI frame stays; egui
    gives no event times. Said in the hand test, not fixed here.
- Recording uses the played time, not the time the frame handled it (`Played.micros` in
  `live.rs`).
- A live note for a track that renders ahead first makes the track direct (O5.4). The note
  then goes into the stream at `consumed + frame`, like any note of a direct track.

The price: the delay is now always one buffer, where before it was half a buffer on average.
At 128 frames that is 1.3 ms more. Even spacing matters more than that to a player, and the
buffer-size setting (O6.4) is the way to less delay.

### O6.2 Remote live notes

- Senders fill `LiveEvent.at_micros` (in the protocol since O4.3) with the played time on the
  **server's** clock: the local time plus the offset from `ClockSync`.
- `Live::receive` turns it back into local time and keeps, per stream (actor and track), the
  last 64 values of `d = arrival - sent`. The playout delay is
  `min(p95(d) + 1 ms, min(d) + 30 ms)`: enough to cover the jitter seen, and never more than
  30 ms above the fastest packet. The note goes to the engine with
  `heard_at = sent + delay`.
- A note that arrives after its time plays at once.
- The delay changes only while the stream holds no note, so the spacing inside a phrase is
  kept.
- Events a packet repeats (`REPEATED = 2`) carry their own times, so a note recovered from a
  later packet still lands where it belongs if there is time left.
- A note's end is never placed before its start.

Tests in `mdaw-proto/src/midi.rs` and `crates/mdaw-client/tests/live.rs`: packets with a jitter
of 0 to 20 ms give notes whose spacing is off by under 1 ms; a late packet plays at once; lost
packets are made up for.

### O6.3 Gradual clock correction (`steer()` in `rt.rs`)

A sound card that runs 50 ppm off the system clock is 20 ms off after 7 minutes. Today that is
a hard `jump`: every timeline note is released and chased, about every 7 minutes, for ever.

This engine renders from notes, so it can slip without touching the audio: after a block of
`n` frames the position moves on by `n + 1` or `n - 1`. Synth voices run on unbroken; only the
times of coming events move by one sample. Nothing has to fade.

- **Controller.** The drift measured per callback is noisy (the callback's time stamps jitter
  by about a millisecond). It goes through a one-pole of 2 s. The slip rate in ppm is a PI
  controller on that: proportional 100 ppm per millisecond of error, integral with a time
  constant of 30 s (which learns the card's steady offset), limited to ±500 ppm. An
  accumulator adds `rate * n` per block and gives one slip of a sample each time it passes
  one.
- **Stems** are audio and would tick at a slip. A stem track crossfades over 64 samples
  between the read before and after the slip (the outline's "dropped or repeated, faded").
- **Plugins** see their transport position move by one sample; that is far inside what any
  plugin expects. Through a helper it is a time mark (O3.5).
- **Tracks that render ahead** take the slip where they are fed, not where they are read:
  the feed covers `n + 1` or `n - 1` timeline samples for `n` stream samples, with a time
  mark. What is already in the ring is one sample off for 85 ms, which is 21 µs.
- **Jumps** stay for a new anchor (`fresh`, 2 ms, as today) and for an error above 100 ms on
  three callbacks in a row (`MAX_DRIFT_MICROS` goes from 20 000 to 100 000). They are faded
  through `Graph::jumped()` (O4.2).
- A better estimate of the server's clock (`follow(…, false)` in `sync_transport`) no longer
  causes a jump; the controller absorbs it.
- `Timing` gets `slip_ppm`, shown in the F12 panel.

Tests with `Backend::Manual` and a `ManualClock` that runs 100 ppm fast for 20 simulated
minutes: `resyncs` is 0, the drift stays under 2 ms after the first 30 s, and a counting mock
device gets each timeline note exactly once. A step of 5 ms in the clock offset is absorbed
within 60 s without a jump.

### O6.4 Buffer size setting

- The profile gets a settings file (`profile.rs`, `settings.toml` in the profile folder):
  `buffer_frames: Option<u32>` and `render_ahead: bool`.
- `Engine::open` asks for `BufferSize::Fixed(n)`, with `n` brought into the range the device
  reports. If the stream does not open, it opens again with the default and a notice says
  so.
- The size in use is what the callback sees (`buffer_frames`), not what was asked: PipeWire
  decides its quantum for the whole graph. The transport bar's tooltip shows the size, the
  output lead and their sum in milliseconds.
- Changing the setting applies at the next start in O6, and at once from O7.1 on, which
  builds the reopen.

**Hand test `o6.md`:** record a steady roll from a MIDI keyboard at 1024 frames and look at the
recorded notes against the grid; two clients playing together over a network with added jitter
(`tc netem`); a 30-minute shared playback watching `resyncs` and `slip_ppm`; buffer sizes 64,
256 and 1024.

## O7: Devices and stems (2 days)

### O7.1 Reopening the output

Today a stream error is printed and the app plays into nothing until it is restarted.

- The stream's error callback sets `Shared.stream_lost` and calls the waker.
- `Engine::reopen()` runs on the UI side from `Engine::tick`: it drops the stream, looks for
  the device with the same name, else the default device, and opens it. On failure it tries
  again after 0.5, 1, 2 and then every 5 s. A notice says "Audio device lost" and later
  "Audio is back on <name>".
- `RtCore` is built again; the compiled project is set again, and the transport is issued
  again from what the session knows (`sync_transport` with a fresh anchor), so playback picks
  up at the right place.
- **Same rate:** plugin processors are kept. The old core hands them back through the
  garbage path, and the new graph adopts them. Every device that rendered ahead is
  cancelled (O5.4), because the transport starts from a fresh anchor.
- **New rate:** nothing made for the old rate can stay. `Plugins::keep_aside` runs for every
  instance, then `close_all`; `Plugins::sample_rate` and the `Compiler`'s rate change;
  instances start again with their kept states; stems are resampled again (from the cache
  of O7.4 when it has them).
- A changed buffer size alone (PipeWire changes its quantum) needs no reopen: the callback
  sees the new size, and `buffer_frames` and `live_delay_micros` follow.
- The buffer-size setting of O6.4 calls `reopen` when it changes.
- `Backend::Manual` gets `lose()` and a rate that can be changed, so the path is tested
  without hardware: playback goes on at the right tick after a reopen at 44.1 kHz.

### O7.2 Underruns

Three sources, each counted in `Meter.overruns`:

- the stream error cpal reports where the backend has one (ALSA `EPIPE`);
- a gap in the callback's playback stamps: this callback's time is later than the last one's
  plus its frames by more than half a buffer;
- a callback that ran longer than its buffer (O0.1).

Three or more in 10 s give one notice, "Audio is dropping out", with the buffer size and a
pointer to the setting. The F12 panel shows all three counts apart.

### O7.3 Resampling (`flac::resample` in `mdaw-engine`)

Linear interpolation folds everything above half the new rate back into the audible band and
dulls the top. It is replaced by a windowed-sinc resampler from the `rubato` crate:
`SincFixedIn` with `sinc_len = 256`, `f_cutoff = 0.95`, `oversampling_factor = 128`, cubic
interpolation between sinc tables and a `BlackmanHarris2` window.

- The resampler's delay is cut from the start and the end is padded, so that sample `i` of the
  input is sample `i * ratio` of the output. Stems must stay lined up with the timeline.
- It runs on the job thread only (`Job::Stem`, `Job::Decoded`, `make_stem`). A 5-minute
  stereo stem takes well under a second.
- Tests: a 1 kHz sine 48 → 44.1 → 48 kHz keeps a signal-to-noise ratio above 100 dB; a
  20 kHz sine at 48 → 44.1 kHz leaves no alias above −100 dB; an impulse lands within half a
  sample of its place; the length is `round(len * ratio)`.
- 48 kHz to 48 kHz stays the untouched path.

### O7.4 Stem cache on disk

- Location: `$XDG_CACHE_HOME/mdaw/stems/<blob hash>-<rate>.f32`.
- Format: a 32-byte header (magic `MDST`, version, rate, channels, frames), then little-endian
  `f32` samples, interleaved. The stem's gain is not baked in, so a gain change does not
  spoil the file. The fade of O4.5 is baked in, and the version covers that.
- Written to a temp name and renamed. A file with a wrong header or length is deleted and
  made again.
- `Job::Stem` looks here before it decodes FLAC and resamples.
- Limit: 2 GiB by default (in `settings.toml`). When over, the files read longest ago go
  first. Files of the open project are never removed.

### O7.5 Memory

A 15-minute stereo stem is 346 MB as `f32` at 48 kHz. Ten of them do not fit in memory as
`Arc<[f32]>`, and they do not need to.

**Measure first:** the project's resident memory and loading time with 10 stems of 5 minutes,
today. If that is fine for the sizes the project aims at, O7.5 stops here and the cache alone
ships.

Else, the preferred design:

- `Source::Stem` holds `StemAudio`: `Ram(Arc<[f32]>)` or `Mapped(Arc<Mapping>)`, where the
  mapping is the cache file of O7.4 opened with `mmap`.
- Reading mapped memory on the audio thread can fault and wait for the disk. So a **pager
  thread** keeps a window resident: from 1 s behind the playhead to 10 s ahead, plus 10 s
  from the loop start and from the start of the project. It uses `madvise(WILLNEED)` and
  touches the pages, then publishes the ready range per stem in two atomics.
- The audio thread plays a stem only inside its ready range. Outside it, the stem is silent
  and comes in with the fade of O4.2 when the range arrives. That happens only on a seek
  far away, for a few milliseconds.
- Stems under 30 s stay in RAM.
- Windows gets the same through `MapViewOfFile` and `PrefetchVirtualMemory`, behind one small
  module.

The outline's fallback (half-precision samples in RAM) halves the memory and no more, and is
only taken if the pager proves fragile.

**Hand test `o7.md`:** unplug the audio interface while playing and plug it in again; switch
the system's output device; switch the device's rate in the system settings; a project with
long stems, cold and warm start, and a seek to the far end.

## O8: Leftovers (1 day)

1. **Device order in the UI.** The model has `DeviceMove` and the engine obeys it. The Device
   tab (`ui/devices.rs`) gets a drag handle per device row, and "Move up" and "Move down" in
   the row's menu. Moving an instrument behind an effect is refused with the reason shown. A
   move is a source switch (O4.2) and is faded.
2. **A real effect.** The test plugin gets an effect variant (a second plugin id in the same
   bundle: a one-pole low-pass with a `CUTOFF` parameter), so effect paths have automated
   tests: the chain order, bypass with crossfade, latency and tail. A real CLAP effect goes
   into the hand test.
3. **Windows.** Plugin windows and plugins inside the process, by hand, on a real machine.
   What O2 and O3 built for helpers does not run there yet; the hand test says what a later
   slice needs (shared memory, `WaitOnAddress`, MMCSS for the priority).
4. **The m5 and m6 hand tests**, as far as results are still open.

**Hand test `o8.md`** covers 1 to 3.

## Not in this slice

- Helpers on Windows and macOS.
- Sends, buses, audio input and sidechains. With them come real delay lines for compensation.
- Automation of parameters on the timeline.
- VST3.
- SIMD in the built-in DSP. The determinism rules forbid it for Pocket.
- Rendering the built-in devices on several threads, or ahead. They are cheap next to
  plugins. If O0 says otherwise, the stream of O3.5 with a thread behind it is the way.
- One helper for a whole chain or a whole project. It would save wake-ups on a live track
  with a long chain, but one crash would then take several plugins down, and rendering ahead
  already takes the wait away from every other track.
- Parallel plugins on Windows (the same stream with a thread behind it).
- An oversampled limiter on the output.

## Order and effort

| Milestone | Days | Needs |
|---|---|---|
| O0 Baseline | 0.5 | |
| O1 Review fixes | 1 | |
| O2 Audio thread hygiene | 1 | O0 for the priority to copy; O1.4's capture half is built with O2.5 |
| O3 Parallel dispatch, stream block | 3 | O0, O2 |
| O4 Smoothing and Pocket v2 | 2.5 | O3.4 for the fade it reuses; O0 for how far O4.4 goes |
| O5 Latency, tails, rendering ahead | 4.5 | O3.5, O4.2 |
| O6 Timing | 2.5 | O4 for faded jumps; the protocol field from O4.3 |
| O7 Devices and stems | 2 | O4.2; O6.4 switches live once O7.1 is in |
| O8 Leftovers | 1 | O4.2, O5 |

About 18 days. The outline said 10; O3, O5 and O6 grew when the detail showed what they take,
and rendering ahead with its stream block added three more.

After each milestone: `cargo test`, `cargo clippy`, `bench-audio` where the milestone names it,
then `docs/manual-tests/o<N>.md` is written and work pauses for the results.

If time is short, the order of value is: O2 and O3 (the dropouts on the track being played),
O5.2 with O5.4 (the deadline gone for every other track), O4 (the clicks), O6.1 and O6.3 (the
feel when playing), O5.1 and O5.3 (wrong stems with latent plugins), then the rest. O7.5, the
spin of O2.7, the priority step of O5.4 and the third step of O4.4 can be dropped without
leaving a hole.

## Risks

- **Real-time priority is not granted.** Without `rtprio` limits and without rtkit the helpers
  stay at normal priority and O2.6 gains nothing. The F12 panel says so, and the hand test
  names the two fixes (the `realtime` group, or PipeWire's rtkit module). O3 still helps in
  that case, and O5.4 takes the deadline away for every track but the one being played.
- **Reading ahead across loops** (O5.2) is the most intricate change: per-track wrap, release
  and chase. It gets the cancel test across a loop wrap before anything else is built on it.
  If it turns out worse than expected, the fallback is the outline's delay lines, for offline
  renders first.
- **The Pocket v2 bump** changes the store's key. O4.3 lets the server open old stores as
  they are, which is only right while no stored encoding changed between 5 and 6. The test
  with a store from v0.1.0 guards it.
- **Rendering ahead has more states** than a round trip: direct and ahead, the change between
  them, cancelling, catching up. The tests that compare its samples with a direct render are
  written before the feature, and `render_ahead = false` is the way out. What stays
  different by design: a parameter changed on a track that is not direct (by another user,
  say) is heard 85 ms later.
- **Plugins that ignore choke events.** After a stop or a seek such a plugin can let through
  the release of a note that was rendered ahead and never heard. If Vital does this in the
  hand test, the fallback is to keep the cut's fade and drop the helper's output for the
  plugin's reported tail, at most 250 ms.
- **The stream block is lock-free code shared by two processes.** A mistake shows as a rare
  hang or a rare wrong sample. The two-thread tests of O3.6 run under `loom` for the sleep
  and cancel protocols as well as at full speed, and the block has no state that a dead
  helper can leave half-written in a way the DAW would wait on.
- **Late results played one block later** (O3.4) shift a plugin's output by one block for one
  block. If a plugin reads the transport closely (an arpeggiator), it sees a position one
  block behind for that block. Accepted: the other choice is a jump.
- **`clack`'s traits** for the output event buffer (O2.3) and the latency and tail extensions
  (O5.1) are assumed to be as in the pinned version; checked on the first day of each
  milestone.
- **Device reopen** behaves differently per backend (PipeWire, ALSA, WASAPI). The automated
  test covers only the logic; the hand test covers the backends at hand.
- **Page faults** in mapped stems (O7.5). The ready range is the guard; a counter of reads
  outside it shows in the F12 panel.
- **New dependencies:** `crossbeam-queue`, `zbus` (helper, Linux), `rubato`, `toml` if not
  already there, `loom` for tests only. Each is behind one small module.

## Changes from the outline

1. One protocol bump for the whole slice, in O4. The field for remote note times ships with
   it.
2. O0 gets a `WORK` parameter in the test synth, and time stamps in the shared block that
   split the wait into wake-up, plugin and way back. Without them O2 and O3 cannot show what
   they gained.
3. O2: the denormal guard sits in `Graph::process`. Parameters to helpers go through a writer
   thread. `RLIMIT_RTTIME` doubles as a watchdog. The spin is kept only if it measures well.
4. O3: one shared bell lets the callback sleep on all helpers at once. A late block is hidden
   with the mirrored end of the block before, and its late result is played next instead of
   being dropped. The shared block counts samples instead of blocks and holds rings, so one
   form serves the round trip, large device buffers and rendering ahead.
5. O4: source switches use the same mirrored fade, not an old processor kept alive. Voice
   stealing uses a 2 ms ghost voice. The filter coefficient is interpolated. The clipper is
   a rational curve above 0.75.
6. O5: compensation feeds latent tracks early instead of delaying the others. No delay
   lines, no shifted playhead, no change to recording. The pipelined mode became rendering
   ahead: tracks nobody plays live are rendered 85 ms in front of the playhead and the
   callback never waits for them. It is on by default.
7. O6: live notes carry the time they should be heard at. Clock drift is corrected by single
   sample slips under a PI controller, which need no fades for synths.
8. O7: a pager thread over the mapped cache file is the preferred answer for long stems, and
   only if measuring says it is needed.
9. Build settings for release builds (link-time optimization, one code generation unit),
   and all measuring in release builds.
10. Pocket v2 also gets faster: a polynomial sine, a voice loop cut at the control steps, and
    voices in vector lanes if O0 shows the need.
11. Rendering ahead changes three things a user can notice. A stop or a seek chokes notes
    that were rendered but not yet heard. An edit further along the song no longer cuts the
    notes sounding now, on any track. A track goes direct, with one short fade, when someone
    starts to play it.
12. While cores are short, helpers that render ahead run one priority step under the
    callback and the helpers of direct tracks.
13. Effort: about 18 days, up from 10.
