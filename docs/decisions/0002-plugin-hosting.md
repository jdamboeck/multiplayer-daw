# ADR 0002: Plugin hosting — VST3 and LV2 through JUCE, CLAP through our own format, in process

Status: accepted, 2026-11-12 (in-process now; optional per-plugin isolation as a later, planned
phase). Amended 2026-11-15: `mdaw-plugin` and its helper are removed (see "Decision" and the
amendment at the end). Measurements are in `docs/tracktion-integration.md`.

## Question

Tracktion hosts VST3 and LV2 through JUCE. Neither Tracktion 3.2 nor JUCE 8.0.6 hosts CLAP
(`clap-juce-extensions` builds CLAP *plugins* from JUCE code; it is not a host). Today we host
CLAP in Rust, each plugin in its own helper process. What hosts plugins once Tracktion is the
engine, and what happens when a plugin misbehaves?

## CLAP: three ways

1. **A CLAP host written in C++ as a JUCE `AudioPluginFormat`**, registered with Tracktion's
   plugin manager. Tracktion then treats CLAP like VST3: same `ExternalPlugin`, automation,
   latency compensation, state, editor window.
2. **A JUCE format whose instances are backed by the existing Rust CLAP host.**
3. **A Tracktion custom plugin that hands its audio to the Rust host and its helper process.**

2 and 3 keep the Rust code, but both put Rust inside Tracktion's audio callback, which the
project rules forbid. Keeping the isolation helper under either would also need the parent
side of its shared-memory protocol on the audio thread — that is, rewritten in C++. Neither
was prototyped.

1 was built: `native/clap-format/` (about 700 lines, against the CLAP 1.2.9 headers). Tested:

- discovery: 18 types found;
- instruments from MIDI and effects on audio, with the same check as VST3;
- parameters both ways, automation, state save and restore;
- latency reported to Tracktion and compensated (same figures as the VST3 build of the plugin);
- editor windows embedded in a JUCE window, opened and closed twice, five plugins;
- real-time playback on a device (the repo's test synth only): no block skipped;
- 7 of 9 plugins pass every step; the other 2 do not keep parameters in their state, and their
  VST3 builds do not either.

Not covered yet: side-chain and extra output buses (disabled), note expressions, parameter
modulation, remote controls, out-of-process scan, resizing editors.

## Isolation

Measured with the repo's test synth during live playback: a failing plugin is harmless; a slow
one drags the whole project; a hung one stops all audio (the UI stays alive); a crashing one
takes the process down. Scanning can run in a child process; processing cannot. This is what
every in-process host does, and it is a real step back from what `mdaw-plugin` did then.

What that step costs in this product is smaller than it was: the project lives on the server
and in the local log, so a crash loses at most the edits not yet acknowledged — and pending
transactions are to be kept durably on the client and resubmitted (phase 5).

## Decision

- **VST3 and LV2: JUCE's hosts, as Tracktion provides them.**
- **CLAP: option 1**, our own format, kept small.
- **Plugins run in process.** Failure policy:
  1. Scan in a child process; a plugin that crashes or hangs the scan is listed as failed and
     not offered.
  2. Before loading a plugin, write its identity to a marker file; clear it once the plugin has
     processed its first blocks. If the app starts and finds a marker, that plugin is loaded
     disabled and the user is told why.
  3. A watchdog on the message thread notices when the audio callback has stopped returning
     and tells the user which plugins are loaded; the app stays usable to save and quit.
  4. Pending edits are on disk before they are sent, so a crash loses no work.
  5. Plugin libraries are never unloaded (integration doc, point 6).
- **`crates/mdaw-plugin` and its helper stay in the tree** until the bridge hosts CLAP in the
  real client and the manual test has passed. Then they are removed with the Rust engine
  (phase 7). *Amended 2026-11-15: removed, the helper included; see below.*
- **Isolation can come back** as a per-plugin option: a Tracktion plugin in C++ that speaks the
  existing shared-memory protocol to the existing Rust helper *process*. Rust then runs only in
  the other process. Decided 2026-11-12: this is a planned phase after the bridge and the
  collaborative slice, not a maybe. *Amended 2026-11-15: the helper and its protocol are no
  longer kept in the tree until then; that phase recovers them from git history.*

Item 1 is built and tested (`mdaw-plugin-scan`, with plugins that crash and hang on purpose).
Item 4 is built and tested (2026-11-13: the client's outbox, `mdaw-client/src/outbox.rs`; see
"What the client does" in `docs/protocol.md`). Items 2 and 3 are designed, not built. Item 5 is in
the bridge.

## Addendum 2026-11-15: responsiveness and plugin windows (ABI 5)

Measured, plugin windows were slow to open not because of the plugins (5–75 ms) but because of
what waited in front of them: a scan that held the engine's message thread while scanner
children ran, a client worker that ran the scan first and queued polling without bound, and
plugin states saved two or more times per read. Fixed without changing the decisions above: the
scan's waiting moved off the message thread (scanner children still examine one file each, with
the same time limit and records); the client's worker got a scheduler with priorities that keep
the order of dependent work, and coalesced background polling; windows got an explicit
lifecycle with tokens. Details and numbers: `docs/tracktion-integration.md`, "ABI 5".

