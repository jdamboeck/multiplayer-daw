# mdaw

Prototype of a server/client DAW for collaborative editing. Working name.

- `docs/protocol.md`: the wire protocol and sync rules as built.
- `docs/manual-tests/`: test protocols to run through by hand after a milestone.
- `docs/architecture.md`: the shape of the project — Tracktion Engine for local audio, Rust for
  everything shared — and what the audit of 2026-11-12 found.
- `docs/tracktion-integration.md`: pinned versions, how to build the native part, test results.
- `docs/decisions/`: decision records. `docs/migration-checklist.md`: what is kept and what goes.

## Status

Several clients edit one project over QUIC; each plays it on its own machine through Tracktion
Engine and hosts VST3, LV2 and CLAP plugins. Linux only. How the pieces fit together:
`docs/architecture.md`.

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
minutes; afterwards it is incremental).

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

Copyright (C) 2026 the mdaw authors. Two licences apply, by component:

- The client (`mdaw-client`, the `mdaw` executable and `mdaw-plugin-scan`) and the bridge crates
  (`mdaw-tracktion`, `mdaw-tracktion-sys`, with `native/`): the GNU Affero General Public
  License, version 3 only (AGPL-3.0-only), [LICENSE-AGPL](LICENSE-AGPL). They link JUCE 8,
  which is AGPLv3 (or commercial), and Tracktion Engine, which is GPLv3 (or commercial).
- Everything else: the server (`mdaw-server`) and the libraries (`mdaw-model`, `mdaw-proto`,
  `mdaw-sync`, `mdaw-net`, `mdaw-test-plugin`, `xtask`): the GNU General Public License,
  version 3 only (GPL-3.0-only), [LICENSE](LICENSE).

Each is free software: you can redistribute it and/or modify it under the terms of its licence,
as published by the Free Software Foundation. It is distributed in the hope that it will be
useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE. The release archives carry both texts.
