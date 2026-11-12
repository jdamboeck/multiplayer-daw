# ADR 0002: Plugin hosting — VST3 and LV2 through JUCE, CLAP through our own format, in process

Status: accepted, 2026-11-12 (in-process now; optional per-plugin isolation as a later, planned
phase). Measurements are in `docs/tracktion-integration.md`.

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
every in-process host does, and it is a real step back from what `mdaw-plugin` does today.

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
  (phase 7).
- **Isolation can come back** as a per-plugin option: a Tracktion plugin in C++ that speaks the
  existing shared-memory protocol to the existing Rust helper *process*. Rust then runs only in
  the other process. Decided 2026-11-12: this is a planned phase after the bridge and the
  collaborative slice, not a maybe; the helper and its protocol stay in the tree until then.

Policy items 1–4 are designed, not built. Item 5 is in the spike.

## Consequences

- One plugin path for all formats: automation, latency, state, windows behave alike.
- We own a CLAP host in C++ and must follow the CLAP spec ourselves.
- Capabilities given up for now: per-plugin process isolation and the deadline that let a slow
  plugin be skipped. Stated here so it is a choice, not an accident.
