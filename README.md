# mdaw

Prototype of a server/client DAW for collaborative editing. Working name.

- `docs/pre-plan.md`: product model and decisions so far.
- `docs/plan.md`: prototype implementation plan and milestones.
- `docs/spikes/`: spike reports.

## Status

- **M0** done: workspace, CI and the CLAP hosting spike (`docs/spikes/clap-host.md`).
- **M1** done: single-user local DAW. Arrangement, piano roll, the built-in Pocket synth and undo, with
  every edit already going through a txn on a loopback replica.
- **M2** (server and live sync) is next.

## Run

```sh
cargo run -p mdaw-client      # the DAW; falls back to a silent engine if no audio device opens
cargo test --workspace --exclude 'spike-*'
```

Double-click an empty lane to create a clip, double-click a clip to edit its notes, Space plays,
Ctrl+Z undoes. The UI tests drive the app headlessly; to render screenshots from them:

```sh
MDAW_SHOTS=/some/dir cargo test -p mdaw-client --test ui -- --ignored screenshots
```
