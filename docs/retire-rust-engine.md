# Plan: retire the Rust audio engine (phase 7)

Status: planned, 2026-11-15; decisions taken by the project owner the same day.

Since phase 5 the client plays through Tracktion (`--features tracktion`); the Rust engine
(`mdaw-engine`, `mdaw-dsp`, `mdaw-plugin`) is still built, still the default without the
feature, and still what CI tests the client with. This plan removes it. Context:
`docs/migration-checklist.md` ("Replace"), ADR 0002, `docs/tracktion-integration.md`.

## What still depends on it (surveyed at c5ba727; checked against a review the same day)

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
2. **Move the small things the client keeps.** `clock` and `midi` from `mdaw-engine` into
   `mdaw-client` (the server does not use them); drop `alloc_guard` and the client's global
   allocator (there is no real-time thread in Rust any more).
3. **Tracktion by default.** The `tracktion` feature goes; `mdaw-tracktion` becomes an ordinary
   dependency of the client. `--old-engine`, `OldAudio` (`audio_old.rs`), `plugins.rs`,
   `Session::engine*`, `Session::enable_plugins(Host)` go; `mdaw-slice` loses its
   `required-features`. The `Audio` trait stays (it keeps the session testable and the engine
   replaceable); the module doc of `audio.rs` ("two engines sit behind it") is rewritten.
   - *Crashed plugins:* `DeviceStatus::Crashed`, `TrackSound::Crashed`,
     `Session::plugin_crashed` and the "crashed, restart" row in `ui/devices.rs` are only ever
     produced by the old helper process (in process, a crash ends the app). They go; failure-
     policy item 2 (a plugin that crashed the app while loading is loaded disabled) will bring
     its own status. `restart_plugin` stays: it retries a plugin that `Failed` to load.
   - *`--test-synth`* (client and `xtask demo`) offered the test synth through the Rust host. It
     goes from the client; `xtask demo --test-synth` instead builds `mdaw-test-plugin` and puts
     a folder with `mdaw-test-synth.clap` on `CLAP_PATH` for the clients.
   - `mdaw_plugin::Waker` goes with `plugins.rs`; the engine and the link use
     `crate::link::Waker` already.

   **Ported in the same commit** (the old tests do not build without `OldAudio`):
   `ui.rs`, `ui_net.rs`, `playback.rs`, `net.rs` to Tracktion. One engine
   per process: a binary whose tests each need an engine either becomes one test with steps (as
   `ui_tracktion.rs`) or is split into binaries. Network tests that need no sound get an engine
   without plugins and without a device. `plugins.rs` tests the Rust CLAP host and its helper and
   goes; what it checks about the session (a plugin's state reaching the document, restart after
   a failure) moves to the Tracktion tests. Nothing tested today goes untested without a line
   here saying so.
   - *The test synth.* Tests got it from `mdaw_plugin` (`Host::test_synth_only()`, which hosts
     the crate in process, and `TEST_SYNTH`). On Tracktion it must be a `.clap` file in a
     `CLAP_PATH` folder. `tests/slice.rs` has a private `clap_dir()` that takes the dev-
     dependency's `cdylib` from `target/debug/deps` (building it only if missing) and copies it
     there; the bridge tests and `mdaw-tracktion`'s tests expect it copied by hand to
     `target/bridge-scratch/clap`. One shared helper for all of them, in `mdaw-test-plugin`
     (`pub fn clap_folder(dir)`, keeping slice.rs's care about concurrent copies and not
     rebuilding during a run), and `mdaw_test_plugin::ID` instead of `mdaw_plugin::TEST_SYNTH`.
