# mdaw

Prototype of a server/client DAW for collaborative editing. Working name.

- `docs/pre-plan.md`: product model and decisions so far.
- `docs/plan.md`: prototype implementation plan and milestones.
- `docs/protocol.md`: the wire protocol and sync rules as built.
- `docs/manual-tests/`: test protocols to run through by hand after a milestone.
- `docs/spikes/`: spike reports.

## Status

- **M0** done: workspace, CI and the CLAP hosting spike (`docs/spikes/clap-host.md`).
- **M1** done: single-user local DAW. Arrangement, piano roll, the built-in Pocket synth and undo, with
  every edit already going through a txn on a loopback replica.
- **M2** done: server and live sync. Several clients edit one project over QUIC, see who is there and
  what the others hold, keep working through a lost connection, and can bring back an edit that lost
  against a concurrent one.
- **M3** done: shared clock and transport. Play, stop, seek and loop are for everyone at once, on the
  same sample as far as the clocks allow; a client can unlink to play on its own.
- **M4** (checkout, commit and changelog samples) is next.

## Run

```sh
cargo run -p mdaw-client      # a local project; falls back to a silent engine if no audio device opens
cargo xtask demo              # a server and two clients (Ann, Ben) on this machine
cargo test --workspace --exclude 'spike-*'
```

Double-click an empty lane to create a clip, double-click a clip to edit its notes, Space plays,
Ctrl+Z undoes.

The pieces of the demo by hand, for another machine or a server that outlives its clients:

```sh
cargo run -p mdaw-server -- --data ./mdaw-data --host <name or address others reach you by>
cargo run -p mdaw-client -- --join 'mdaw://...' --name Ann
```

The server prints the join string. `cargo xtask server` and `cargo xtask client <name>` do the same
with everything kept under `target/demo`; `cargo xtask help` lists the options.

The UI tests drive the app headlessly. To render screenshots from them:

```sh
MDAW_SHOTS=/some/dir cargo test -p mdaw-client --test ui -- --ignored screenshots
MDAW_SHOTS=/some/dir cargo test -p mdaw-client --test ui_net -- --ignored screenshots
```
