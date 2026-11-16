# ADR 0004: The session stays on the UI thread

Status: deferred, 2026-11-16.

## Context

The client's session (receiving from the network, the replica, the outbox of unsent edits, the
hand-off to the engine) runs inside the UI frame: `Session::frame` is called every frame from
`App`. The audit of 2026-11-16 proposed moving it to its own thread, with the UI sending it
commands and drawing snapshots it publishes.

## Decision

Not now. The one cost of the session in the frame that could be measured was waiting for the
disk: the outbox forced its file to the disk (`sync_data`) every 250 ms while edits were
unconfirmed, rewrote it about once a second, and synced every saved plugin state inline. Those
syncs moved to a thread owned by the outbox (foundation plan, T7); the UI thread only writes into
the page cache. Everything else stays where it is.

## When to take it up again

When one of these shows:

- a frame over 16 ms that a trace puts in `Session::frame`;
- the delay with which the client receives while its window is hidden (about 250 ms) matters to
  users;
- a second window or view needs the same session.

## Size

About 78 uses of `app.session` in `src/ui` and `src/app.rs`, and every UI test, which drives the
session through the frame.
