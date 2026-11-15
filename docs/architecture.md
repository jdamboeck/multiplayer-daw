# Architecture

Status: 2026-11-15, branch `tracktion-integration` (from `m1-local-daw` at `379f0a1`). The Rust
audio engine is gone (phase 7, `docs/retire-rust-engine.md`): the client plays through Tracktion
only. "What the audit found" is the audit of 2026-11-12, with what became of each point.

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

Rust workspace, edition 2024, and the C++ bridge under `native/`. Linux only for now: the bridge
builds on Linux only, so there is no Windows build until it is ported. The server and the
libraries are GPL-3.0-only; the client and the bridge crates are AGPL-3.0-only, because they
link JUCE 8 (README, "License").

| Part | Lines | What it does |
|---|---|---|
| `mdaw-model` | 2.1k | Project, typed stable IDs (`EntityId = actor << 32 | counter`), `Tick` at 960 PPQ, the `Op` enum and `apply`, the built-in instrument's parameter table (`two_osc.rs`) |
| `mdaw-proto` | 0.4k | Wire messages (protocol version 11), `Txn { id: TxnId, base_seq, ops }`, blobs |
| `mdaw-sync` | 0.4k | Client replica: optimistic ops, rebase on server events |
| `mdaw-server` | 1.5k | Hub, sequencer, SQLite store (format `7/11`), receipts |
| `mdaw-net` | 0.5k | QUIC (quinn) |
| `mdaw-client` | 10.2k | egui app: arrange, piano roll, devices, changelog; the outbox; the engine worker and its scheduler; MIDI input (midir) and the clock for recording |
| `mdaw-tracktion-sys` | 0.4k | Declarations of the bridge's C header; its build script runs the CMake build in `target/native` |
| `mdaw-tracktion` | 2.6k | Safe `Engine`; brings the canonical project into the engine whole or by difference (`sync_project`) |
| `mdaw-test-plugin` | 0.5k | A small, deterministic CLAP synth for the tests (the failure tests use the bridge's C fixtures, `native/tracktion-bridge/test/bad_clap.c`) |
| `native/tracktion-bridge` | 6.6k | The C ABI (`include/mdaw_engine.h`) over Tracktion Engine and JUCE; the plugin scanner `mdaw-plugin-scan` |
| `native/clap-format` | 1.1k | CLAP hosting as a JUCE `AudioPluginFormat` (ADR 0002) |

The model has tracks, MIDI clips, notes, a tempo map, devices with parameters and opaque state.
It has no time signatures, no automation and no audio clips yet.

At the audit (2026-11-12) the workspace also had `mdaw-dsp` (own audio graph, the old Pocket
synth, offline render), `mdaw-engine` (cpal device, real-time thread, transport) and
`mdaw-plugin` (CLAP host, each plugin in a helper process over shared memory); they were removed
on 2026-11-15 (`docs/migration-checklist.md`, "Removed"). Baseline then: `cargo test --workspace
--no-fail-fast` → 237 passed, 0 failed, 9 ignored.

## What the audit found

These are confirmed in the code, not suspected.

1. **A transaction can be applied twice.** `client_txn_id` is a counter per connection. The
   server keeps no record of which transactions it accepted. If the acknowledgement is lost and
   the client reconnects through a snapshot, it resubmits and the server applies the edit again.
   Fixed 2026-11-13: transactions carry a random `TxnId` made once by the client and kept with
   the pending edit, also on disk across client restarts; the server keeps a receipt per (user,
   `TxnId`), written in the same SQLite transaction as the event, and answers a transaction sent
   again with its original outcome (`Accepted` / the same `Rejected`). Tested at the hub, in the
   simulation (duplicates, lost acknowledgements, catch-up and snapshot reconnects, server
   restarts) and with real clients (`mdaw-server/tests/receipts.rs`, `sim.rs`,
   `mdaw-client/tests/net.rs`).
2. **Events are not durable when acknowledged.** `store.rs` opens SQLite with WAL and
   `synchronous=NORMAL`. A power loss can drop events the clients were already told about.
   Fixed 2026-11-13: the store runs with `synchronous=FULL`; the hub commits event and receipt,
   then applies the event in memory and sends it, and a failed commit applies and sends nothing.
   Measured: 0.23 ms p50, 0.5 ms p99 per commit (ext4, NVMe), so no group commit.
3. **The audio thread waits on other processes.** For every isolated plugin, every block,
   `isolated/parent.rs` blocks on a futex until the helper answers. It has a deadline, but a
   blocking wait in the audio callback is the cost of that isolation. Gone with `mdaw-plugin`
   on 2026-11-15: plugins run in process (ADR 0002).
4. **The graph is replaced whole.** Each edit rebuilds the audio graph and sends it to the
   real-time thread (`Command::SetGraph`). Gone: the engine is brought from one state of the
   project to the next by their difference (`mdaw_tracktion::sync_project`).
5. **Machinery for things that are now non-goals.** Shared transport and "follow"
   (`mdaw-proto/src/transport.rs`, hub, session), clock sync (`mdaw-net/src/clock.rs`), live MIDI
   relay (`mdaw-proto/src/midi.rs`, `mdaw-server/src/relay.rs`, `mdaw-client/src/live.rs`).
   Removed 2026-11-13.
6. **Editing a plugin track needs a checkout.** Checkout, lease renewal, force release and
   commit run through proto, sync, server and client (`RefuseReason::PluginOutsideCommit`,
   `StemRequired`). The target has no mandatory checkout for ordinary edits. Removed 2026-11-13.
7. **Two playback implementations would exist** once Tracktion is in: `mdaw-dsp` +
   `mdaw-engine` and Tracktion. The Rust one goes when Tracktion has passed (phase 7), not
   before. Done 2026-11-15: Tracktion is the only engine.

## Collaboration target

- The server holds the canonical project and an ordered event log; clients hold a replica and
  apply their own edits optimistically.
- A transaction is `{ id, base_seq, ops }` (the revision is the log's sequence number). `id`
  (`TxnId`, 128 random bits) is generated once by the client, stored with the pending edit in
  the client's outbox on disk, and survives reconnects and restarts.
- The server stores a **receipt** per (authenticated user, `id`) in the same SQLite
  transaction as the event (the head is the highest seq in the log; it is not stored apart). A
  resubmission returns the stored result (`Accepted { txn, seq }` or the same `Rejected`) and
  applies nothing. Refusals get receipts too: a refused transaction stays refused.
- The event, and any acknowledgement, is sent only after the commit is on disk
  (`synchronous=FULL` for every write; measured in phase 5, no group commit needed).
- Reconnect: the client says its revision; the server sends the events since, or a snapshot.
  All pending transactions are resubmitted, oldest first, with their original IDs; receipts make
  that safe.
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
