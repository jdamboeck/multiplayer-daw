# Plan: retire the Rust audio engine (phase 7)

Status: planned, 2026-11-15; decisions taken by the project owner the same day.

Since phase 5 the client plays through Tracktion (`--features tracktion`); the Rust engine
(`mdaw-engine`, `mdaw-dsp`, `mdaw-plugin`) is still built, still the default without the
feature, and still what CI tests the client with. This plan removes it. Context:
`docs/migration-checklist.md` ("Replace"), ADR 0002, `docs/tracktion-integration.md`.

## What still depends on it (surveyed at c5ba727)

| Used | By | What it is |
|---|---|---|
| `OldAudio` (`audio_old.rs`, 424 lines), `plugins.rs` (638, the glue to the Rust CLAP host), `--old-engine` | client | the old engine behind the `Audio` seam |
| `Session::engine()/engine_mut()`, `Session::enable_plugins(Host)` | client, tests | old-engine access |
| `mdaw_engine::clock::now_micros` | `audio_tracktion.rs`, `session.rs`, `live.rs` | a monotonic clock for recording |
| `mdaw_engine::midi` (midir input) | `live.rs`, `ui/live.rs` | MIDI keyboard input (the client reads ports itself, point 19) |
| `mdaw_engine::alloc_guard` | `main.rs` | aborts on allocation in the Rust real-time thread; Tracktion has none in Rust |
| `mdaw_dsp::pocket` | `ui/devices.rs`, `session.rs`, `mdaw-tracktion/src/apply.rs` | the old Pocket synth's parameter table; Tracktion plays old Pocket devices through 4osc with it |
| `mdaw_plugin::{Host, TEST_SYNTH}` | `main.rs`, tests | the Rust CLAP host |
| client tests | `ui.rs` (14), `ui_net.rs` (7), `net.rs` (17), `playback.rs` (2), `plugins.rs` (2) | run on the old engine; `ui_tracktion.rs`, `slice.rs`, `engine_scheduling.rs` run on Tracktion |
| CI (`ci.yml`) | Linux **and Windows** | excludes the Tracktion crates: the native build is never run |
| release (`release.yml`) | Linux and Windows | builds the client without Tracktion; ships `mdaw-plugin-host` |
| `mdaw_tracktion::built_scanner_path()` | `audio_tracktion.rs` | the scanner's path is the build folder, compiled in: a packaged client would not find it and would scan in process |

The bridge builds on Linux only (`mdaw-tracktion-sys/build.rs` refuses other targets).

## Decisions (the project owner, 2026-11-15)

1. **Windows is dropped for now.** CI and release build Linux only until the bridge is ported
   (nothing on Windows has been tested with Tracktion). README and release notes say so.
2. **CI builds the native part** (submodules, JUCE and Tracktion, about 10 minutes cold, then
   cached with `target/native`) and runs every client test on Tracktion: CI tests what ships.
3. **`mdaw-plugin` goes now**, the isolation helper included. Per-plugin isolation, when it
   comes, needs a C++ host side anyway; the helper and its shared-memory protocol are recovered
   from git history then (the last commit that has them is named in ADR 0002). ADR 0002 is
   amended accordingly.
4. **Old stores are refused.** `BuiltinKind::Pocket` leaves the model; the built-in instrument
   is the 4osc one (`TwoOsc`). Removing the variant renumbers `TwoOsc` in the wire and store
   formats, so the protocol version (10 -> 11) and the store version (6 -> 7) go up: data
   directories and projects from before are not readable, as with earlier format changes. The
   client's outbox and any project cache on disk are refused the same way (check what they
   carry a version in; add one if not). `POCKET_TO_FOUR_OSC` in `mdaw-tracktion` goes with it.

## Steps (each one a commit that builds and passes its tests)

1. **Packaging the scanner.** Find `mdaw-plugin-scan` next to the client's executable at run
   time, falling back to the compiled-in build path for development; xtask and the release
   copy it there. Without this, the release would ship a client that scans plugins in process.
