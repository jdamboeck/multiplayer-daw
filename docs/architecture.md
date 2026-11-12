# Architecture

Status: 2026-11-12, branch `tracktion-integration` (from `m1-local-daw` at `379f0a1`).

The guiding rule: **collaborate on the project, not on the audio clock.** People share a
project — tracks, clips, notes, instruments, plugin settings, automation, tempo, audio files.
Each person plays it back on their own machine with their own engine. Nothing about playback
crosses the network.

This document describes where the code is today, where it is going, and what the audit of the
existing code found. The decisions behind the target are in `docs/decisions/`; the engine work
is in `docs/tracktion-integration.md`; what happens to each existing part is in
`docs/migration-checklist.md`.

## Target

```
            ┌────────────────────────── client process ──────────────────────────┐
            │  egui UI ──► ProjectOperation ──► transaction ──► canonical project  │
            │     ▲                                  │               │ (Rust)      │
            │     │                                  ▼               ▼             │
            │  meters, playhead            sync (QUIC, revisions)  derive/apply    │
            │     ▲                                  │               │ C ABI       │
            │     │                                  │               ▼             │
            │  mdaw-tracktion (safe Rust) ◄── mdaw-tracktion-sys ── tracktion-bridge│
            │                                        │          (C++, message thr.)│
            │                                        │               ▼             │
            │                                        │      Tracktion Engine + JUCE│
            │                                        │      devices, plugins, audio│
            └────────────────────────────────────────┼─────────────────────────────┘
                                                     ▼
                                   server: canonical state, event log,
                                   receipts, assets (Rust only, no audio)
```

- **Rust** owns the application: UI, the canonical project, stable IDs, transactions,
  optimistic edits, revisions, conflict policy, sync, auth, persistence, assets.
- **Tracktion Engine** (C++/JUCE) owns sound on each client: devices, transport, MIDI and audio
  clips, recording, plugin hosting, routing, automation playback, offline render. The server
  never links it.
- **The bridge** is a small C ABI: opaque handles, plain data, no exceptions across it. All
  mutations go to the engine's message thread. Rust is never called from the audio callback;
  the audio thread only writes meters and positions into atomics that Rust polls.
- **One authority.** The Rust project is canonical; the Tracktion Edit is derived from it and
  never saved as the project (ADR 0001).

## Today

Rust workspace, edition 2024, GPL-3.0-only.

| Crate | Lines | What it does |
|---|---|---|
| `mdaw-model` | 1.7k | Project, typed stable IDs (`EntityId = actor << 32 | counter`), `Tick` at 960 PPQ, the `Op` enum and `apply` |
| `mdaw-proto` | 1.4k | Wire messages (protocol version 5), `Txn { client_txn_id, base_seq, ops }`, blobs, live MIDI packets, transport |
| `mdaw-sync` | 0.7k | Client replica: optimistic ops, rebase on server events, checkout workspaces |
| `mdaw-server` | 1.9k | Hub, sequencer, SQLite store, checkout/lease bookkeeping, live-MIDI relay |
| `mdaw-net` | 0.6k | QUIC (quinn), clock sync |
| `mdaw-dsp` | 1.7k | Own audio graph, built-in synth, offline render, plugin slots |
| `mdaw-engine` | 1.6k | cpal device, real-time thread, transport |
| `mdaw-plugin` | 3.3k | CLAP host (clack), each plugin in a helper process over shared memory |
| `mdaw-client` | 8.9k | egui app: arrange, piano roll, devices, checkout UI, live jam, changelog |
| `mdaw-test-plugin` | 0.5k | CLAP synth that can fail, stall, hang or crash on request |

Baseline before any change: `cargo test --workspace --no-fail-fast` → 237 passed, 0 failed,
9 ignored.

The model has tracks, MIDI clips, notes, a tempo map, devices with parameters and opaque state.
It has no time signatures, no automation and no audio clips yet.

## What the audit found

These are confirmed in the code, not suspected.

1. **A transaction can be applied twice.** `client_txn_id` is a counter per connection. The
   server keeps no record of which transactions it accepted. If the acknowledgement is lost and
   the client reconnects through a snapshot, it resubmits and the server applies the edit again.
2. **Events are not durable when acknowledged.** `store.rs` opens SQLite with WAL and
   `synchronous=NORMAL`. A power loss can drop events the clients were already told about.
3. **The audio thread waits on other processes.** For every isolated plugin, every block,
   `isolated/parent.rs` blocks on a futex until the helper answers. It has a deadline, but a
   blocking wait in the audio callback is the cost of that isolation.
4. **The graph is replaced whole.** Each edit rebuilds the audio graph and sends it to the
   real-time thread (`Command::SetGraph`).
5. **Machinery for things that are now non-goals.** Shared transport and "follow"
   (`mdaw-proto/src/transport.rs`, hub, session), clock sync (`mdaw-net/src/clock.rs`), live MIDI
   relay (`mdaw-proto/src/midi.rs`, `mdaw-server/src/relay.rs`, `mdaw-client/src/live.rs`).
6. **Editing a plugin track needs a checkout.** Checkout, lease renewal, force release and
   commit run through proto, sync, server and client (`RefuseReason::PluginOutsideCommit`,
   `StemRequired`). The target has no mandatory checkout for ordinary edits.
7. **Two playback implementations would exist** once Tracktion is in: `mdaw-dsp` +
   `mdaw-engine` and Tracktion. The Rust one goes when Tracktion has passed (phase 7), not
   before.

## Collaboration target

- The server holds the canonical project and an ordered event log; clients hold a replica and
  apply their own edits optimistically.
- A transaction is `{ txn_id, base_revision, ops }`. `txn_id` is generated once by the client,
  stored with the pending edit, and survives reconnects and restarts.
- The server stores a **receipt** per (authenticated user, `txn_id`) in the same SQLite
  transaction as the event and the new revision. A resubmission returns the stored result and
  applies nothing.
- The acknowledgement is sent only after the commit is on disk (`synchronous=FULL` for the
  event path, to be measured in phase 5).
- Reconnect: the client says its revision; the server sends the events since, or a snapshot.
  Pending transactions are resubmitted with their original IDs; receipts make that safe.
- Conflicts are decided per operation: last writer wins on independent properties; note
  properties are independent of each other; an update to something deleted is dropped and can
  never recreate it; structural edits that cannot be ordered unambiguously are rejected and the
  client rebases.
- No CRDTs. No checkout for ordinary edits.

## Engine target

See `docs/tracktion-integration.md` for what was built and measured. In short: one Tracktion
`Engine` and one `Edit` per client, built from the canonical project and kept in step by
applying each accepted operation to the entity it maps to. Editing while playing does not
interrupt playback (measured). Playback state — playing, position, loop — is local and never
enters the project.
