# Multiplayer DAW: Prototype Implementation Plan

## Context

The pre-plan (`13ae8541-pre-plan.md`) fixes the product model: a server/client DAW for collaborative editing, with per-track live and async (checkout/commit) modes, a changelog with listening samples, live MIDI, and deterministic built-ins next to local CLAP plugins. It leaves the sync protocol, op format and UI technology open.

This plan turns that into a buildable prototype covering all six items of pre-plan section 11. It closes the open questions with concrete decisions, defines the document model, op format and wire protocol, and splits the work into milestones that each end in something runnable. The repo is greenfield (`/home/data/lab/mine/multiplayer-daw` is empty; Rust 1.97 is installed).

Decided with the user: **egui** for the UI, and the plan covers the **whole prototype slice**.

## Decisions closing the pre-plan's open questions

| Question | Decision for the prototype |
|---|---|
| UI | egui/eframe 0.37, in-process. UI only reads an immutable project snapshot and emits intents, so it can be swapped later. |
| Transport | QUIC (quinn 0.11) for everything: one reliable control stream, one stream per blob transfer, datagrams for live MIDI and clock pings. |
| Serialization | serde + postcard, length-prefixed frames, `proto_version` in `Hello`. A breaking change means wiping the server DB (schema version checked on open). |
| Sync model | One totally ordered **event log** per project. Document state is a fold over events; the changelog is a view of the same log. |
| Readers and commits | Auto-follow: a commit is truth for everyone immediately. |
| Checkpoint | Not built. |
| Permissions | None. Anyone holding the session token is equal, including force-release. |
| Determinism | Built-in synth is bit-exact per sample rate on x86-64 Linux and Windows, enforced by a golden-hash test. Each built-in carries a `dsp_version`; behaviour changes need a new version. |
| Audio codec | FLAC (pure Rust: `flacenc` to encode, `claxon` to decode; built in M4 after `symphonia` 0.5 refused to probe `flacenc` output that `ffprobe` reads fine). 16-bit for changelog samples, 24-bit for stems, both 48 kHz stereo. Opus is a later size optimisation. |
| Server storage | SQLite (`rusqlite`, bundled) for events and snapshots; blobs on disk addressed by BLAKE3 hash. |
| Audio I/O | cpal 0.18: `pipewire` and `jack` features on Linux, WASAPI on Windows. |
| Plugin hosting | `clack-host` + `clack-extensions` 0.2 (both on crates.io). Spiked first; JUCE engine remains the fallback. |

Two interpretations of the pre-plan that you should confirm or correct:

1. **Async conflict ("your version" vs "current project state").** The checkout owner's workspace follows live project state. The client keeps the project snapshot from checkout time. If project state relevant to the track changed since (in the prototype: tempo points inside the track's extent), the commit dialog renders both variants and offers *Commit as is* or *Commit and restore the old tempo*. The second is an ordinary project-level edit bundled atomically into the commit.
2. **Tracks with CLAP devices are edited only through checkout**, by a client that has the plugins. The server rejects live content edits on them and requires a stem with every commit. This keeps the invariant "stem matches committed content". A later tempo change marks the stem stale in the UI but still plays it.

Out of scope: audio clips and recording audio, automation lanes, sends and routing beyond track to master, VST3, direct p2p, audio streaming, accounts, reverting a track to an arbitrary old version.

## Workspace layout

Cargo workspace, edition 2024, working name `mdaw`.