2. **Drop the old Pocket** (decision 4): the model variant, the 4osc approximation table, the
   tests that use it (`mdaw-model/tests/properties.rs`, `mdaw-sync/tests/replica.rs`,
   `mdaw-tracktion/tests/engine.rs`, client `record.rs`, `session.rs`, `ui/devices.rs`, tests
   `net.rs`, `playback.rs`, `plugins.rs`); protocol 11, store 7. `mdaw-tracktion` then no longer
   depends on `mdaw-dsp`.
3. **Move the small things the client keeps.** `clock` and `midi` from `mdaw-engine` into
   `mdaw-client` (the server does not use them); drop `alloc_guard` and the client's global
   allocator (there is no real-time thread in Rust any more).
4. **Tracktion by default.** The `tracktion` feature goes; `mdaw-tracktion` becomes an ordinary
   dependency of the client. `--old-engine`, `OldAudio` (`audio_old.rs`), `plugins.rs`,
   `Session::engine*`, `Session::enable_plugins(Host)` go. The `Audio` trait stays (it keeps
   the session testable and the engine replaceable). `ui/devices.rs` loses the paths only the
   old engine used (the helper's crash and restart map to the bridge's statuses).
5. **Port the tests.** `ui.rs`, `ui_net.rs`, `playback.rs`, `net.rs` to Tracktion. One engine
   per process: a binary whose tests each need an engine either becomes one test with steps (as
   `ui_tracktion.rs`) or is split into binaries. Network tests that need no sound get an engine
   without plugins and without a device. `plugins.rs` tests the Rust CLAP host and its helper and
   goes; what it checks about the session (a plugin's state reaching the document, restart after
   a failure) moves to the Tracktion tests. Nothing tested today goes untested without a line
   here saying so.
6. **CI and release** (decisions 1 and 2): Linux only; checkout with submodules
   (`git -c url."https://github.com/".insteadOf="git@github.com:" submodule update --init
   --recursive --depth 1`); the packages from `docs/tracktion-integration.md` "Build" plus
   cmake, ninja and xvfb/xprop; cache `target/native`; build the scanner, the test plugins and
   the test synth; run the C bridge test, the Rust tests, and the window tests through
   `run_gui_tests.sh`. Release: the client, the server and `mdaw-plugin-scan`; no
   `mdaw-plugin-host`. `default-members` and the `--exclude` flags go: the whole workspace is
   built and tested.
7. **Remove the crates.** `mdaw-engine`, `mdaw-dsp`, `mdaw-plugin` from the workspace and from
   xtask's build list (`-p mdaw-plugin`); `xtask demo --tracktion` becomes the default (the flag
   goes). `mdaw-test-plugin` stays (the bridge tests and the client tests use the test synth).
8. **Docs.** `architecture.md`, `migration-checklist.md` (rows to "Removed"), `plan.md`, ADR 0002
   (decision 3, and the commit to recover the helper from), README (Linux only; the build needs
   the native part and its packages), `tracktion-integration.md` ("Build" becomes the normal
   build), `protocol.md` (protocol 11, store 7).
9. **Hand test.** `docs/manual-tests/tracktion-client.md` and `plugin-windows.md` on the result,
   with a fresh data directory (old ones are refused).

## Not part of this phase

Failure-policy items 2 (a marker file for a plugin that crashes the app while loading) and 3
(the stalled-audio watchdog's user-facing side) of ADR 0002; time signatures, automation and
audio clips; the bridge on Windows/macOS.

## Risks

- Build time: every client build and every CI run builds JUCE and Tracktion once (about
  10 minutes cold; the build folder is shared and incremental afterwards).
- Tests on Tracktion are slower (engine start, teardown about 1 s) and one engine per process
  constrains how tests are split.
- Users on Windows lose the client until the bridge is ported (decision 1).
- Every existing server data directory and project is unreadable after step 2 (decision 4).
- CI minutes: the native build and the slower Tracktion tests make each run longer; the
  `target/native` cache is what keeps it bearable.
