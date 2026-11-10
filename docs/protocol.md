# mdaw protocol

What client and server say to each other, as built. Protocol version **2**. The types live in
`mdaw-proto` (`lib.rs` for events, `wire.rs` for messages, `transport.rs` for the transport and
datagrams) and in `mdaw-model` (ops); this text explains the rules around them. It covers what
exists after milestone M3: the state plane with live edits, and the shared clock and transport.
Checkout and commit, blobs and live MIDI are in `docs/plan.md` and get their sections here when
they are built.

## Connection

- QUIC, ALPN `mdaw/0`. Idle timeout 10 s, keep-alive every 2 s.
- The server creates a self-signed certificate on first start and keeps it in its data directory.
  Clients do not use a certificate authority. They accept exactly the certificate whose SHA-256
  fingerprint is in the join string.
- Join string: `mdaw://<host>:<port>?fp=<64 hex digits>&token=<secret>`. The token is the session
  secret; everyone who holds it has the same rights.
- The client opens one bidirectional stream, the control stream. Each direction is a sequence of
  frames: a `u32` little-endian length, then that many bytes of postcard. Frames above 16 MiB are
  an error and end the connection.

## Joining

The client speaks first:

```
Hello { proto_version, token, user_key: [u8; 16], name, resume: Option<Resume> }
Resume { prev_actor, session_secret, last_seq }
```

`user_key` is a random key the client install made once and keeps in its profile. The server maps
it to a `UserId` that stays the same across connections. `name` is a display name; the server
trims it, drops control characters, cuts it to 32 characters and calls an empty one "anonymous".

The server answers with one of:

```
Welcome { actor, user, session_secret, head_seq, sync, acked, peers, transport }
Refused(BadToken | ProtoVersion(server's) | Protocol)      // then the server closes
```

- `actor` is a number no other connection ever had or will have. The client mints entity IDs as
  `actor << 32 | counter`, so IDs never collide and need no coordination.
- `sync` is `Snapshot(Project)` at `head_seq`, or on a resume `Events(...)` with everything after
  `last_seq`.
- `session_secret` proves on the next connection that the previous one was this client's.
- `transport` is the shared transport as it is now (see "Clock and transport").

### Resuming

A client that lost its connection sends `resume` with the actor and secret of its last `Welcome`
and the seq of the last event it received. If the secret fits and the log still reaches back that
far (at most 5000 events), the server drops the old connection and answers with the missing
events. Otherwise the answer carries a snapshot, as for a new client.

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
   on a track that is checked out or carries a CLAP device, and adding a CLAP device outside a
   commit are rejected. A rejected transaction changes nothing; its sender alone gets
   `Rejected { client_txn_id, reason }`.
2. **Apply** to the server's document. Applying is total: an op on something that no longer
   exists, or an add of an ID that exists, does nothing, on every replica alike.
3. **Find what it displaced** (below).
4. **Append** the event to the log on disk, then send `Event` to every client, the sender included.

Events are the only way the document changes. Every client applies them in `seq` order, so all
replicas hold the same document.

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

## Server storage

One SQLite database (`project.db`, WAL mode) in the data directory, next to the certificate
(`cert.der`, `key.der`):

| Table | Holds |
|---|---|
| `events(seq, actor, txn, data)` | The log. `data` is the postcard `Event`. |
| `snapshots(seq, data)` | The document every 1000 events, to shorten loading. |
| `users(id, key, name)` | `UserId` per install key. |
| `meta(key, value)` | Format version, session token, the key session secrets are derived from, actor counter. |

The format entry is `<store version>/<protocol version>`. A database with another format is not
opened; the prototype has no migrations, so a breaking change means a new data directory.

A client whose outgoing queue reaches 4096 messages is disconnected and resumes later.
