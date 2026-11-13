# mdaw

Prototype of a server/client DAW for collaborative editing. Working name.

- `docs/pre-plan.md`: product model and decisions so far.
- `docs/plan.md`: prototype implementation plan and milestones.
- `docs/protocol.md`: the wire protocol and sync rules as built.
- `docs/manual-tests/`: test protocols to run through by hand after a milestone.
- `docs/spikes/`: spike reports.
- `docs/architecture.md`: where the project is going — Tracktion Engine for local audio, Rust for
  everything shared — and what the audit of the current code found.
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

**In progress** (branch `tracktion-integration`): moving local audio to Tracktion Engine. So far
a native spike under `native/`, a C bridge with its Rust side, and the documents above.

## Native build (Tracktion spike)

Needs CMake, Ninja, a C++20 compiler and the packages listed in `docs/tracktion-integration.md`.

```sh
git -c url."https://github.com/".insteadOf="git@github.com:" submodule update --init --recursive --depth 1
cmake -S native -B target/native -G Ninja -DCMAKE_BUILD_TYPE=RelWithDebInfo
cmake --build target/native --target mdaw-te-spike
S=target/native/spike/mdaw-te-spike_artefacts/RelWithDebInfo/mdaw-te-spike
$S play                     # eight seconds of MIDI through the default audio device
$S render --out demo.wav    # the same, rendered to a file without a device
```

The Rust build and tests do not depend on it yet.

## Downloads

Releases on GitHub carry ready-made builds of the client (`mdaw`) and the server (`mdaw-server`)
for Linux x86-64 and Windows x86-64. The Linux build needs PipeWire and ALSA libraries
(`libpipewire-0.3`, `libasound2`), which desktop distributions have.

## Run

```sh
cargo run -p mdaw-client      # a local project; falls back to a silent engine if no audio device opens
cargo xtask demo              # a server and two clients (Ann, Ben) on this machine
cargo test --workspace --exclude mdaw-tracktion --exclude mdaw-tracktion-sys
```

Double-click an empty lane to create a clip, double-click a clip to edit its notes, Space plays,
Ctrl+Z undoes.

Clients load the CLAP plugins installed in the usual folders (and in `CLAP_PATH`), plus a
built-in test synth; `--no-plugins` starts one without plugins, in which their tracks are
silent. On Linux every plugin runs in a helper process of its own (`mdaw-plugin-host`, which must
sit next to `mdaw`), so a plugin that crashes or hangs takes only itself down;
`MDAW_PLUGINS_IN_PROCESS=1` runs them inside `mdaw` instead.

The pieces of the demo by hand, for another machine or a server that outlives its clients:

```sh
cargo run -p mdaw-server -- --data ./mdaw-data --host <name or address others reach you by>
cargo run -p mdaw-client -- --join 'mdaw://...' --name Ann
```

With the release builds, the same is `mdaw-server --data ./mdaw-data --host <name>` and
`mdaw --join 'mdaw://...' --name Ann`.

The server prints the join string. `cargo xtask server` and `cargo xtask client <name>` do the same
with everything kept under `target/demo`; `cargo xtask help` lists the options.

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
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See [LICENSE](LICENSE) for the full text.
