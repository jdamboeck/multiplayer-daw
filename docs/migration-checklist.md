# Migration checklist

What happens to each existing part. The rows of the "Remove" table are done (2026-11-13, see
"Removed" below); everything else goes only when the thing that replaces it has passed its tests. Order follows the phases in
the mission: bridge (4), collaborative slice (5), reliability (6), retire the Rust engine (7).

## Keep

| Part | Why |
|---|---|
| `mdaw-model`: IDs, ticks, `Op`, `apply` | the canonical project (ADR 0001) |
| `mdaw-sync` replica: optimistic apply and rebase | the collaboration core |
| `mdaw-server`: hub, sequencer, SQLite store, users and tokens | stays Rust-only |
| `mdaw-net`: QUIC transport | unchanged |
| `mdaw-proto`: framing, hello/welcome, submit, events, snapshots, blobs | unchanged in shape |
| Blob store (content-addressed) | becomes the asset store for audio files and large plugin state |
| `mdaw-client`: egui app, arrange, piano roll, device strip, undo | the UI |
| `mdaw-test-plugin` | the misbehaving-plugin tests use it against Tracktion too |
| `xtask` demo and release tooling | gains the native build step |

## Replace

| Part | With | When |
|---|---|---|
| `mdaw-engine` (cpal device, real-time thread, transport) | Tracktion through `mdaw-tracktion` | phase 7, after the bridge passes |
| `mdaw-dsp` graph, Pocket synth, offline render | Tracktion graph, a Tracktion built-in or bundled instrument, Tracktion render | phase 7 |
| `mdaw-plugin` CLAP host (clack) | `native/clap-format` | phase 7, after the manual test (ADR 0002) |
| `mdaw-plugin` isolation helper | in-process hosting plus the failure policy in ADR 0002; later an optional isolated mode whose host side is C++ and whose helper is the existing Rust process | the helper stays until that isolated mode exists |
| `client_txn_id` per connection | stable transaction ID, receipts by user + ID | phase 5 |
| `synchronous=NORMAL` on the event path | commit on disk before the acknowledgement | phase 5 |
| Whole-graph replacement on every edit | per-operation updates to the Edit | phase 4 |

## Merge / rework

| Part | Into |
|---|---|
| Stems for clients without a plugin | removed 2026-11-13 with checkout; a missing plugin is silent and shown as "plugin not installed". An optional fallback may come back later |
| Changelog with rendered samples | done 2026-11-13: samples and commit entries removed; the panel lists edits displaced by concurrent ones, with Restore. A full history view over revisions is still to come |
| Device model (`DeviceAdd`, `DeviceSetParam`, `DeviceSetState`) | gains plugin format + identifier, parameters by plugin parameter ID, state as blob hash |
| Tempo map | gains time signatures; maps to Tracktion's `TempoSequence` |
| Model | gains automation and audio clips |
| Presence | done 2026-11-13: `Presence { editing }` only; the playhead field is gone |

## Remove

All done on 2026-11-13 (branch `tracktion-integration`, protocol version 6).

| Part | Where | Why |
|---|---|---|
| Shared transport, "follow", unlink | `mdaw-proto/src/transport.rs`, `ClientMsg::SetTransport`, `ServerMsg::Transport`, hub, `session.rs`, `ui/transport.rs`, `link.rs` | playback is local |
| Clock sync | `mdaw-net/src/clock.rs` | no shared clock |
| Live MIDI relay and jam | `mdaw-proto/src/midi.rs`, `mdaw-server/src/relay.rs`, `mdaw-client/src/live.rs`, `ui/live.rs` | no network MIDI; local MIDI input and recording move to Tracktion |
| Mandatory checkout, lease renewal, force release, commit | `ClientMsg::{Checkout, LeaseRenew, Release, ForceRelease, Commit, Attach}`, refusals `CheckedOut`, `PluginTrack`, `PluginOutsideCommit`, `NotOwner`, `NotCheckedOut`, `OutOfScope`, `StemRequired`; `mdaw-sync` workspaces; server lease code; `mdaw-client/src/checkout.rs`, `ui/checkout.rs` | ordinary editing needs no checkout |
| `spikes/clap-host` | | superseded; `docs/spikes/clap-host.md` stays as history |
| Tests of the above | `tests/checkout.rs`, `tests/live.rs`, `tests/transport.rs`, parts of `tests/hub.rs`, `ui_net.rs` | go with the feature |

Removing these changed the wire protocol: version 6. Data directories of earlier servers are
not readable (store format `2/6`).

## Removed (2026-11-13)

- Shared transport, follow and Link; clock sync (client and server); the datagram path.
- Live MIDI relay and receiving others' notes. Playing one's own instrument from the keyboard or
  a MIDI device, and recording it into a clip, stay local.
- Checkout, lease renewal, force release, commit, attach; `mdaw-sync` workspaces; forks; the
  checkout UI and commit dialog.
- Stems, changelog samples and their preview playback; FLAC encoding and decoding in
  `mdaw-engine`.
- `spikes/clap-host`.
- Their tests.

## Capabilities that change — nothing is dropped silently

| Today | After | 
|---|---|
| A crashing or hung CLAP plugin takes down only its helper | it takes down or stalls the app; mitigations in ADR 0002 |
| Everyone hears the same playhead | each person has their own |
| Others hear your MIDI keyboard live | they hear it when you have recorded a clip |
| CLAP only | VST3, LV2 and CLAP |
| No latency compensation | Tracktion's |