| Crate | Purpose | Key dependencies |
|---|---|---|
| `mdaw-model` | IDs, document, ops, `apply`/inverse, affected-range diff. Pure, no I/O. | `imbl`, `serde` |
| `mdaw-proto` | Wire messages, events, framing codec. | `postcard`, `blake3` |
| `mdaw-sync` | Client replica state machine (confirmed + pending = view), checkout workspace, undo. Pure: messages in, messages out. | model, proto |
| `mdaw-dsp` | Built-in synth "Pocket", tempo-map compiler, graph `process`, offline render. No device I/O. | `libm` |
| `mdaw-engine` | cpal stream, RT thread, transport, preview bus, FLAC encode/decode, MIDI input. | `cpal`, `rtrb`, `flacenc`, `claxon`, `midir` |
| `mdaw-plugin` | Plugin hosting behind a format-neutral interface (`Backend`, `Instance`, `mdaw_dsp::ExternalDevice`); CLAP is the first backend. | `clack-*`, `x11rb`, `windows-sys` |
| `mdaw-net` | quinn setup, cert pinning, frame streams, blob transfer, clock sync. | `quinn`, `rustls`, `rcgen`, `tokio` |
| `mdaw-server` | Binary: project actor, sequencing, persistence, blob store, leases, MIDI relay. | `rusqlite`, net, proto, model |
| `mdaw-client` | Binary `mdaw`: egui app, controller, glue. Also exposes a headless client for tests. | `eframe`, sync, engine, net |
| `mdaw-test-plugin` | Tiny deterministic CLAP instrument for headless host tests. | `clack-plugin` |

Also: `docs/pre-plan.md` (copied in), `docs/protocol.md` (the spec below, kept current), `xtask` or a `Makefile` for the demo commands, GitHub Actions matrix (ubuntu, windows) running build and tests.

## Document model (`mdaw-model`)

**IDs.** The server hands each connection a fresh, never-reused `ActorId(u32)`. Clients mint `EntityId(u64) = actor << 32 | counter` with no coordination. Typed wrappers: `TrackId`, `ClipId`, `NoteId`, `DeviceId`, `TempoId`. `UserId(u32)` identifies a client install (random key stored in client config) and survives reconnects; checkouts are owned by `UserId`. `BlobHash([u8; 32])` is BLAKE3.

**Time.** `Tick(i64)` at 960 PPQ. Notes are relative to their clip. Tempo map is a list of constant-tempo points.

```rust
Project { name, tempo: OrdMap<TempoId, TempoPoint{tick, milli_bpm}>, master_gain,
          track_order: Vector<TrackId>, tracks: OrdMap<TrackId, Arc<Track>> }
Track   { shell: {name, color},                       // project-level, always live
          content: Arc<TrackContent>,                 // locked by checkout
          checkout: Option<{user, base_seq, lease_at}>,
          stem: Option<{blob, tempo_hash, len_samples}> }
TrackContent { gain, pan, mute, clips: OrdMap<ClipId, Arc<Clip>>, devices: Vector<Device> }
Clip    { start, length, name, notes: OrdMap<NoteId, Note{start, dur, pitch, vel}> }
Device  { id, kind: Builtin{kind, dsp_version} | Clap{plugin_id, state: Option<BlobHash>},
          params: OrdMap<u32, f64>, bypass }
```

`imbl` persistent maps give O(1) snapshots and deterministic iteration order. Solo is local view state, not synced.

**Ops** are property-level and target stable IDs:

- Project scope: `ProjectSetName`, `MasterSetGain`, `TempoPut`, `TempoRemove`, `TrackAdd{id, shell, after}`, `TrackMove`, `TrackSetShell`.
- Track structure: `TrackRemove`.
- Track content: `TrackSetMix`, `ClipAdd`, `ClipRemove`, `ClipSet{prop}`, `NoteAdd`, `NoteRemove`, `NoteSet{prop}`, `DeviceAdd`, `DeviceRemove`, `DeviceMove`, `DeviceSetParam`, `DeviceSetState`.

Each op reports `scope() -> Project | TrackStructure(id) | TrackContent(id)`.

`apply(&mut Project, &Op) -> Applied{inverse: Option<Op>}` is total and deterministic. An op on a missing entity, or an add with an existing ID, is a no-op on every replica. Permission and lock checks happen only on the server before sequencing.

A **`Txn { client_txn_id, base_seq, ops }`** is one user gesture, atomic, and the undo unit. `base_seq` is the last server seq the client had applied.

`affected_ranges(before, ops) -> Vec<(TrackId, TickRange)>` lives here so client (for rendering) and server (for the entry) compute the same answer.

## Event log and wire protocol (`mdaw-proto`)

