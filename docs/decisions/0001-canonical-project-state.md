# ADR 0001: The Rust project is canonical; the Tracktion Edit is derived

Status: accepted, 2026-11-12, after the second look below. Feasibility shown by the spike; the
bridge (phase 4) is the proof.

## Question

Shared content needs one authority. Tracktion has its own project format, the Edit (a
`ValueTree` saved as XML). We have a Rust model with stable IDs and operations. Which is the
project?

## Options

**A. Rust model canonical, Tracktion Edit derived.** The project is the Rust model and its
operation log. Each client builds an in-memory Edit from it and applies each accepted operation
to the matching Tracktion object. The Edit is never the saved or synchronised form.

**B. Tracktion Edit primary, with a revisioned representation for collaboration.** The Edit's
ValueTree is the project; collaboration syncs ValueTree changes (or the server stores Edit XML
with revisions).

**C. Both, kept in step in both directions.** UI edits go to either; changes in the Edit are
turned back into operations.

## Comparison

| | A | B | C |
|---|---|---|---|
| Server needs Tracktion | no | yes, to validate and merge — or it stores XML it cannot check | no |
| Stable IDs | ours, already there | Tracktion's `EditItemID`s are per-edit counters; two clients creating things offline collide | two ID spaces to reconcile |
| Conflict policy per operation | on our typed ops | on generic tree property/child changes; "delete blocks stale update" has to be rebuilt on top | both |
| Format stability | ours | tied to Tracktion's XML across its versions | both |
| What Tracktion writes on its own (plugin window positions, caches, transient properties) | never enters the project | must be filtered out of sync | must be filtered |
| Work | a mapping layer, one direction | less mapping; a ValueTree sync protocol and a server that understands it | most |
| Risk | mapping lags Tracktion features we want | rule "Tracktion must never be a server dependency" is hard to keep | the fragile dual model |

## Decision

**A.** It is the only option where the server stays plain Rust, the IDs we already have keep
working, and the conflict rules are written against operations that mean something
("set note velocity") rather than tree edits.

One exception to "one direction": things that originate inside the engine. A knob turned in a
plugin's own window, or a plugin's state blob, starts in Tracktion. The bridge reports these as
*events*; Rust turns them into ordinary operations (`DeviceSetParam`, `DeviceSetState`) and they
go through the same transaction path as any other edit. The Edit is still never read back as
the project.

## How it works

- **Mapping.** The bridge keeps a table from our `EntityId` to the Tracktion object
  (`EditItemID`) for tracks, clips, plugins. Notes are addressed inside their clip by our note
  ID, kept in a per-clip table. Nothing is addressed by position in a list.
- **Applying.** Local edit → `ProjectOperation` → transaction → canonical project (optimistic)
  → the same op applied to the Edit. When the server's order differs, the replica rebases and
  the bridge receives the corrective ops. There is no "rebuild the whole Edit" on each edit;
  a full rebuild is the fallback after a snapshot.
- **Time.** We keep 960 PPQ ticks. Tracktion positions are beats (`BeatPosition`, a double);
  tick/960 is exact for every value we produce. The tempo map and (new) time signatures map to
  `TempoSequence`.
- **Plugin state.** A device holds: format, plugin identifier, parameter values by the plugin's
  parameter ID, and an opaque state blob. Blobs above a small size go to the asset store and
  the model holds the hash. State is read from the engine only after a processed block
  (integration doc, point 8).
- **Assets.** Audio files are content-addressed blobs (the existing blob store). The bridge is
  handed a local file path once the blob is present.
- **Not in the project.** Play/stop, playhead, loop range, selected device, window positions,
  meters.

## Evidence so far

- Edits to a playing Edit (add note, add track with instrument and clip) do not interrupt
  playback: 0 restarts, position keeps advancing.
- An Edit built in code, saved and reloaded, has the same content — so building from our model
  rather than from Tracktion's files loses nothing we use.
- Plugin state and parameters survive set → process → save → reload for 14 of 14 VST3 plugins
  and the CLAP plugins that persist state at all.

## Second look: judged for the future, not for the code that exists

The question was put again with one instruction: do not keep the current model because it is
there. So each option is judged as if nothing were written yet.

**What the product fixes regardless of this decision:** the UI is egui in Rust; the server is
Rust and never links Tracktion; several people edit at once, also offline, and reconnect.

**B, Tracktion Edit primary, taken seriously.** Its real attraction: every DAW feature
Tracktion has (audio clips, warp, comps, automation curves, clip effects, racks) would be in
the project the day we expose it, and a *generic* tree sync — set property, add / remove / move
child, by node ID — gives uniform conflict rules for free. Four things break it here:

1. *The UI is in Rust.* To draw a piano roll, Rust must hold the notes. Either it asks through
   the FFI for everything it draws (a wide, chatty binding to Tracktion's object model — the
   opposite of a small C ABI), or it keeps a Rust copy of the tree. A Rust copy that the UI
   reads and optimistic edits write *is* a second model; B then has the dual model it was
   meant to avoid.
2. *IDs.* Tracktion numbers items with a per-Edit counter. Two people creating a clip at the
   same time get the same ID. We would have to replace ID allocation inside Tracktion or remap
   on every rebase.
3. *The tree is Tracktion's private format.* It changes between Tracktion versions, the engine
   writes to it on its own (state flushes, fix-ups on load, transient properties), and its
   invariants are not written down. A merge of two valid concurrent edits can produce a tree
   no single Tracktion ever wrote. A server without Tracktion cannot check it; a client that
   loads it may assert or crash — and in a shared project one bad tree reaches everyone.
4. *Undo.* Tracktion's undo is one user's linear history. Per-user undo in a shared project has
   to be built on operations either way.

B is the right choice for a Tracktion-native app with a JUCE UI. It is not this app.

**C, a generic property tree in Rust** (entities with IDs, typed properties, ordered children;
one conflict rule for all). Cheaper per new feature than typed operations, and worth
remembering. Rejected for now: it gives up compile-time checking of what an edit may contain
and moves validation into schemas we would have to write anyway, and the document is small —
about 25 operations today, perhaps 45 with automation, audio clips and time signatures.

**A, judged fresh.** If we designed the Rust document today for this product we would want:
stable IDs that two offline clients can allocate without colliding; operations at property
level, addressed by ID, that never fail and are safe to apply twice; ordered lists by
"after which sibling" rather than index; opaque plugin state by content hash. That is what
`mdaw-model` already is. So it is kept because it is the design we would choose, and what does
not fit is cut out rather than carried:

- out: checkout scopes in operations and `apply`, stem-carrying commits, anything keyed to
  shared transport;
- changed: devices identify a plugin by format + plugin ID and parameters by the plugin's own
  parameter ID (today: CLAP only, numeric index);
- added: time signatures, automation, audio clips, stable transaction IDs.

**Cost of A, stated plainly:** each shared feature needs a model type, operations, and a
mapping to Tracktion. Tracktion features we do not model are not shared. That is the price of
a server and a UI that do not depend on the engine, and of a project format we own.

**Decision unchanged: A**, with the model trimmed and extended as listed, not preserved.

## What would change this

If the mapping layer turns out to need most of Tracktion's model mirrored in Rust (for example
if we want Tracktion's clip effects, warp time and comp editing all shared), B becomes cheaper
than A. Revisit if the model grows past what a derived mapping can carry.
