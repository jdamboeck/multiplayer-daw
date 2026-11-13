# mdaw protocol

What client and server say to each other, as built. Protocol version **6**. The types live in
`mdaw-proto` (`lib.rs` for transactions and events, `wire.rs` for messages) and in `mdaw-model`
(ops); this text explains the rules around them. People share the project — tracks, clips,
notes, devices, tempo — and nothing else: each client plays it back on its own, and nothing
about playback travels.

Version 6 removed the shared transport and clock sync, live notes over the network, checkout
and commit with changelog samples, and stems (all of version 5). Data directories written by an
earlier server are not readable (see "Server storage").

## Connection

- QUIC, ALPN `mdaw/0`. Idle timeout 10 s, keep-alive every 2 s. No datagrams are used.
- The server creates a self-signed certificate on first start and keeps it in its data directory.
  Clients do not use a certificate authority. They accept exactly the certificate whose SHA-256
  fingerprint is in the join string.
- Join string: `mdaw://<host>:<port>?fp=<64 hex digits>&token=<secret>`. The token is the session
  secret; everyone who holds it has the same rights.
- The first bidirectional stream the client opens is the control stream. Each direction is a
  sequence of frames: a `u32` little-endian length, then that many bytes of postcard. A client's
  frames may be up to 16 MiB, so no event is much larger; the server's up to 256 MiB, for a
  snapshot or a catch-up. A longer frame is an error and ends the connection. A client does not
  send a message over its limit: an edit that large is taken back with a notice, and the
  connection stays.
- Every further stream the client opens carries one blob (see "Blobs"). The server serves them
  only after the `Welcome`.

## Joining

The client speaks first:

```
Hello { proto_version, token, user_key: [u8; 16], name, resume: Option<Resume> }
Resume { prev_actor, session_secret, last_seq }
```

`user_key` is a random key the client install made once and keeps in its profile. The server maps
it to a `UserId` that stays the same across connections. `name` is a display name; the server
trims it, drops control characters, cuts it to 32 characters and calls an empty one "anonymous".
The first message may be at most 64 KiB; a larger one ends the connection without an answer,
before the token is looked at.

The server answers with one of:

```
Welcome { actor, user, session_secret, head_seq, sync, acked, peers, history }
Refused(BadToken | ProtoVersion(server's) | Protocol)      // then the server closes
```

- `actor` is a number no other connection ever had or will have. The client mints entity IDs as
  `actor << 32 | counter`, so IDs never collide and need no coordination.
- `sync` is `Snapshot(Project)` at `head_seq`, or on a resume `Events(...)` with everything after
  `last_seq`.
- `session_secret` proves on the next connection that the previous one was this client's.
- `history`, with a snapshot only: the last 200 events that displaced someone's edit (see
  "Concurrent edits"), as many of the newest as fit in 16 MiB, so a new client can list them.
  Their effect is in the snapshot already; the client does not apply them.

### Resuming

A client that lost its connection sends `resume` with the actor and secret of its last `Welcome`
and the seq of the last event it received. If the secret fits and the log still reaches back that
far (at most 5000 events, and at most 64 MiB of them), the server drops the old connection and
answers with the missing events. Otherwise the answer carries a snapshot, as for a new client.

Events are written to disk without waiting for each to land, so a crash of the machine can take
the last few with it, and after the restart other events take their seqs. A client of the run
before the restart (its `prev_actor` was handed out then) that claims events past the head the
server started with gets a snapshot. Actor IDs, the token and the session key are written
durably, so none of them is ever handed out twice.

`acked` in the `Welcome` is the highest `client_txn_id` of the previous actor that the server has
seen, sequenced or rejected. The client drops pending transactions up to that number that the
catch-up did not confirm (they were rejected while it was away) and submits the rest again under
its new actor. This works across a server restart too, because the log stores each event's actor
and transaction number.

Limits of this, accepted for the prototype:

- When the resume is not accepted and the client receives a snapshot, it submits all pending
  transactions again. One the old connection got through is then applied twice.
- After a server restart the server no longer knows which transactions it rejected. They come in
  again and are rejected again.

## Edits

```
Txn { client_txn_id, base_seq, ops: Vec<Op> }          // client to server, in Submit
Event { seq, actor, user, wall_ts, client_txn_id, ops, superseded: Vec<Superseded> }
                                                       // server to everyone
Rejected { client_txn_id, reason }                     // server to the sender
RejectReason = Invalid(why) | FutureBase | MissingBlob(hash)
```

