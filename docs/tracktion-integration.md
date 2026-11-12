# Tracktion Engine integration

Status: 2026-11-12. Everything stated as a result here was run on this machine; anything not
run is listed under "Not tested yet".

## Versions

| What | Pin | Notes |
|---|---|---|
| Tracktion Engine | tag `v3.2.0`, commit `0a5f4e6a5f53d09c89b414a44386a12df7fa1ec6` | submodule `native/third_party/tracktion_engine`; `Engine::getVersion()` still says "v3.1.0" |
| JUCE | 8.0.6, commit `19edd538429c93d277bf95b55aaa7e3eb545f951` | Tracktion's own submodule, `modules/juce` |
| CLAP headers | tag `1.2.9`, commit `cd94482ba5941ae410809b6fbaed3bc851044270` | submodule `native/third_party/clap`, MIT |

Nothing is fetched at build time and no branch is tracked. To move a pin, check out the new tag
in the submodule and commit the submodule pointer.

**Licences.** Tracktion Engine is GPLv3 or commercial. JUCE 8 is AGPLv3 or commercial. This
project is GPL-3.0-only. A binary that links JUCE 8 without a commercial licence is distributed
under AGPLv3 terms; the server does not link either and is unaffected. Decided 2026-11-12:
the client is distributed under AGPLv3 terms. The licence files and README have to say so
before the first release that links JUCE.

Test machine: CachyOS, Linux 7.3.1, PipeWire 1.6.10 with `pipewire-jack`, GCC 16.2.1,
CMake 4.4.5, Ninja.

## Build

Packages (Arch / CachyOS):

```sh
sudo pacman -S --needed base-devel cmake ninja git \
    alsa-lib pipewire-jack freetype2 fontconfig \
    libx11 libxext libxrandr libxinerama libxcursor libxcomposite libxrender \
    ladspa
```

`pipewire-jack` (or `jack2`) supplies the JACK headers and library; JUCE loads the JACK library
at run time. X11 libraries are needed for plugin editor windows. `curl` and WebKit are switched
off in the build.

Fetch the sources (the first command is only needed if GitHub over SSH is not set up, because
some nested submodules use SSH URLs):

```sh
git -c url."https://github.com/".insteadOf="git@github.com:" \
    submodule update --init --recursive --depth 1
```

Configure and build:

```sh
cmake -S native -B target/native -G Ninja -DCMAKE_BUILD_TYPE=RelWithDebInfo
cmake --build target/native --target mdaw-te-spike
```

The spike binary is
`target/native/spike/mdaw-te-spike_artefacts/RelWithDebInfo/mdaw-te-spike`.

`native/CMakeLists.txt` passes `-Wno-template-body`: GCC 15 and later reject the bundled
`nanorange.hpp` without it. The build is C++20.

Nothing here is Linux-only by design. The CLAP editor embedding (`XEmbedComponent`), the
`dlopen` pinning and the plugin search paths are the Linux-specific parts and are small.

## The spike

`native/spike/` is a console program that drives Tracktion the way the bridge will. Every
command prints `RESULT key=value` lines and exits non-zero if a check fails.

```sh
S=target/native/spike/mdaw-te-spike_artefacts/RelWithDebInfo/mdaw-te-spike
$S devices                         # audio and MIDI devices per backend
$S play [--type JACK] [--device <name>] [--rate <hz>] [--block <n>]   # play, stop, seek, edit while playing
$S saveload                        # build an edit, save, reload, compare
$S render --out demo.wav           # offline render, no device opened
$S scan --format VST3|CLAP|LV2 [--oop]            # --oop: scan in a child process
$S plugin  --file <path> --format <fmt> [--param <name>] [--list-params]
$S latency --file <path> --format <fmt> [--set Name=value]   # e.g. Mix=0, so the plugin passes its input through
$S editor  --file <path> --format <fmt> [--hold <ms>]
$S misbehave --file <test-synth.clap> --mode none|fail|slow|hang|crash
```

The demo for "MIDI playback and offline render": `play` plays an eight-second MIDI clip through
Tracktion's built-in FourOsc synth on the default device; `render` writes the same edit to a
wav file without opening a device.

## Results: engine