4. **Remove the crates and the old Pocket** (one commit: `mdaw-dsp` and `mdaw-engine` use the old Pocket themselves, and `mdaw-tracktion`'s 4osc approximation needs `mdaw-dsp`). `mdaw-engine`, `mdaw-dsp`, `mdaw-plugin` from the workspace and from
   xtask's build list (`-p mdaw-plugin`); `xtask demo --tracktion` becomes the default (the flag
   goes). `mdaw-test-plugin` stays (the bridge tests and the client tests use the test synth).
   Workspace dependencies only they used go: `cpal`, `rtrb`, `libm`, and in `mdaw-plugin`'s own
   list `clack-host`, `clack-extensions`, `memmap2`, `x11rb`, `libc`. `midir` moves to the
   client with the MIDI input (step 2); `clack-plugin` stays (the test synth); `blake3` and
   `postcard` stay (proto, server, model). `Cargo.lock` follows.

   The old Pocket: the model variant, the 4osc approximation table, the
   tests that use it (`mdaw-model/tests/properties.rs`, `mdaw-sync/tests/replica.rs`,
   `mdaw-tracktion/tests/engine.rs`, client `record.rs`, `session.rs`, `ui/devices.rs`, tests
   `net.rs`, `playback.rs`, `plugins.rs`); protocol 11, store 7. `mdaw-tracktion` then no longer
   depends on `mdaw-dsp`. In `ui/devices.rs` the old Pocket panel goes whole (`SECTIONS`,
   `param()`, `KnobParam for ParamInfo`, `to_norm`, `from_norm`, the `mdaw_dsp::pocket` import):
   the 4osc panel uses only `mdaw_model::two_osc`, so nothing has to move into the model for it.
   `session.rs` loses its `pocket::DSP_VERSION` arm. (`mdaw-slice`'s `pocket-param` command
   drives the 4osc instrument; at most a rename.)
5. **CI and release** (decisions 1 and 2; nothing is pushed between step 3 and this one, since CI cannot build the client before it): Linux only; checkout with submodules
   (`git -c url."https://github.com/".insteadOf="git@github.com:" submodule update --init
   --recursive --depth 1`); the packages from `docs/tracktion-integration.md` "Build" plus
   cmake, ninja and xvfb/xprop; cache `target/native`; build the scanner, the test plugins and
   the test synth; run the C bridge test, the Rust tests, and the window tests through
   `run_gui_tests.sh`. Release: the client, the server and `mdaw-plugin-scan`; no
   `mdaw-plugin-host`. Packages: `libpipewire-0.3-dev` and `libclang-dev` were for cpal's
   PipeWire backend (bindgen) and go; JUCE's come in (ALSA, JACK headers, X11, Xext, Xrandr,
   Xinerama, Xcursor, Xcomposite, Xrender, freetype, fontconfig, ladspa; the Ubuntu names of the
   Arch list in `tracktion-integration.md` "Build"), with cmake, ninja, xvfb and x11-utils. `default-members` and the `--exclude` flags go: the whole workspace is
   built and tested.
6. **Licence.** Decided 2026-11-12 (`tracktion-integration.md`, "Licences"): the client links
   JUCE 8 and is distributed under AGPLv3 terms, "before the first release that links JUCE" —
   which is the next release. `mdaw-client`'s `license` becomes `AGPL-3.0-only` (the workspace
   default stays `GPL-3.0-only`, for the server and the libraries); the AGPLv3 text is added
   (`LICENSE-AGPL`, shipped with the client by the release); README's licence section says which
   applies to what.
7. **Docs.** After step 4, so that ADR 0002 can name the last commit that still has the helper
   (step 4's parent) for recovering it. `architecture.md`, `migration-checklist.md` (rows to
   "Removed"), `plan.md`, ADR 0002 (decision 3 and that commit), README (Linux only; the build
   needs the native part and its packages), `tracktion-integration.md` ("Build" becomes the
   normal build), `protocol.md` (protocol 11, store 7, and its mentions of the old Pocket and
   the Rust engine), `optimizations-plan.md` (about the old engine's processors and helper:
   marked as history at the top, not rewritten), `manual-tests/tracktion-client.md` (step F.1
   compares with `--old-engine`: removed). ADR 0001 needs nothing (it does not mention the old
   engine). The m2–m6 manual tests stay as they are, as history.
8. **Version and release notes.** Workspace version 0.2.0. Release notes: Tracktion Engine with
   VST3, LV2 and CLAP; Linux only (no Windows build until the bridge is ported); data
   directories, projects and client profiles of 0.1 are not readable (protocol 11, store 7);
   the client is AGPLv3.
9. **Hand test.** `docs/manual-tests/tracktion-client.md` and `plugin-windows.md` on the result,
   with a fresh data directory (old ones are refused).

No release is tagged before step 8: steps 5 and 6 together make the first release that
links JUCE, and it has to carry the AGPLv3 text.

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
- Every existing server data directory and project is unreadable after step 4 (decision 4).
- CI minutes: the native build and the slower Tracktion tests make each run longer; the
  `target/native` cache is what keeps it bearable.
