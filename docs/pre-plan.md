# "Multiplayer" DAW: Pre-Plan

Status: early brainstorm, decisions so far. Next topic: sync protocol and op format.

## 1. Vision

A DAW built on a server/client model. The server syncs clients as fast as possible, and clients push their local changes to the server. The focus is **collaborative editing**, not real-time performance. Target platforms are Linux and Windows, with a native, fast client and server.

## 2. Architecture: two planes

| Plane | Carries | Characteristics |
|---|---|---|
| **State plane** | Project document: tracks, clips, notes, automation, tempo, plugin params | Small data, server-ordered ops, conflict handling |
| **Audio plane** | Rendered audio (stems, changelog samples, optional streaming) | Large, latency-sensitive, loosely coupled to state |

A third, small side channel carries **live MIDI** (see section 6).

## 3. Core model

**Unit of sync: the track.** Project-level state (tempo map, markers, master bus, track order, routing between tracks) is always live and shared. Async checkout locks only a track's own contents (clips, notes, automation, plugin chain).

### Track modes

- **Live:** multiple clients can edit at once. The server assigns a total order to ops, and clients apply them optimistically.
- **Async (checked out):** exactly one client owns the track and works in a private local workspace. Everyone else sees the last committed version, read-only, with a "being edited by X" marker.

### Transitions

- **Live to async (checkout):** the client takes a snapshot of the current track version and works locally.
- **Async to live (commit):** the commit is immediately the truth for all clients and releases the track back to live. There is no review gate.
- Both transitions are quick and reversible. Each mode switch produces a changelog entry.

### Rules

- A commit is a **fast-forward**: no merge, because only one client can edit a checked-out track.
- Async only affects *downstream* visibility. Others see nothing until the commit lands.
- **Structural changes** (delete, restructure, re-route) to a track need it to be live or unclaimed, not checked out by someone else.
- A **send belongs to its source track** and is locked with it.
- Optional later: a "checkpoint" that commits but keeps the track checked out. Skipped for now because it muddies "commit means live".

## 4. Live-mode conflicts

- Every note, clip and parameter has a stable ID. Edits are property-level.
- The server's order wins (last writer wins), so nothing stalls.
- While a user is dragging or editing a clip, others get a **soft lock** on that clip.
- The displaced edit is kept in the changelog as a "superseded" entry with a listening sample and can be restored with one click.

## 5. Async-mode conflicts

Because commits are fast-forward, true conflicts are rare. The main case is **project state changing while a track is checked out** (tempo, send target, master chain), so the committed track now behaves differently.

- It is detected at commit time.
- The committer sees two listening samples: "your version" versus "with the current project state". They pick one and then commit.
- The committer resolves it, because commits are immediately authoritative.

**Abandoned checkouts:** use a lease with heartbeats, plus a manual force-release by any user. On release, the owner's unpushed work is kept as a recoverable fork.

## 6. Live MIDI (p2p-style jamming)

- Latency is network latency (tens of milliseconds). The feel will be "close", not tight, and that is acceptable for an editing-focused tool.
- **Playback vs recording are separate:** you hear your own notes instantly and the other person's with latency, but every note is timestamped against the shared transport clock. Recorded clips therefore land exactly where they were played.
- Send MIDI over an unreliable, sequenced channel (UDP or QUIC datagrams), with stuck-note protection (note-off resends or timeouts).
- Both sides need the same synth and preset locally. This is trivial for built-in synths and awkward for arbitrary VSTs.
- **Route via the server first.** Direct p2p (hole punching or WebRTC data channels) is a later optimization.

## 7. Plugins and audio

- **Built-in synths and plugins:** a base set, network-enabled and **deterministic**. Only notes and params are synced, and each client renders locally, so bandwidth is minimal.
- **Local-only VSTs:** plugin state is stored as an opaque blob. Collaborators without the plugin hear a **stem**, which the owner bounces on commit.
- Supported formats: **CLAP first-class**, VST3 as a compatibility layer.
- **Sync mode decides what crosses the wire:** control data for built-ins, plugin state plus stems for local VSTs, and streamed audio only in a possible live-streaming mode.

## 8. Changelog with listening samples

Every commit and mode switch produces a changelog entry with short audio clips of the affected parts.

- The **committing client renders the samples**, because only it has the plugins. They are uploaded with the commit as compressed clips stored by content hash.
- The affected range (which bars, which tracks) comes from diffing the ops.
- The reliable sample is the **track soloed**. A "track in context" sample works only when the other tracks can be rendered too (built-ins), or from other tracks' last stems.
- The same clips power conflict resolution (sections 4 and 5).

## 9. Other infrastructure

- **Time sync:** shared clock, tempo map and latency compensation, so all clients agree on where a given bar is.
- **Assets:** samples and stems live in content-addressed storage (dedupe by hash) and are uploaded separately from ops.
- **History and undo:** the changelog doubles as version history. Undo in multi-user sessions should undo only your own edits.

## 10. Tech direction

- **Native, cross-platform (Linux and Windows), fast.**
- Leaning toward a **Rust core**: shared crate for state, ops and protocol between client and server, strong networking (QUIC).
- **Main risk:** plugin hosting (CLAP and VST3) in Rust. Spike it early. The fallback is a JUCE-based client engine (watch the licensing).
- Audio I/O must work on Windows and on Linux (JACK or PipeWire).
- UI is undecided: native toolkit (egui, Slint) or a web-style UI over the native engine.

## 11. Prototype scope (thin vertical slice)

1. Server plus two clients sharing a project (tracks and MIDI clips) in live mode.
2. One deterministic built-in synth, rendered locally on both clients.
3. Shared transport and clock.
4. Async checkout and commit for one track, with a changelog entry and rendered sample.
5. Timestamped live MIDI through the server.
6. Local plugin hosting (CLAP) with a stem bounce on commit.

**Early spikes:** plugin hosting in Rust, and audio rendering of changelog clips.

## 12. Open questions

- **Sync protocol and op format:** entities and IDs, server sequence numbers, commit object structure (ops plus assets plus changelog samples), reconnect and reconcile. *This is the next topic.*
- UI technology: native toolkit or web-style over a native engine.
- Whether to add a checkpoint (commit while keeping the checkout).
- Whether readers auto-follow commits or pull manually. The current assumption is that commits are truth for all.
- Permissions and roles (who can force-release, who can commit to which track).
- Plugin determinism and version pinning for built-ins (float behavior across platforms).