Every change to the project is a transaction, whoever makes it and whatever it touches; there
are no locks and no other kind of edit. A transaction is one user gesture: applied whole or not
at all, and one step of undo.
`client_txn_id` counts up per client. `base_seq` is the last event the client had applied when it
made the edit.

The server handles one transaction at a time:

1. **Check.** Ops out of range (`Invalid`), an empty transaction (`Invalid`), a `base_seq` ahead
   of the log (`FutureBase`), and an op that names a blob the server does not hold
   (`MissingBlob`: a plugin's state in `DeviceAdd`, `DeviceSetState` or a `TrackAdd`) are
   rejected. So is a transaction that would take the project past its size limits
   (1000 tracks, 10 000 tempo points, 10 000 clips and 32 devices per track, 50 000 notes per
   clip, 4096 parameters per device), checked op by op against what is there. A rejected
   transaction changes nothing; its sender alone gets `Rejected { client_txn_id, reason }`.
2. **Apply** to the server's document. Applying is total: an op on something that no longer
   exists, or an add of an ID that exists, does nothing, on every replica alike.
3. **Find what it displaced** (below).
4. **Append** the event to the log on disk, then send `Event` to every client, the sender included.

Events are the only way the document changes. Every client applies them in `seq` order, so all
replicas hold the same document.

The project thread handles one message at a time. Each connection may have 64 MiB of messages
waiting for it, each counted as at least 1 KiB; past that, the server stops reading from it until
there is room, and QUIC's flow control slows the client down.

### What the client does

The client keeps the document as of the last event (`confirmed`) and its own transactions the
server has not answered yet (`pending`). What the user sees is `confirmed` with `pending` applied on
top, so edits show at once. An event from the client's own actor confirms the oldest pending
transaction; `Rejected` removes one, and the view falls back to what the server has.

Transactions go out in the order they were made. One that names a blob this client is still
uploading waits, and everything after it waits behind it, until the server has said `Stored`;
so the server never sees an edit before its blobs. After a reconnect the pending transactions go
again in the same way, behind the uploads that start over.

### Concurrent edits: last one wins, the other is kept

Two edits are concurrent when the later one was made without knowing the earlier
(`base_seq` of the later < `seq` of the earlier). If both write the same property of the same
entity, the later one in the log wins. The server records the loser in the winner's event:

```
Superseded { entity, prop, lost: Op, lost_actor, lost_user, lost_seq }
```

`lost` is the op that was overwritten, complete, so any client can submit it again: that is the
Restore button in the changelog panel, which lists these records and nothing else. An edit to something a concurrent transaction removed is recorded
the same way, in its own event (`lost_seq` equals the event's `seq`); it cannot be restored while
the thing is gone.

The server looks for displaced edits in the last 4096 events only. Clients that join by snapshot
do not see entries from before they joined.

### Undo

Each client keeps the inverses of its own transactions (500 steps). Undo submits the inverse as a
normal transaction. An inverse that would set a property is left out when the property no longer
holds what this client wrote, so undo never takes back someone else's later change.

## Presence

```
Presence { editing: Option<EntityRef> }                             // client to server
Presence { actor, presence }                                        // server to the others
Peers(Vec<Peer { actor, user, name }>)                              // server to everyone
```

Presence is relayed and never stored. Clients send it when it changes, at most 10 times a second,
and repeat it every 500 ms while they hold something.

`editing` names what the user is dragging. The other clients treat that entity and everything
inside it as held: they dim it and do not start drags on it. This is advice between clients, not a
lock; the server does not look at it, and an edit that arrives anyway is sequenced like any other.
A hold that is not repeated for 2 s lapses, and all holds of a client end when it disconnects.

`Peers` is sent whenever someone joins or leaves.

## Playback is local

Play, stop, seek and loop act on the client's own engine. No message carries them, the server
knows nothing of them, and two people on one project play it independently. A tempo change is
an ordinary edit; each engine keeps its playhead at the same musical position when it arrives.

Notes played on the computer keyboard or a MIDI device are heard at once on the player's own
machine and nowhere else.

### Recording

Recording is local and lands as an ordinary transaction. While recording is armed, a client
stamps each note its player starts or ends with its own clock, and turns that into a tick by its
own transport, from where the playhead was when it was last heard. Notes played while the
transport does not run are dropped. When the transport stops, or recording is disarmed, the take
becomes one `Txn`: per track, `NoteAdd` into the clip that spans all of the take's notes on it,
or else a `ClipAdd` named "Take" of whole bars holding them. Notes still held end where the
playhead was last; a note held across the end of the loop lasts to that end. The take is one
undo step and reaches the others like any edit.

## Plugins

A device can be a plugin installed on the user's machine:

```
DeviceKind::Plugin { format: PluginFormat, id: String, state: Option<BlobHash> }
PluginFormat = Clap            // another format is another variant; nothing else changes
```

- **Ordinary edits.** Anyone puts a plugin on any track (`DeviceAdd`), removes it, bypasses it
  or changes its settings, by transactions like every other edit.
- **One instrument.** A track has one instrument, the first device: Pocket or a plugin that
  makes sound from notes. Picking another replaces it in the same transaction; plugins that
  change sound (effects) come after it. The client keeps to this; the server does not check it.
- **State.** The plugin's settings are its own state, an opaque blob; `id` names the plugin
  within its format. The device's `params` are not used for plugins. Turning a plugin's knobs,
  in its own window or in the generic controls, changes the running instance; every 2 s the
  client saves the state of each instance that says it changed or whose window is open, and if
  the save differs from the one the same instance gave when it was made or last saved, uploads
  it and submits `DeviceSetState` (which waits for the upload, see "What the client does").
  Successive saves of one plugin are one undo step. Plugins do not always say that their
  settings changed (Vital does not for presets), so a save is compared only with saves of the
  same instance: different instances save the same settings differently.
- **Hearing a track with plugins.** Each client decides for itself. With every plugin of the
  track installed and its state at hand, it runs them, loaded with the document's state.
  Otherwise the whole track is silent on that client, and its header says so: "plugin not
  installed" when a plugin is missing (or the client runs with `--no-plugins`), "no sound here"
  when the state could not be had, "loading plugin…" while one starts. The device tab names the
  missing plugin.
- **Downloads.** State blobs are downloaded when a client needs them.
- **Listening volume.** The volume in each client's top bar is that listener's own: applied at
  the end of that client's output, it is not part of the document. (The document's master
  volume stays, at 1 unless an older project set it.)
- **Crashes.** On Linux each plugin instance runs in a helper process (`mdaw-plugin-host`),
  also for scanning, one file at a time and a few files at once. What a scan found is kept in
  `~/.cache/mdaw/clap-scan` and a file is scanned again only once it changed; one whose scan
  failed is left out until it changes. Helpers start on threads of their own, so the client
  does not wait for them; meanwhile the track header says "loading plugin…". The audio thread
  hands a helper each block through anonymous shared memory (a `memfd` only that helper gets)
  and waits at most the block's length plus 2 ms, and never past 60% of the audio callback for
  all plugins together; a late block goes without the plugin. Helpers report changes to their
  state and window themselves. A helper whose client is gone ends within two seconds.
  A helper that crashes or stops answering is ended: the client says so, the track is silent,
  and the user can restart the plugin, which comes back with the settings the document has (at
  most about two seconds old). A plugin that does not start or will not take its settings is
  treated the same way, as is one running inside the client whose processing fails. While a
  plugin waits for a restart it is not tried again.

## Blobs

Plugin state travels as blobs (and audio files will), named by the BLAKE3 hash of their bytes. Each transfer
has a stream of its own, so a large upload never holds up the control stream:

```
BlobRequest::Put { hash, len }  ->  Send | Stored | Refused;  bytes, finish  ->  Stored | Refused
BlobRequest::Get { hash }       ->  Found { len } + bytes | Missing
```

The server answers `Stored` at once if it has the blob, checks that uploaded bytes hash to the
announced hash, and refuses blobs over 256 MiB. Uploads go to disk as they arrive; the server
never holds one in memory whole. The client checks the hash of every download.
Blobs are kept as files under `blobs/` in the data directory, each upload written to a file of its
own, flushed to disk and then renamed, so a stored blob is whole even after a crash. Halves left
by a crash are removed when the server starts. Blobs are never deleted. A client may have four
transfers at a time; further streams wait.

## Server storage

One SQLite database (`project.db`, WAL mode) in the data directory, next to the certificate
(`cert.der`, `key.der`):

| Table | Holds |
|---|---|
| `events(seq, actor, txn, data)` | The log. `txn` is the event's `client_txn_id`; `data` is the postcard `Event`. |
| `snapshots(seq, data)` | The document every 1000 events, to shorten loading. |
| `users(id, key, name)` | `UserId` per install key. |
| `meta(key, value)` | Format version, session token, the key session secrets are derived from, actor counter. |

Blobs are files in `blobs/`, named by their hash in hex.

The format entry is `<store version>/<protocol version>`, now `2/6`. A database with another
format is not opened; the prototype has no migrations, so a breaking change means a new data
directory. In particular, data directories of protocol 5 and earlier are not readable: move them
away and start a new project.

A client whose outgoing queue reaches 4096 messages is disconnected and resumes later.
