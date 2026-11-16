# ADR 0003: The server stores JSON; the stored format changes by rule

Status: accepted, 2026-11-16.

## Context

The server keeps the project in one SQLite database (`project.db`): the event log, the receipts
of transactions, snapshots of the document, and the runs of the server. Until format 7 every one
of these was postcard, and the store refused any database whose format differed from
`<store version>/<protocol version>` ("Formats are not migrated yet").

Postcard is positional: a value carries no field names, so adding one field to `Clip` makes every
stored project unreadable. Time signatures, automation and audio clips will all add fields, and a
project people have worked on must outlive the upgrade that adds them.

Measured on the real types:

| | JSON | postcard |
|---|---|---|
| A project of 100 000 notes | 6.1 MB | 1.2 MB |
| Encoding it | 7 ms | 2 ms |
| Decoding it | 16 ms | 6 ms |
| A small event | 340 bytes, 0.4 µs | |

A snapshot is written once per 1000 events, so the larger and slower snapshot costs little; an
event's encoding is far below the commit itself (about 0.25 ms, `tests/commit_bench.rs`).

## Decision

- What the server keeps on disk (`events.data`, `snapshots.data`, `receipts.reject`,
  `meta.runs`) is JSON, written by serde from the model and protocol types as they are. There
  are no separate stored copies of the types.
- **Free changes**, no version bump, no migration:
  - adding a struct field that has `#[serde(default)]`;
  - adding an enum variant;
  - removing a struct field (old rows keep the key, which is ignored).
- **Every other change** (renaming or moving a field or variant, changing a field's type or
  meaning, removing an enum variant) bumps `STORE_VERSION` and adds one migration step. A step
  works on the untyped JSON values, so it needs no frozen copy of the old types.
- A removed field's name is never used again with another meaning.
- On the stored types, never use `#[serde(skip_serializing_if)]`, `flatten`, `untagged` or
  `deny_unknown_fields`: the first three break postcard on the wire, the last breaks the free
  removal of a field.
- The wire is postcard and positional. Any change to a wire type, free or not, bumps
  `PROTO_VERSION`; client and server must match. Only the server's disk has to outlive upgrades.
- The store's `format` entry is the store version alone, a decimal number; the first migratable
  format is 8. A store older than version 8 (including the old `7/11` entries), or newer than the
  server, is refused.

The client's outbox stays postcard, tied to the protocol version.

## Adding a migration

Bump `STORE_VERSION` in `crates/mdaw-server/src/store.rs` and append one `Step` to `STEPS` in
`crates/mdaw-server/src/migrate.rs`; a compile-time check keeps the two in step. Write the step's
two functions, one for an event row and one for a snapshot row, against `serde_json::Value`,
never against the current types. Add a test that feeds a literal row of the old version through
the step and decodes the result with the current types. When the server opens an older store it
first copies the database to `project.db.v<old version>` (unless that copy exists), then runs
every step from the store's version on in one transaction, checks that every row decodes, and
writes the new version; if any row does not decode, nothing changes and the server refuses to
start.

## Consequences

- Adding a field with a default, the common case, needs nothing but the field.
- Stored rows are larger (about five times for a snapshot) and can be read with any JSON tool.
- Stores written before this decision are not converted: move the data directory away to start
  a new project, as with every earlier version.
- **The ceiling.** A snapshot is one SQLite value, and SQLite's default limit for one value is
  1 GB. At about 61 bytes per note as JSON that is roughly 16 million notes; the wire's limit for
  a snapshot frame (256 MiB of postcard, about 12 bytes per note) is reached at roughly 22
  million. The model's limits per clip and per track allow more than either. A snapshot that
  cannot be stored is logged and the server goes on (it then replays more of the log when it
  starts); a snapshot that cannot be sent keeps new clients out. A limit on the whole project's
  size has to cover both and is not built yet.