```rust
Event { seq: u64, actor, user, wall_ts, body }
EventBody = Txn{client_txn_id, ops, superseded: Vec<Superseded>}
          | Checkout{track}
          | Commit{track, ops, project_fix: Vec<Op>, stem, entry: ChangelogEntry}
          | Release{track, reason: Abandoned | Forced{by}}
          | Attach{entry_seq, sample}                       // late sample for an entry
ChangelogEntry { summary, ranges, samples: Vec<Sample{kind, blob, range, ms}> }
Sample.kind = Solo | InContext | Before | ProjectBase | ProjectCurrent
```

**Connection.** ALPN `mdaw/0`. The server creates a self-signed cert on first run and prints a join string `mdaw://host:port?fp=<sha256>&token=<secret>`; the client pins the fingerprint.

**Control stream** (client-opened bidi stream, `u32` length + postcard, 16 MiB cap):

| Client to server | Server to client |
|---|---|
| `Hello{proto_version, token, user_key, name, resume: Option<{prev_actor, session_secret, last_seq}>}` | `Welcome{actor, user, session_secret, head_seq, sync: Snapshot(Project) \| Events(Vec<Event>), peers, transport}` |
| `Submit(Txn)` | `Event(Event)` (the only way the document changes) |
| `CheckoutRequest{track}`, `LeaseRenew{track}`, `Release{track}`, `ForceRelease{track}` | `Rejected{client_txn_id \| request, reason}` |
| `Commit{track, ops, project_fix, stem, entry}` | `BlobStored{hash}`, `BlobMissing{hashes}` |
| `BlobQuery{hashes}`, `Attach{entry_seq, sample}` | `Peers(...)`, `Presence{actor, ...}` relay |
| `Presence{editing: Option<EntityRef>, playhead}` (10 Hz max) | `Transport(TransportState)` |
| `SetTransport(TransportState)` | |

**Blob streams.** One stream per transfer: `Put{hash, len}` + bytes, or `Get{hash}` answered with `len` + bytes. The server verifies the hash. Large uploads never block ops.

**Datagrams.** `Ping{t0}` / `Pong{t0, server_micros}` and `Midi{...}` (see M5).

**Server sequencing rules**, run in a single project thread:

1. Reject the whole txn if any op is `TrackContent(t)` and `t` is checked out (by anyone) or contains a CLAP device, or is `TrackStructure(t)` and `t` is checked out by another user. CLAP `DeviceAdd` is valid only inside a `Commit`.
2. Apply to the server's document.
3. Scan events in `(base_seq, head]` for writes by other actors to the same `(entity, prop)`, or deletes of an entity this txn edits. Record each as `Superseded{entity, prop, lost_value, lost_actor, lost_seq}`; this is the data behind one-click restore.
4. Append to SQLite (WAL), then broadcast. Slow consumers are disconnected and resume later.

`Commit` is accepted only from the checkout owner, with all referenced blobs present and a stem if the track has CLAP devices. It is a fast-forward by construction.

**Client replica (`mdaw-sync`).** Keeps `confirmed` (state at server seq N) and `pending` (unacked txns); `view = confirmed + pending`. On each event: apply to `confirmed`, pop the matching pending txn if it is ours, rebuild `view`, emit the set of changed tracks for the engine. A `Rejected` drops the pending txn and surfaces a notice.

**Reconnect.** `Hello.resume` names the previous actor and last seq. The server closes the old session, then answers with the missing events (or a snapshot). The client pops pending txns it finds in the catch-up and resubmits the rest under its new actor.

**Checkout workspace.** On its own `Checkout` event the client stores base content plus a local op journal on disk; its view substitutes `base + local ops` for that track. Local ops never go on the wire until `Commit`. If the checkout is force-released, the journal becomes a local fork file that can be restored as a new track.

**Leases.** `LeaseRenew` every 10 s. After 60 s without one the checkout shows as stale. Nothing auto-releases; any user can force-release.

**Undo.** Per-client stack of inverse txns. Undo submits the inverse as a normal txn; an inverse `Set` is skipped if the property no longer holds the value this client wrote.