VST3 and LV2 instance and editor creation stay on the message thread, as JUCE 8.0.6 requires.
A plugin call that hangs or is slow still blocks the engine; in-process calls cannot be
interrupted.

**Out-of-process plugin windows are not built.** Carla separates UI bridges (a plugin's window in
another process) from plugin bridges (its processing in another process) and prefers UI bridges
by default. After the fixes above, the measured delays left are the plugins' own window creation
(tens of milliseconds) — nothing a UI process would remove; it would add process start and IPC,
and it cannot be done for VST3 and LV2 instances that live in this process without the instance
data-access paths those formats rely on. A UI process would only pay off for plugins whose window
code hangs or crashes the application while their processing is fine; none is known so far. If
one appears, the smallest useful prototype is for LV2 only (the format has a clean UI/DSP split
through ports and atoms): a helper process that instantiates the plugin's LV2 UI, drives
`LV2UI_Idle_Interface` at 30 Hz or more on its own UI thread, embeds into an X11 window id the
host passes, and exchanges port writes and atom messages with the host over a pipe, with a
timeout like Carla's (4 s) after which the host closes the window and says so. CLAP and VST3 would
come after, if ever. The per-plugin isolation of *processing* stays the planned phase above.

## Amendment 2026-11-15: the helper is removed

Decision 3 of `docs/retire-rust-engine.md` (the project owner, 2026-11-15): `mdaw-plugin` goes
with the Rust engine now, the isolation helper `mdaw-plugin-host` and its shared-memory protocol
included. Per-plugin isolation, when it comes, needs a C++ host side anyway (a Tracktion plugin
on the parent side, see above); keeping the Rust crate building in the meantime cost more than
recovering it.

The last commit that has the crate is `7521840` ("Play through Tracktion only, and port the
client's tests to it"); it was removed in the next one, `82dbaa4`. The helper is
`crates/mdaw-plugin/src/bin/mdaw-plugin-host.rs`, the protocol and both of its sides
`crates/mdaw-plugin/src/isolated/` (`protocol.rs`, `shm.rs`, `parent.rs`, `child.rs`), its tests
`crates/mdaw-plugin/tests/isolated.rs`. To read a file: `git show
7521840:crates/mdaw-plugin/src/isolated/protocol.rs`; to bring the whole crate back into the
tree: `git checkout 7521840 -- crates/mdaw-plugin`. As it stands there it depends on `mdaw-dsp`
and `mdaw-engine` (also in that commit) and on the workspace dependency `rtrb`; its other
dependencies are in its own `Cargo.toml`.

## Consequences

- One plugin path for all formats: automation, latency, state, windows behave alike.
- We own a CLAP host in C++ and must follow the CLAP spec ourselves.
- Capabilities given up for now: per-plugin process isolation and the deadline that let a slow
  plugin be skipped. Stated here so it is a choice, not an accident.