| Check | Result |
|---|---|
| Device discovery | ALSA and JACK backends listed with their devices and MIDI ports |
| Playback on ALSA (→ PipeWire) | plays, position advances, 0 xruns |
| Playback on JACK (PipeWire's JACK) | plays, position advances, 0 xruns |
| Stop | playhead stays, output falls silent |
| Seek, then play | position correct |
| Edit while playing (add note; add track with instrument and clip) | playback continues, 0 restarts |
| Save and reload | same tracks, clips, notes, plugin |
| Offline render without a device | wav written, audible signal |

## Results: plugins

VST3, scan: 231 plugin types from 34 files, no failures; the out-of-process scan finds the same.

VST3, hosting, 14 plugins (Vital, Odin2, OB-Xd, ChowKick, Replicant303, SID, vitalium, Geonkick,
vitOTT, Dragonfly Hall Reverb, RF-76, RF-2A, SwankyAmp, LSP Compressor Stereo). For each: load,
render (instrument from MIDI, effect on an audio clip), set a parameter, save, reload, compare
state and parameter, add automation and compare the render.

- 14 of 14 load, render, restore their parameter and respond to automation. Odin2 and SID need
  a continuous parameter named for the check (`--param`): the first one the spike picks is
  stepped without saying so and snaps.
- Effects change the audio (difference from dry far above the repeat-render noise).
- Vital and vitalium do not write bit-identical state twice; their parameters are restored.
- chipsounds and chipsynth-C64 do not keep parameter values in their state. Their CLAP builds
  behave the same, so this is the plugins, not the host.

CLAP, through the format in `native/clap-format/` (see ADR 0002): scan finds 18 types in about
70 ms. Hosting check on 9 plugins: Vital, ChowKick, RF-76, RF-2A, RF-EQ1-A and BIT LAYER pass
everything; Odin2 passes with a continuous parameter named, as its VST3 build does. chipsounds
and chipsynth-C64 fail only the parameter-restore step, as their VST3 builds do. RF-76 gives the same numbers as CLAP
as it does as VST3. No plugin was activated off the main thread and no audio block was skipped.

LV2 (JUCE's own host): scan finds 154 types. Dexed and DISTRHO a-reverb pass the hosting check;
ChowKick's LV2 build loads and renders but does not restore the parameter.

Latency compensation (RF-76 and RF-2A, VST3 and CLAP): the plugin reports 64 samples; measured
on the bare instance, 64. On a track, the output lines up with a track that has no plugin, and
two tracks (one with the plugin, one without) sum to twice one track within 8e-8 RMS — without
compensation the error would be 0.24.

Editor windows: VST3 (Vital, OB-Xd, RF-76, Dragonfly) and CLAP (ChowKick, Vital, RF-76, Odin2,
chipsounds) open in a native window, close, open again and close, with no error; a parameter
changed from the host side while the editor is open reaches the engine. Screenshots of the
CLAP editors of ChowKick, Vital, RF-76 and Odin2 show their full interfaces. Turning a knob
with the mouse is in the manual test.

Misbehaving plugin, during live playback, using the repo's CLAP test synth:

| Mode | What happened |
|---|---|
| none | plays normally, 0 blocks skipped |
| fail (process returns an error) | playback continues |
| slow (50 ms per block) | whole project slows to about a quarter speed, CPU meter at 100 % |
| hang (never returns) | playhead frozen, all audio stops; UI thread stays responsive |
| crash (abort) | the whole process dies |

So Tracktion gives no protection against a plugin once it is processing. Scanning can run in a
separate process; processing cannot. What we do about it is in ADR 0002.

## What the bridge must do

These were all found by running into them.

1. **Everything goes through the JUCE message thread.** Tracktion's housekeeping (graph
   rebuilds, device changes, plugin loading, timers) runs there. The bridge owns one thread
   that runs the message loop; every C ABI call posts to it.
2. **Keep a wanted transport state.** Any change to the device list — a MIDI device plugged in,
   or Tracktion's own MIDI ports appearing just after the audio device opens — clears the
   playback graph and stops the transport. Seen once after every device open. The bridge
   remembers "should be playing" and starts again.
3. **Own the settings file.** Tracktion writes the chosen backend to
   `~/.config/<app>/Settings.xml` and reuses it. The bridge supplies its own storage and sets
   the backend explicitly.
4. **JACK decides rate and block size.** Requests are ignored (48000 / 192 here). The JACK
   client is called "JUCEJack" unless `JUCE_JACK_CLIENT_NAME` is set.
5. **A plugin must be in the known-plugin list before it can be loaded.**
6. **Never unload a plugin library.** OB-Xd leaves a thread running after its last instance is
   gone; when JUCE unloads the library the process crashes (5 of 5 runs). Loading each module
   once with `RTLD_NODELETE` before use fixes it (5 of 5 pass).
7. **Drain the message loop before and after destroying an edit** (about 300 ms), or VST3
   teardown can crash.
8. **A parameter reaches the plugin with the next processed block.** Set a value, save
   immediately, and the saved state has the old value. Process (or render) before reading state.
9. **Identify parameters by ID, never by name.** Tracktion adds its own "dry level" and "wet
   level" to every external plugin; Dragonfly has a "Dry Level" of its own.
10. **Tracktion's clip path is not bit-transparent.** An audio clip rendered through an empty
    track comes out 2 samples late and about 3 dB down (0.708). Cause not investigated; likely
    the default pan law. Tests compare against a rendered reference, not the source file.
11. **Synths are not repeatable.** FourOsc, Vital, Odin2 and OB-Xd give different samples on
    each render (free-running oscillators). Tests measure the repeat difference first and
    require a change well above it.
12. **Define the window-manager atoms before JUCE starts.** JUCE — ours, and the older JUCE
    inside plugins such as Vital — looks up atoms like `_NET_WORKAREA` "only if they exist" and
    uses some unchecked. On XWayland under niri, right after login, nine of them do not exist
    and opening Vital's editor ends the process with `BadAtom (X_GetProperty)`. ChowKick's
    editor opened on the same server, so it is the plugin's code that trips. Once any program
    has defined the atoms the problem is gone until the X server restarts, which is why it
    only showed after a reboot. `mdaw::defineWindowManagerAtoms()` (`native/clap-format/x11_atoms.cpp`)
    defines them at startup.
13. **A CLAP plugin may ask for timers and file descriptors while it is being initialised.**
    Plugins built on a toolkit (ChowKick, on JUCE) run their whole event loop from the host's
    timer. The host must accept the request from the first moment; ours first refused it until
    its own object existed, and the editor window stayed black.

### Found while building the bridge

14. **Never run a JUCE dispatch loop from inside a message callback on Linux.** A nested
    `runDispatchLoopUntil` empties the event list the outer loop is iterating; the outer loop
    then sleeps for ever. The spike's `pump()` is only safe because `main` calls it. The bridge
    splits such work into steps with timer gaps instead. Tracktion nests a loop itself in the
    `PluginManager` destructor, so the engine is deleted only after the loop has ended.
15. **Tracktion's `paramID` for a hosted plugin is the parameter's index as text.** The
    plugin's real id is `HostedAudioProcessorParameter::getParameterID()`.
16. **Without a running device the transport stops itself** and overwrites the position from a
    playhead that never moves. The bridge keeps the wanted state and position itself.
17. **Changing the tempo map does not always move clips** (`insertTempo`, `removeTempo` do not;
    `setBpm` does). The bridge places every clip again from its ticks after any tempo change.
    A tempo point's curve 0 means a ramp; steps need curve 1.
18. **Renderer defaults to switch off:** `canRenderInMono` (a mono file when the graph is
    mono) and `checkNodesForAudio` (rendering silence is an error).
19. **Tracktion opens every MIDI port, hardware included,** shortly after the first edit, even
    with no audio device.
20. **An external parameter set by the host comes back** later as a change "by the plugin" with
    the same value; the bridge filters it so the application sees no echo.
21. **`Edit::Options::defaultMasterVolumedB` is -3 dB.** That is the 3 dB of point 10's level
    difference; the bridge sets 0. `PluginList::insertPlugin` can refuse without an error.
22. **JUCE's identifier string for a plugin contains a hash of its file path,** so it differs
    between machines. A shared project needs the plugin's own id. For VST3, JUCE keeps only a
    32-bit hash of the class id; the bridge reads the full id from the bundle's
    `moduleinfo.json` when there is one, otherwise from the plugin's factory. On Windows the
    first eight bytes are stored in a different order and are converted, so the text is the
    same everywhere. The conversion is checked against the VST3 SDK's own code on Linux; the
    Windows and macOS loading code has never run.
23. **Examine plugin files in a child process.** `mdaw-plugin-scan` looks at one file and
    writes what it found; the bridge runs it per file with a time limit (30 s), kills it if it
    hangs, and remembers a file that crashed or hung until the file changes. With it, listing
    plugins and adding a device never load a library just to look at it. Scanning this machine
    that way: VST3 0.75 s (229 plugins), LV2 2.5 s (154), CLAP 0.1 s (17), the same plugins as
    in-process, no failures; a repeat scan with nothing changed takes about 0.1 s.
24. **LV2 dynamic manifests are loaded in the application** when the engine starts, scanner or
    not: JUCE builds lilv with that (deprecated) extension on. None are installed here.
25. **Do not ignore SIGCHLD in the application.** The bridge then cannot tell a scanner crash
    from an empty result.

## The bridge

`native/tracktion-bridge/`: the C header `include/mdaw_engine.h` is the whole interface; the
implementation is a static library that carries JUCE and Tracktion inside it.

```sh
cmake --build target/native --target mdaw-bridge-test
cargo build -p mdaw-test-plugin
mkdir -p target/bridge-scratch/clap
cp target/debug/libmdaw_test_plugin.so target/bridge-scratch/clap/mdaw-test-synth.clap
CLAP_PATH=$PWD/target/bridge-scratch/clap target/native/tracktion-bridge/mdaw-bridge-test
```

The test is written in C, uses no audio device and ends with `RESULT ok=1`. It covers building
a project by id, rendering, idempotent repeats, note updates, mute, gain, bypass, the tempo
map, parameters and state for the built-in synth and a CLAP plugin, no echo of own changes,
and creating the engine twice in one process.

From Rust: `crates/mdaw-tracktion-sys` (hand-written declarations; its build script runs the
CMake build in `target/native`, or in `MDAW_NATIVE_BUILD_DIR`) and `crates/mdaw-tracktion`
(safe `Engine`, plus `load_project` and `apply_op`, which bring a canonical `mdaw_model`
project into the engine whole or one operation at a time).

```sh
cargo test -p mdaw-tracktion-sys -p mdaw-tracktion
```

Plain `cargo build` and `cargo test` leave these two crates out (`default-members`), and CI
excludes them, because they need the submodules and a C++ toolchain. The integration test
applies model operations to the model and the engine side by side, renders after each step,
and checks that the incremental result matches a fresh load. After moving a submodule pin,
run `cmake --build target/native` once: the build script does not watch third-party sources.

ABI 2 and 3 closed the gaps found from the Rust side. Plugins are named by their own id, the same
on every machine (CLAP id, LV2 URI, VST3 class id as 32 hex digits; see
`src/plugin_ids.h`): JUCE's identifier string contains a hash of the file path. The ids of
CLAP and VST3 plugins are read from their libraries once and kept in `plugin-ids.xml` in the
settings folder. The list offers only plugins whose file is still there, each id once. CLAP
parameters report their plain range, so the model's values (plain, by CLAP id) are applied,
after the state. `mdaw_project_has`, `mdaw_clip_set_name`, `mdaw_device_reset_state` (back to
the state captured when the device was made) and `mdaw_last_status` are new. The engine's
tempo range (20–300 bpm) is still narrower than the model's (20–400); the Pocket synth's
parameters are still not mapped to the engine's synth.

One VST3 and one LV2 instrument each were loaded and rendered through the bridge by hand
(ChowKick and Vital as VST3; Dexed and String machine as LV2), not by the committed tests.

The built-in Pocket synth of the model is played by Tracktion's 4osc. All twenty Pocket
parameters are mapped (table in `crates/mdaw-tracktion/src/apply.rs`); it is an approximation
for the migration — velocity response, voice count and the exact filter and envelope shapes
differ. For that table `mdaw-tracktion` depends on `mdaw-dsp`; when the Rust engine is retired
the table moves or the model's built-in device becomes 4osc itself.

The model's tempo limit is now 20–300 bpm, the range Tracktion plays.

Open model question: after a parameter is put back to "default" in the model and the project
is reloaded, the value in the plugin's stored state wins over the default.

Built but not yet exercised by any test: plugin editor windows through the bridge; everything
that needs an open audio device (device switching, meters, xruns, loop, restart after a
device-list change, the stalled-audio event); parameter events from a plugin's own window; the
resulting order after track and device moves; VST3 and LV2 through the bridge; calls from
several threads at once. A hung plugin blocks every call: there is no timeout.

## Not tested yet

- Sample-rate and buffer-size changes on the ALSA backend while running; several output
  devices; selecting a device other than the default.
- MIDI input from a hardware device (discovery is tested; receiving notes is not).
- Recording.
- Plugin editors: mouse gestures, resizing.
- Out-of-process scanning for the CLAP format (in-process scan only).
- Plugins with side-chain or multiple output buses in use (extra buses are disabled).
- Windows and macOS.