**Transport.** Session state, not part of the document: `TransportState{playing, anchor_tick, anchor_server_micros, loop, epoch}`. Any client sets it; play is scheduled 150 ms ahead. When a tempo op lands during playback, the server re-anchors at the current position. Each client has a *Link* toggle; unlinked clients run a private transport.

## Engine design (`mdaw-dsp`, `mdaw-engine`)

- Threads: cpal callback (no allocation, no locks), UI, tokio network runtime, render worker.
- `rtrb` rings carry commands into the callback (transport, live notes, swapped track data) and playhead, meters and garbage back out.
- Per-track compiled data (sorted note events, device params) is rebuilt off-thread when `mdaw-sync` reports a changed track and swapped in whole.
- Events are applied at exact sample offsets by splitting blocks, so output does not depend on buffer size. The same `Graph::process` runs live and offline.
- `render_offline(project, tracks, tick_range, 48_000) -> Vec<f32>` builds a fresh graph, pre-rolls up to 2 s (discarded), and adds a 1 s tail.
- **Pocket synth v1:** 16 voices, two polyBLEP oscillators, ADSR, state-variable low-pass with envelope, oldest-voice stealing. Determinism rules: `f32` with basic operators and `libm` functions only, no platform `sin`/`exp`, explicit denormal flushing, seeded per-voice PRNG, no SIMD intrinsics.
- **Sample range:** union of affected ranges snapped out to bars, first 8 bars at most; for non-temporal changes, the first 4 bars that contain notes.

## Milestones

Each milestone ends with a demo and its tests green on Linux; CI also builds and tests on Windows.

### M0: Bootstrap and CLAP spike

- `git init`, workspace skeleton, CI, copy the pre-plan into `docs/`.
- **Spike S1** (time-boxed, throwaway binary `spikes/clap-host`): scan `~/.clap`, `/usr/lib/clap`, `CLAP_PATH` and the Windows CLAP folders; load Vital (installed locally) and one effect from `~/.clap`; play notes through cpal; save and reload state; run a second instance offline with the same state; open the plugin GUI.
- GUI order of attempts, since the session is Wayland and most Linux plugins only embed into X11: floating window via the CLAP gui extension, then a host-created X11 parent window (`x11rb`, through XWayland), with Win32 parenting on Windows.
- **Exit decision:** continue with clack, or switch the engine plan to JUCE. Record the outcome in `docs/spikes/clap-host.md`.

### M1: Single-user local DAW

- `mdaw-model` complete with property tests: inverse round-trips, apply is total, replicas fed the same txn order converge.
- `mdaw-dsp` with Pocket, tempo compiler, offline render; golden BLAKE3 test of a fixed MIDI sequence.
- `mdaw-engine` playback through cpal with a local transport.
- `mdaw-client` UI: transport bar, track list, arrangement timeline (clip add, move, resize), piano roll (note add, move, resize, delete, velocity), Pocket knobs, undo. All edits already go through `Txn` and a loopback `mdaw-sync` replica.
- **Demo:** draw a clip, hear it, undo it.

### M2: Server and live sync

- `mdaw-net` (quinn, pinning, framing) and `mdaw-server` (project thread, SQLite events, snapshot every 1000 events, sequencing rules 1 to 4).
- `mdaw-sync` over the network: optimistic apply, rejection, reconnect and resume.
- Presence and soft locks: a clip being dragged by someone else is dimmed and not draggable; advisory only, 2 s timeout.
- Changelog panel showing `Superseded` entries with a restore button (data only; samples arrive in M4).
- Tests: deterministic simulation of N replicas with delay, reorder and disconnect; in-process server with two headless clients over loopback QUIC.
- **Demo:** two clients on one machine edit the same clip; kill and restart one and it catches up.

### M3: Shared clock and transport

- Clock sync over datagrams with a min-RTT filter (burst of 5 on connect, then 1 Hz).
- `TransportState` handling, scheduled start, server re-anchor on tempo change, Link toggle.
- Engine maps server time to its sample clock; drift above 20 ms triggers a resync jump.
- **Demo:** press play on client A; both playheads agree within about 10 ms on localhost, measured by logging positions against server time.

