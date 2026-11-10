# Spike S1: hosting CLAP plugins from Rust

Date: 2026-11-10. Code: `spikes/clap-host` (throwaway, excluded from CI lint and tests).

## Decision

**Continue with `clack-host` 0.2.** Everything the prototype needs from a CLAP host worked on
Linux: scanning, instantiation, parameters, state, real-time and offline processing, and
embedded plugin editors. Nothing found here points towards the JUCE fallback.

Windows was not tested (no machine available in this session). The spike's processing code is
platform-neutral; its editor code is Linux-only. Windows stays a manual check at M6.

## What was tested

Machine: CachyOS, niri (Wayland) with XWayland, PipeWire 1.6, rustc 1.97. Plugins installed
locally: 18 in 20 `.clap` files under `~/.clap` and `/usr/lib/clap`.

| Plugin | Kind | Load, params, state | Offline render | Real-time (cpal) | Editor |
|---|---|---|---|---|---|
| Vital 1.6.0 | instrument, 903 params | ok | ok | ok | ok |
| Odin2 2.4.1 | instrument, 258 params | ok (see cookie note) | ok | not run | ok |
| ChowKick 1.2.0 | instrument, 21 params | ok | ok | not run | not run |
| Riverflux RF-76 0.3.1 | effect, 20 params, sidechain port | ok | ok | ok | ok |
| BIT LAYER 0.5.7 | effect, 181 params | ok | ok | not run | not run |

Commands, all of the form `cargo run -p spike-clap-host -- <cmd>`:

- `scan`: list every plugin in the search paths.
- `info <id> [--params]`: ports, extensions, parameters.
- `render <id> [--param name=0..1]...`: offline render, parameter change, state round trip,
  second and third instance, comparisons.
- `play <id> [--gui]`: real-time output through cpal, optionally with the editor open.
- `gui <id> [--shot file.png]`: open the editor in a host-created window and screenshot it.

## Findings

### Hosting basics

- Scanning costs 0 to 8 ms per file. Instantiation takes 0 to 135 ms (Odin2 is the slowest).
- Plugin IDs are arbitrary strings. Plogue's contain spaces and commas
  (`com.Plogue Art et Technologie, Inc.chipsounds`). Store and compare them as opaque strings.
- Offline rendering runs at 60x to 190x real time for one instance (dev profile).
- Real time through cpal's PipeWire host: 48 kHz, 64-frame callbacks, no stream errors.
  Vital's worst callback was 1.5 ms against a 1.33 ms budget, once per run; RF-76 stayed at 50 us.
- Extensions the host must provide: `log`, `thread-check`, `params`, `state`, `gui`,
  `timer-support` (every editor registered a timer), `posix-fd-support` (the Riverflux and
  BIT LAYER editors use it), `audio-ports`, `note-ports`. `render` is optional on the plugin
  side: Odin2 does not have it.

### Rules the host has to follow

These were found by breaking them:

1. **Send parameter cookies.** Odin2 segfaults inside `process()` when a parameter event carries
   a null cookie. The host must keep the cookie from `params.get_info` and attach it to every
   parameter event (`ParamValueEvent::with_cookie`).
2. **`stop_processing()` belongs to the audio thread.** Vital reports `HOST_MISBEHAVING` when it
   is called from the main thread. The engine must stop a plugin from inside the audio callback
   and only then deactivate it on the main thread.
3. **`gui.show()` may report failure and still work.** Odin2 does this. Treat the result as a
   warning.
4. A plugin crash takes the process down, since hosting is in-process. The prototype accepts
   this; the on-disk checkout journal from the plan limits what a crash can lose.

### Editors

- Every plugin tested supports exactly one windowing mode on Linux: **X11, embedded**. None
  offers a floating window and none supports Wayland. So the first attempt in the plan (floating
  window) is not available, and the second is the answer: the host creates an X11 window with
  `x11rb` and hands it to the plugin as parent. Under Wayland this goes through XWayland.
- That works. All three editors render correctly, including Vital's OpenGL editor, and keep
  animating from host timers. Vital's editor also ran while its audio processed on the cpal
  thread, with no errors.
- Fixed-size editors need `WM_NORMAL_HINTS` with minimum equal to maximum size. Without it a
  tiling compositor stretches the host window and the editor is clipped or letterboxed. With
  it, niri floats the window at the plugin's size.
