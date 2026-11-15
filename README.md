# mdaw

Prototype of a server/client DAW for collaborative editing. Working name.

- `docs/pre-plan.md`: product model and decisions so far.
- `docs/plan.md`: prototype implementation plan and milestones.
- `docs/protocol.md`: the wire protocol and sync rules as built.
- `docs/manual-tests/`: test protocols to run through by hand after a milestone.
- `docs/spikes/`: spike reports.
- `docs/architecture.md`: the shape of the project — Tracktion Engine for local audio, Rust for
  everything shared — and what the audit of 2026-11-12 found.
- `docs/tracktion-integration.md`: pinned versions, how to build the native part, test results.
- `docs/decisions/`: decision records. `docs/migration-checklist.md`: what is kept and what goes.

## Status

- **M0** done: workspace, CI and the CLAP hosting spike (`docs/spikes/clap-host.md`).
- **M1** done: single-user local DAW. Arrangement, piano roll, the built-in Pocket synth and undo, with
  every edit already going through a txn on a loopback replica.
- **M2** done: server and live sync. Several clients edit one project over QUIC, see who is there and
  what the others hold, keep working through a lost connection, and can bring back an edit that lost
  against a concurrent one.
- **M3** done: shared clock and transport. Play, stop, seek and loop are for everyone at once, on the
  same sample as far as the clocks allow; a client can unlink to play on its own.
- **M4** done: checkout, commit and changelog samples. A track can be checked out and worked on
  alone, then committed with rendered samples that everyone can play from the changelog; tempo
  changes during a checkout can be undone by the commit, and a checkout someone else ends leaves
  its owner a fork.
- **M5** done: live MIDI. Notes from the computer keyboard or a MIDI device are heard by everyone
  as they arrive; recording turns them into a clip. A checked-out track hears only its owner.
- **M6** done: plugins and stems. CLAP plugins go onto a track through a checkout; the commit
  carries their state and a stem. Clients with the plugin run it, the others play the stem. The
  host sits behind a format-neutral interface, so VST3 can be added as another backend.

M3 to M6 describe history. On branch `tracktion-integration` (2026-11-13) the shared transport
and clock sync, live MIDI over the network, checkout/commit with changelog samples, and stems
were removed: people collaborate on the project, not on the audio clock. Every edit, plugins
included, is an ordinary transaction; each client plays the project on its own; a client
without a plugin hears nothing from that track and says so.

On 2026-11-15 the Rust audio engine (`mdaw-engine`, `mdaw-dsp`, `mdaw-plugin`) was removed: the
client plays through Tracktion Engine and hosts VST3, LV2 and CLAP plugins, examined in a separate
scanner process (`docs/retire-rust-engine.md`). Data directories, projects and unsent edits of
earlier versions are not readable (protocol 11, store 7).

**Linux only.** The bridge to Tracktion builds on Linux only so far; there is no Windows build
until it is ported.

## Build

The client links Tracktion Engine and JUCE through a C++ bridge (`native/`), so the build needs
CMake, Ninja, a C++20 compiler, the system packages listed in `docs/tracktion-integration.md`
("Build", for Arch and Ubuntu) and the pinned submodules:

```sh
git -c url."https://github.com/".insteadOf="git@github.com:" submodule update --init --recursive --depth 1
cargo build
```

`cargo` builds the native part itself, in `target/native` (the first time takes about ten
minutes; afterwards it is incremental). The Tracktion spike, a native test program:

```sh
cmake --build target/native --target mdaw-te-spike
S=target/native/spike/mdaw-te-spike_artefacts/RelWithDebInfo/mdaw-te-spike
$S play                     # eight seconds of MIDI through the default audio device
$S render --out demo.wav    # the same, rendered to a file without a device
```

## Downloads

Releases on GitHub carry ready-made builds for Linux x86-64: the client (`mdaw`), the server
(`mdaw-server`) and the plugin scanner (`mdaw-plugin-scan`, which must stay next to `mdaw`). The
client needs the ALSA, X11 (or Wayland), freetype and fontconfig libraries, which desktop distributions have;
JACK is used if it is there.

## Run

```sh
cargo run -p mdaw-client      # a local project; runs without sound if the engine does not start
cargo xtask demo              # a server and two clients (Ann, Ben) on this machine
cargo test --workspace        # the tests; those that need windows or an audio device are ignored
```

The client hosts VST3, LV2 and CLAP plugins (scanned at startup, in child processes, by
`mdaw-plugin-scan` next to `mdaw`) and keeps its engine settings in `<profile>/engine/`.
`--no-plugins` starts one without plugins, in which their tracks are silent; `cargo xtask demo
--test-synth` puts the test synth on the clients' `CLAP_PATH`.

Double-click an empty lane to create a clip, double-click a clip to edit its notes, Space plays,
Ctrl+Z undoes.

The pieces of the demo by hand, for another machine or a server that outlives its clients:

```sh
cargo run -p mdaw-server -- --data ./mdaw-data --host <name or address others reach you by>
cargo run -p mdaw-client -- --join 'mdaw://...' --name Ann
```

With the release builds, the same is `mdaw-server --data ./mdaw-data --host <name>` and
`mdaw --join 'mdaw://...' --name Ann`.

The server prints the join string. `cargo xtask server` and `cargo xtask client <name>` do the same
with everything kept under `target/demo`; `cargo xtask help` lists the options.

Tests that open plugin windows run on a virtual X display (needs `xvfb-run` and `xprop`), never
on the desktop:

```sh
native/tracktion-bridge/test/run_gui_tests.sh                 # the bridge's C test, with windows
native/tracktion-bridge/test/run_gui_tests.sh cargo test -p mdaw-client --test editor_gui -- --ignored
```

The bridge's C test without windows, and how it finds the test synth: `docs/tracktion-integration.md`,
"The bridge".

The UI tests drive the app headlessly. To render screenshots from them:

```sh
MDAW_SHOTS=/some/dir cargo test -p mdaw-client --test ui -- --ignored screenshots
MDAW_SHOTS=/some/dir cargo test -p mdaw-client --test ui_net -- --ignored screenshots
```

## License

Copyright (C) 2026 the mdaw authors.

This program is free software: you can redistribute it and/or modify it under the terms of the GNU
General Public License, version 3, as published by the Free Software Foundation. It is distributed
in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.

Which licence applies to what:

- The server (`mdaw-server`) and the libraries (`mdaw-model`, `mdaw-proto`, `mdaw-sync`,
  `mdaw-net`, `mdaw-test-plugin`, `xtask`): GPL-3.0-only, [LICENSE](LICENSE).
- The client (`mdaw-client`, the `mdaw` executable and `mdaw-plugin-scan`) and the bridge crates
  (`mdaw-tracktion`, `mdaw-tracktion-sys`, with `native/`): AGPL-3.0-only,
  [LICENSE-AGPL](LICENSE-AGPL). They link JUCE 8, which is AGPLv3 (or commercial), and Tracktion
  Engine, which is GPLv3 (or commercial).

The release archives carry both texts.