### M4: Checkout, commit and changelog samples

- Server: `Checkout`, `Commit`, `Release`, `ForceRelease`, leases, blob store and blob streams.
- Client: workspace with on-disk journal, read-only rendering of other users' checked-out tracks with an owner badge, commit dialog, fork file on force-release and "restore as new track".
- Render worker: Solo sample always; InContext when all other tracks are built-in or have stems. FLAC encode, upload, then `Commit`.
- Tempo-conflict flow from interpretation 1, with ProjectBase and ProjectCurrent samples.
- Late `Attach` of a Before sample to `Superseded` entries by the client whose edit was displaced.
- Changelog panel plays samples through the engine's preview bus.
- **Demo:** A checks out a track and edits; B sees the lock and old content; A commits; B sees the new notes and plays the sample. Repeat with B changing tempo mid-checkout, and with B force-releasing.

### M5: Live MIDI

- Input from `midir` devices and the computer keyboard.
- `Midi{actor, track, stream_seq, events, held: Vec<u8>}` datagrams relayed by the server. Each datagram repeats the previous two events and the full held-note set; receivers silence any sounding note missing from it and time out after 2 s. Keepalive every 250 ms while notes are held.
- Remote notes play on receipt into the track's instrument.
- Recording is local: the player's own notes are timestamped on its audio clock against the linked transport and written as a normal `NoteAdd` txn on stop. Recorded data therefore travels on the reliable state plane and lands exactly where it was played.
- **Demo:** two clients jam on a Pocket track; record on A; the clip appears on B with identical note positions. Repeat under `tc netem` with 80 ms delay and 5% loss, and check for stuck notes.

### M6: CLAP devices and stems

- Productionise the S1 host in its own crate, `mdaw-plugin`, behind a format-neutral interface so that VST3 can be added as a second backend: plugin scan, instance lifecycle, params and state, GUI window, offline instance for rendering. The audio graph sees plugins only as `ExternalDevice`.
- `DeviceKind::Plugin { format, id, state }` with the state blob uploaded on commit; the checkout-only rule and stem requirement enforced on the server.
- Stem bounce on commit (full track, pre-fader, 24-bit FLAC, peak-scaled to -1 dBFS at most with the factor stored as `stem.gain`, rendered in a fresh offline instance), download and cached decode on other clients, resampling (linear for now) when device rates differ, stale badge when `tempo_hash` differs.
- Clients that have the same plugin render it live from the state blob; others play the stem.
- `mdaw-test-plugin` drives headless host tests in CI.
- **Demo:** A adds Vital to a track through checkout and commits. B, started with plugins disabled (`--no-plugins`, also `--no-clap`), hears the stem; started normally, B renders Vital itself.

## Verification

- `cargo test --workspace`: model property tests, sync simulation, loopback integration, golden audio hash, CLAP host tests against `mdaw-test-plugin`.
- `cargo clippy --workspace -- -D warnings` and the same test suite in CI on ubuntu and windows runners.
- Demo launcher (`cargo xtask demo`): starts `mdaw-server` with a temp data dir and two clients with separate `--profile` directories, pre-filled with the join string. Each milestone's demo above is run through it by hand.
- Real-time safety: debug builds install an allocator guard that panics on allocation inside the audio callback.
- Windows: build and headless tests in CI; audio output, MIDI input and plugin GUI checked by hand on a Windows machine at M1, M5 and M6.

## Risks carried into implementation

- **Plugin GUIs on Wayland** (resolved by spike S1): plugins only offer embedded X11, which works through XWayland in a host-created window. Without XWayland there are no plugin editors.
- **Offline second instances** (resolved by spike S1, see `docs/spikes/clap-host.md`): instances restored from state sound right but are not bit-exact for every plugin. Stems and samples are bounced offline in a fresh instance; the stem is the reference sound.
- **Clock drift** handling is a resync jump in the prototype; continuous correction is deferred.