- Resizable editors (RF-76) follow the host window through `adjust_size` and `set_size`.
- Consequence for the client: plugin editors are separate top-level X11 windows, not embedded in
  the egui window. A system without XWayland gets no plugin editors; parameters stay reachable
  through generic controls.

### State and reproducibility

The test: render 4 s (a chord and a single note for instruments, a gated saw through effects),
change a parameter, save state, load it into new instances, render again, and compare.

| Plugin | State size | Same instance, rendered twice | Fresh instance vs first | Two fresh instances loaded from the same state |
|---|---|---|---|---|
| RF-76 | 2 KB | bit-identical | bit-identical | bit-identical |
| BIT LAYER | 8 KB | bit-identical | bit-identical | bit-identical |
| ChowKick | 1.7 KB | bit-identical | bit-identical | differs at -27 dB with noise enabled |
| Odin2 | 49 KB | differs audibly | bit-identical | bit-identical |
| Vital, default patch | 179 KB | differs audibly | differs audibly | differs audibly |
| Vital, phase randomisation off | 179 KB | bit-identical | not run | bit-identical |

- **State restore works everywhere.** A second instance reports the changed parameter values
  after `state.load` and sounds like the first. For Vital with phase randomisation off, the
  difference between the two instances is 139 dB below the signal.
- **Bit-exact reproduction cannot be assumed.** Vital's default patch randomises oscillator
  phase per note, and ChowKick's noise is unseeded, so no two renders match. Odin2 carries
  internal state across deactivate and activate, so a reused instance drifts from a fresh one.
  "Differs audibly" here means the same notes at the same level within about 3 dB per 50 ms
  window, the plugin's own run-to-run variation.
- **A fresh instance is the most reproducible starting point.** Odin2 is bit-identical from a
  fresh instance and not from a reused one.
- **State blobs are not canonical.** Vital's state re-saved by a second instance has the same
  length and 115,688 differing bytes. Saving twice from one instance gives identical bytes.
  A changed state hash therefore does not mean the sound changed.
- Plugins do not raise `state.mark_dirty` for parameter changes that came from the host, which
  matches the CLAP spec. The host has to track its own parameter edits.
- **Plugin output exceeds full scale.** Peaks reached +2.1 dBFS (Vital), +3.3 dBFS (Odin2) and
  +4.9 dBFS (RF-76 driven hard). Integer FLAC would clip these.

## Consequences for the plan

1. **The plan's risk "offline second instances may not reproduce the live instance's sound" is
   retired.** Offline instances restored from state sound right, so stems and changelog samples
   are bounced offline as planned; the real-time bounce fallback is not needed.
2. **Bounce recipe (M6):** create a new instance, load the state blob, set render mode to
   offline where the plugin supports it, activate, render, destroy. Never reuse the live
   instance or an earlier bounce instance.
3. **The committed stem is the reference sound of a CLAP track**, as planned. A client that has
   the plugin and renders it live may hear a slightly different performance than the stem.
   That is inherent to plugins like Vital and is accepted. Changelog samples for CLAP tracks are
   cut from the same render as the stem, so the sample and the stem always agree.
4. **Stems get a stored gain (new, M6):** the bounce measures its peak, scales to at most
   -1 dBFS before the 24-bit FLAC encode, and records the factor in the stem metadata
   (`stem.gain`), which playback undoes. Samples do the same.
5. **`DeviceSetState` is emitted only after a real change** (a parameter edit, or the plugin
   raising `mark_dirty`), never by comparing state hashes. Otherwise every commit by a different
   client would upload a new 179 KB blob for an unchanged Vital patch.
6. **Engine rules (M6):** keep parameter cookies and refresh them on `params.rescan`; start and
   stop processing on the audio thread; run host timers and fd polling from the UI loop.
7. **Editor windows (M6):** host-created X11 top-level window on Linux with fixed-size hints
   where the editor is not resizable. Win32 parenting on Windows, untested so far.

## Not covered by the spike

- Windows, in every respect.
- Passing transport and tempo to `process()`. Tempo-synced plugins need it; the engine provides
  it in M6.
- Several plugin instances in one audio graph, latency reporting, note expressions, parameter
  automation from the editor back to the host (the event path exists, nobody turned a knob).
- Real-time safety of the spike itself: it locks a mutex in the audio callback. The engine uses
  the lock-free design from the plan.
