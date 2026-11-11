# mdaw protocol

What client and server say to each other, as built. Protocol version **5**. The types live in
`mdaw-proto` (`lib.rs` for events, `wire.rs` for messages, `transport.rs` for the transport and
datagrams, `midi.rs` for live notes) and in `mdaw-model` (ops); this text explains the rules
around them. It covers what exists after milestone M6: the state plane with live edits, the
shared clock and transport, checkout and commit with blobs and changelog samples, live notes
with recording, and plugins with stems.

## Connection

- QUIC, ALPN `mdaw/0`. Idle timeout 10 s, keep-alive every 2 s.
- The server creates a self-signed certificate on first start and keeps it in its data directory.
  Clients do not use a certificate authority. They accept exactly the certificate whose SHA-256
  fingerprint is in the join string.
- Join string: `mdaw://<host>:<port>?fp=<64 hex digits>&token=<secret>`. The token is the session
  secret; everyone who holds it has the same rights.
- The first bidirectional stream the client opens is the control stream. Each direction is a
  sequence of frames: a `u32` little-endian length, then that many bytes of postcard. A client's
  frames may be up to 16 MiB, so no event is much larger; the server's up to 256 MiB, for a
  snapshot or a catch-up. A longer frame is an error and ends the connection. A client does not
  send a message over its limit: an edit that large is taken back, a commit that large is
  cancelled, each with a notice, and the connection stays.
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
Welcome { actor, user, session_secret, head_seq, sync, acked, peers, transport, history, stale }
Refused(BadToken | ProtoVersion(server's) | Protocol)      // then the server closes
```

- `actor` is a number no other connection ever had or will have. The client mints entity IDs as
  `actor << 32 | counter`, so IDs never collide and need no coordination.
- `sync` is `Snapshot(Project)` at `head_seq`, or on a resume `Events(...)` with everything after
  `last_seq`.
- `session_secret` proves on the next connection that the previous one was this client's.
- `transport` is the shared transport as it is now (see "Clock and transport").
- `history`, with a snapshot only: the last 200 events the changelog shows (everything but live
  edits that displaced nothing), as many of the newest as fit in 16 MiB, so a new client sees recent commits and their samples. Their
  effect is in the snapshot already; the client does not apply them.
- `stale`: the checkouts whose lease ran out (see "Checkout and commit").

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
Txn { client_txn_id, base_seq, ops: Vec<Op> }           // client to server, in Submit
Event { seq, actor, user, wall_ts, body }               // server to everyone
EventBody::Txn { client_txn_id, ops, superseded: Vec<Superseded> }
```

A transaction is one user gesture: applied whole or not at all, and one step of undo.
`client_txn_id` counts up per client. `base_seq` is the last event the client had applied when it
made the edit.

The server handles one transaction at a time:

1. **Check.** Ops out of range, an empty transaction, a `base_seq` ahead of the log, content edits
   on a track that is checked out or carries a plugin, and adding a plugin outside a commit are
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

### Concurrent edits: last one wins, the other is kept

Two edits are concurrent when the later one was made without knowing the earlier
(`base_seq` of the later < `seq` of the earlier). If both write the same property of the same
entity, the later one in the log wins. The server records the loser in the winner's event:

```
Superseded { entity, prop, lost: Op, lost_actor, lost_user, lost_seq }
```

`lost` is the op that was overwritten, complete, so any client can submit it again: that is the
restore button in the changelog. An edit to something a concurrent transaction removed is recorded
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
Presence { editing: Option<EntityRef>, playhead: Option<Tick> }     // client to server
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

## Clock and transport

The transport is session state. It is not part of the document, is not stored, and starts out
stopped at tick 0 when the server starts. All times are the server's clock: microseconds since
the server process started.

### Clock

```
Ping { t0 }                      // datagram, client to server; t0 is the client's own clock
Pong { t0, server_micros }       // datagram, the answer
```

Datagrams are postcard without a length prefix. They may get lost; nothing waits for them.

A client pings 5 times, 20 ms apart, on every new connection, then once a second. For each pong
that comes back at client time `t1`, the round trip is `t1 - t0` and the offset of the server's
clock is `server_micros - (t0 + t1) / 2`. The client keeps the last 16 samples and believes the
one with the shortest round trip: that one spent the least time queued, so its halfway point is
closest to the truth. The estimate is off by at most half of that round trip. It starts from
nothing on every connection.

### Transport

```
TransportState { playing, anchor_tick: f64, anchor_server_micros, loop_range, epoch }

SetTransport(TransportState)                         // client to server
Transport { state: TransportState, by: Option<ActorId> }    // server to everyone
```

- Stopped, the playhead rests at `anchor_tick`. Playing, it is at `anchor_tick` at
  `anchor_server_micros` and moves on from there by the project's tempo map. Before that time it
  waits at `anchor_tick`.
- With a `loop_range`, playback that started before the region's end wraps from its end to its
  start. Playback anchored at or behind the end runs on.
- Any client may ask for a new state. A request is the whole state, and the last one to reach the
  server wins as a whole. The server forces it into range: ticks into the song, an empty loop
  region to none, the anchor time of a playing state to between 10 s ago and 2 s ahead, the
  anchor time of a stopped state to now. It numbers the result with the next `epoch` and sends
  it to everyone, the asker included, with `by` naming the asker.
- A request that makes no sense (an anchor tick that is not a number) changes nothing. The asker
  alone gets the unchanged state back, again with `by`. So every request is answered exactly
  once to the one who made it.
- When a transaction that changes the tempo map is sequenced while the playhead moves, the
  server re-anchors: the new anchor is where the playhead is at that moment by the old map. It
  sends the `Event` first, then `Transport` with `by: None`.
- When the last client leaves while the transport plays, the server stops it at the nearest
  whole tick. The loop region stays.

### What the client does

- **Start and seek.** To start, a client asks for `playing` with the anchor 150 ms ahead of its
  estimate of the server's clock, so that the message is everywhere before it is due and all
  engines start on the same sample. A seek during playback is the same with a new
  `anchor_tick`: everyone waits at the new place for 150 ms. Pause asks for a stopped state at
  the nearest whole tick to where the playhead is.
- **Acting at once.** The asker acts on its request without waiting. It counts its unanswered
  requests; while there are any, it ignores what the server says about the transport, because a
  later answer will replace it. The answer to its last request then counts, whatever it is.
- **Following.** The engine turns the anchor into its own clock (`anchor_server_micros - offset`)
  and compares, per audio buffer, where its playhead is with where the anchor puts it at the
  time the buffer will be heard. A fresh anchor is taken up exactly, on the sample it is due. A
  playhead that is more than 20 ms off for three buffers in a row jumps to where it should be;
  smaller errors are left alone (no continuous correction in the prototype). A better clock
  estimate moves the anchor without restarting anything.
- **Tempo.** A client that sees the tempo map change while the playhead moves re-anchors by
  itself the same way the server does, and takes the server's anchor when it arrives.
- **Link.** A client can leave the shared transport (the Link switch). Its transport is then
  private: it starts as a copy of the shared one, and play, stop, seek and loop act on it alone
  and are not sent. Linked again, the client takes up the shared state. A client whose
  connection is down, or that has no clock estimate yet, behaves as unlinked until it has both.

## Live notes

What a player plays is heard by everyone else as it arrives, about one network delay late. It is
sound, not an edit: it changes nothing in the document and is not stored. Recording turns it into
an edit (see below).

```
Midi(MidiPacket)        // datagram, client to server, and server to the other clients
MidiPacket { actor, track, stream_seq: u32, events: Vec<LiveEvent>, held: Vec<u8> }
LiveEvent  { seq: u32, pitch: u8, vel: u8 }      // vel 1..=127 starts the note, 0 ends it
```

- **Streams.** A player's notes on one track are one stream. `stream_seq` counts its packets,
  `LiveEvent::seq` its events; both start at 1 and only grow.
- **Sending.** Every note start or end goes out at once in a packet that also repeats the two
  events before it and lists every pitch the player still holds on the track (`held`). While
  notes are held, the packet is repeated every 250 ms, and for 1 s after the last one ended, so a
  release that got lost is made up for soon. With every note held a packet is about 150 bytes.
- **Relaying.** The server passes each packet on from the connection's network task, without the
  project thread. It sets `actor` to the sender's; what the client put there is ignored. It sends
  the packet to every other connection except one whose user has the track checked out while
  the sender is someone else: a checked-out track hears only its owner until the commit or the
  release. The owner's own notes on the track are passed on as usual. A connection's packets
  are passed on at most 400 a second on average, with bursts of 200; more are dropped.
- **Receiving.** A receiver keeps, per stream, the highest `stream_seq` and event `seq` seen and
  the pitches it plays. A packet not newer than the last is dropped. Of a newer one it plays the
  events it has not seen, in order; a start only if `held` still has the pitch, since a note that
  ended in an event the packet no longer carries would only be cut off again. Then it ends every
  pitch it plays that `held` does not list. A stream not heard from for 2 s is silenced, and so
  is everything of a player who leaves, everything on a lost connection, and everything on a
  track as soon as this user's checkout of it is in the log.
- **Hearing.** Notes play into the track's instrument on arrival, through a ring straight into
  the audio thread, without waiting for the UI. Each player's notes have their own voices, so
  two players holding the same pitch do not end each other's notes. The player's own notes are
  heard at once.

### Recording

Recording is local and lands as an ordinary transaction. While recording is armed, a client
stamps each note its player starts or ends with its own clock, and turns that into a tick by
the transport it follows: for the shared transport, the stamp plus the clock offset, put into
`TransportState::position`. Notes played while the transport does not run are dropped. When the
transport stops, or recording is disarmed, the take becomes one `Txn`: per track, `NoteAdd` into
the clip that spans all of the take's notes on it, or else a `ClipAdd` named "Take" of whole bars
holding them. Notes still held end where the playhead was last; a note held across the end of
the loop lasts to that end. The take is one undo step and follows the usual rules: a track
someone else has checked out refuses it, and on the client's own checkout it goes into the
journal.

## Checkout and commit

A user can take a track's contents out of live editing, work on them alone, and hand the result
in as one commit. While the checkout lasts, everyone else hears and sees the contents as they
were, and cannot change them.

```
// client to server
Checkout { track }                 LeaseRenew { track }
Release { track }                  ForceRelease { track }
Commit(CommitRequest { track, ops, project_fix, stem, entry })
Attach { entry_seq, sample }

// server to everyone, as events
EventBody::Checkout { track }
EventBody::Commit { track, ops, project_fix, stem, entry: ChangelogEntry }
EventBody::Release { track, reason: Abandoned | Forced { owner } }
EventBody::Attach { entry_seq, sample }

// server to the asker, when a request is turned down
Denied { request: Checkout(track) | Release(track) | ForceRelease(track) | Commit(track) | Attach(seq), reason }

ChangelogEntry { summary, ranges: Vec<TickRange>, samples: Vec<Sample> }
Sample { kind: Solo | InContext | Before | ProjectBase | ProjectCurrent, blob, range, ms }
```

Every client folds these events into its document with the same function as the server
(`mdaw_proto::apply_event`): a `Checkout` sets the track's `checkout` to the event's user and
seq, a `Commit` applies `ops` and then `project_fix`, sets the stem and clears the checkout, a
`Release` clears the checkout. A checkout is owned by a user, not a connection, so it survives
reconnects and restarts on both sides.

The server's rules:

- **Checkout** needs the track to exist and not be checked out.
- **Live edits** of a checked-out track's contents are rejected for everyone, the owner included.
  Its shell (name, colour) and position stay live. Removing the track is allowed to its owner
  only.
- **Commit** is accepted only from the owner. Its `ops` must all be content ops of that track,
  its `project_fix` ops project-scope ops. Every blob it names must be on the server already: the
  samples, the stem, the states of plugins. A track that ends up with a plugin needs a stem
  (see "Plugins and stems"). The contents could not change since the checkout, so a commit is a
  fast-forward.
- **Release** ends the sender's own checkout and keeps the contents as they were.
  **ForceRelease** ends anyone's; the event records whose it was.
- **Attach** adds a `Before` sample to a recent entry that displaced an edit of the sending user:
  how the track sounded with that edit. One per track of such edits; the blob must be on the
  server. Anything else is denied with `NoSuchEntry`.

**Leases.** The owner's client sends `LeaseRenew` every 10 s. A checkout without one for 60 s is
stale: the server sends everyone `Stale(tracks)` whenever that set changes, and clients show it.
Nothing is released automatically. After a server restart every checkout gets a fresh lease.

### What the client does

- **Workspace.** On its user's `Checkout` event the client starts a workspace for the track: a
  journal of ops, the track and the tempo points at that moment, and the `seq` of the `Checkout`
  event. Edits of the track's contents go into the journal instead of a transaction and are not
  sent; the view shows the confirmed contents with the journal applied. A property written
  again keeps only its last write in the journal (adding, removing and moving stay as they
  were), so the journal grows with what changed, not with how long a drag took. Undo works
  across journal and live edits alike. The journal is written to the profile
  (`projects/<server>/checkouts/`) at most every half second while it changes and when the
  session ends, and picked up again when the client starts and finds the same checkout (same
  `seq`) still its own.
- **Commit.** Starting a commit freezes the workspace. A worker thread renders the samples (see
  below) and the client shows them in the commit dialog. On the user's decision it uploads the
  blobs, sends `Commit` once all are stored, and sends it again on a new connection until the
  `Commit` event arrives or the server says `Denied`. Cancelling the dialog unfreezes the
  workspace.
- **Tempo conflict.** If the tempo at any point inside the track's extent differs from the tempo
  at checkout, the dialog offers *Commit as is* and *Commit and restore the old tempo*. The second
  sends the ops that turn the current tempo points into the old ones as `project_fix`, applied in
  the same event.
- **Forced release.** When someone else ends the checkout, the journal applied to the track as
  it was at checkout is saved as a fork (`projects/<server>/forks/`), whatever became of the
  track since. A journal on disk whose checkout is no longer the client's when it starts (ended
  while the client was not running) becomes a fork the same way. The changelog offers to add a
  fork to the project as a new track, right below the track it came from. A dropped checkout
  (the owner's own `Release`) throws the journal away.
- **Others' checkouts** are drawn dimmed with the owner's name, and edits of them are refused in
  the client already. Anyone can end them, after a confirmation.
- **Displaced edits.** When an event displaces an edit of this client's user (see "Concurrent
  edits"), the client renders the track alone as it sounded with that edit and attaches it to the
  event as a `Before` sample.

### Samples

Rendered offline at 48 kHz with the same graph as playback, then encoded as 16-bit stereo FLAC.
The range is the union of the changed ranges grown to whole bars, at most 8 bars; without a
changed range (mix or device edits), the 4 bars from the first bar with a note. A track without
notes gets no samples. A commit carries:

- `Solo`: the track alone, and `InContext`: all tracks, if every other track can be rendered here
  (has no plugins). For a track with plugins both are cut from its stem.
- With a tempo conflict also `ProjectBase` (old tempo) and `ProjectCurrent` (tempo now), all
  tracks. With the old tempo restored, `Solo` and `InContext` are rendered with it.

Other clients download a sample when it is first played, decode and resample it to the output
rate on a worker thread, and play it on the engine's preview bus, beside the transport. A
sample longer than five minutes, at whatever rate, or at a rate outside 8 to 384 kHz, is not
played. A sample is downloaded and decoded once, however often its button is pressed meanwhile.

## Plugins and stems

A device can be a plugin installed on the user's machine:

```
DeviceKind::Plugin { format: PluginFormat, id: String, state: Option<BlobHash> }
PluginFormat = Clap            // another format is another variant; nothing else changes
Track.stem: Option<Stem { blob, tempo_hash, len_samples, gain }>
```

- **Only through a checkout.** A plugin goes onto a track, and a track with a plugin is changed,
  only while it is checked out (the server rejects it otherwise, and the client says so before
  sending). Every commit of such a track carries a stem.
- **One instrument.** A track has one instrument, the first device: Pocket or a plugin that
  makes sound from notes. Picking another replaces it in the same transaction; plugins that
  change sound (effects) come after it. The client keeps to this; the server does not check it.
- **State.** The plugin's settings are its own state, an opaque blob; `id` names the plugin
  within its format. The device's `params` are not used for plugins. Turning a plugin's knobs,
  in its own window or in the generic controls, changes nothing in the document; at the commit
  the owner's client saves the state of every plugin of the track, uploads it, and adds a
  `DeviceSetState` to the checkout's journal, unless the save is the same as the one the same
  instance gave right after it was made or last committed. Plugins do not always say that their
  settings changed (Vital does not for presets), so a save is compared only with saves of the
  same instance: different instances save the same settings differently.
- **Stem.** The owner's client renders the track alone, from new instances in offline mode
  loaded with the states, from the start of the timeline to the end of the last clip plus one
  second of tail, at 48 kHz, before the track's fader, pan and mute and before the project's
  master volume, which apply where it plays. It scales the audio so that its peak is at
  most -1 dBFS (plugins play beyond full scale), encodes it as 24-bit stereo FLAC, and records
  the factor that undoes the scaling as `gain`, and the tempo map it was rendered with as
  `tempo_hash`. The plugins get half a second of silence to settle first. The render runs on
  the UI thread, a slice per frame, because plugins expect
  their main thread to be that one.
- **Hearing a track with plugins.** Each client decides for itself. With every plugin of the
  track installed and its state at hand, it runs them, loaded with the committed state. Otherwise
  the whole track plays its stem, sample-aligned to the timeline, through the track's fader and
  pan; with another tempo map than `tempo_hash` the stem is marked stale and still plays. With
  neither, the track is silent. Live notes reach a track only where its plugins run.
- **Downloads.** State blobs and stems are downloaded when a client needs them. A stem is decoded
  and resampled to the output rate off the UI thread; one longer than 15 minutes is not played.
- **Listening volume.** The volume in each client's top bar is that listener's own: applied at
  the end of that client's output, it is not part of the document, of stems or of samples.
  (The document's master volume stays, at 1 unless an older project set it.)
- **A client with no plugins** (`--no-plugins`) plays every such track from its stem.
- **Crashes.** On Linux each plugin instance runs in a helper process (`mdaw-plugin-host`),
  also for scanning, one file at a time and a few files at once. What a scan found is kept in
  `~/.cache/mdaw/clap-scan` and a file is scanned again only once it changed; one whose scan
  failed is left out until it changes. Helpers start on threads of their own, so the client
  does not wait for them; meanwhile the track header says "loading plugin…". The audio thread
  hands a helper each block through anonymous shared memory (a `memfd` only that helper gets)
  and waits at most the block's length plus 2 ms, and never past 60% of the audio callback for
  all plugins together; a late block goes without the plugin. Helpers report changes to their
  state and window themselves. A helper whose client is gone ends within two seconds.
  A helper that crashes or stops answering is ended: the client says so, the track plays its
  stem (or is silent), and the user can restart the plugin, which comes back with its settings
  as kept aside at most two seconds before. A plugin that does not start or will not take its
  settings is treated the same way, as is one running inside the client whose processing
  fails. While a plugin waits for a restart it is not tried again.
- **A commit is not sent** when a plugin of the track could not be made for the render, fails
  or crashes during it, or when the track has notes and the render is all silence; nor when the
  stem would run past 15 minutes from the start of the song. The client says why, and the
  track stays checked out. This is all local to the client: nothing about it reaches the server or the others.

## Blobs

Audio and plugin state travel as blobs, named by the BLAKE3 hash of their bytes. Each transfer
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
| `events(seq, actor, txn, data)` | The log. `data` is the postcard `Event`. |
| `snapshots(seq, data)` | The document every 1000 events, to shorten loading. |
| `users(id, key, name)` | `UserId` per install key. |
| `meta(key, value)` | Format version, session token, the key session secrets are derived from, actor counter. |

Blobs are files in `blobs/`, named by their hash in hex.

The format entry is `<store version>/<protocol version>`. A database with another format is not
opened; the prototype has no migrations, so a breaking change means a new data directory.

A client whose outgoing queue reaches 4096 messages is disconnected and resumes later.
