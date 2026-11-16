# Foundation plan

Written 2026-11-16 on branch `client-ui` (head `9e7525e`). It turns the audit of that day into
eleven tasks, in the order they should be done. Each task is one commit (T1 is two; T8 is one
or two). T2b, part of T7 and T9 come from an outside code review of `main` at `10989a8`, checked
against the code on the same day.

The plan is written to be carried out by someone who has not read the audit. Every task says
what to change, in which files, what not to touch, and how to tell it is done.

## Rules for whoever carries this out

1. **One task at a time, in order.** T6 depends on T1 and T2. The others are independent, but
   keep the order anyway.
2. **Stay inside the task.** Each task has a "Files" list and a "Do not" list. If the work seems
   to need a file that is not listed, stop and report instead of widening the task.
3. **Gates after every task**, all three green before committing:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo build -p mdaw-test-plugin
   env -u DISPLAY -u WAYLAND_DISPLAY cargo test --workspace
   ```

   The first cargo command may rebuild the native part (`target/native`); that can take minutes.
   Never run window tests on the real display. `editor_gui` is ignored by default; leave it so.
4. **A failing test is information.** If a test fails after a change this plan asked for, and
   the plan does not say that this test changes, stop and report the failure with its output. Do
   not edit the assertion, do not add a special case, do not mark it ignored.
5. **Commit on the current branch, never push.** Commit messages are one plain sentence saying
   what changed, like the existing ones (`git log --oneline -10`).
6. **Docs are part of the task.** Where a task lists doc edits, they go in the same commit.
7. **Line numbers** in this plan are those of commit `9e7525e`. They shift as tasks land: find
   the code by the function or text named, not by the number.
8. After T4, after T7 and after T10, write the hand-test section named in the task and **pause
   for the user's results** before going on.

## Decisions this plan makes

These were made while planning. Each can be overruled before the task starts.

| Decision | Why |
|---|---|
| The server stores events, snapshots, receipts and runs as JSON, not postcard. The wire stays postcard. | JSON carries field names, so adding or removing a field needs no migration, and a migration can patch untyped values without frozen copies of old types. Measured on the real types: a project of 100 000 notes is 6.1 MB, encodes in 7 ms and decodes in 16 ms (postcard: 1.2 MB, 2 ms, 6 ms); a small event is 340 bytes and 0.4 µs. A snapshot is written once per 1000 events. |
| Stores written before this plan (`7/11`) are not converted. | That is what every earlier version did. The first migratable format is 8. |
| Client and server must still run the same protocol version. | Unchanged. Only the server's disk needs to outlive upgrades. |
| The client's outbox stays postcard, tied to the protocol version. | Unchanged, out of scope. |
| The session stays on the UI thread. Only the disk syncs move off it (T7). | The full split touches about 78 call sites and every UI test; it is recorded as deferred in ADR 0004, not attempted here. |
| Plugin devices lose their `params` map; built-in devices keep theirs (T6). | The client writes `params` only for the built-in. Plugin settings travel as state blobs. |
| The track's shell/content split stays. | Harmless, and flattening it would touch most of the code base. |
| The `Audio` trait is not slimmed down. | Not in the six steps chosen. |
| `docs/migration-checklist.md` and `docs/retire-rust-engine.md` stay. | They are referenced by the ADRs, the release notes and CI. |
| Of ADR 0002 (plugin hosting), the crash marker (failure policy item 2) is built as the last task, T10, in the Rust client and without touching the bridge. Per-plugin process isolation is not built. | The marker is small and makes a crashing plugin survivable. Isolation is real-time C++ and a protocol between processes: a slice of its own with its own design and measurements. |
| The review's server findings (blob downloads held in memory, permissions of an existing data directory, no limit on the whole project's size) are not fixed here. | Valid, but server hardening for untrusted participants is a pass of its own. The size ceiling is written into ADR 0003 (T1a). |

---

## T1. A rule for how the stored format changes

**Why.** `crates/mdaw-server/src/store.rs` refuses any store whose format differs from
`<STORE_VERSION>/<PROTO_VERSION>` with "Formats are not migrated yet". Everything stored is
postcard, which is positional: adding one field to `Clip` makes every existing project
unreadable. Time signatures, automation and audio clips will all add fields.

**Outcome.** Stored data is JSON. Additive changes need nothing. Other changes are a numbered
migration step that runs when the store is opened. The store's version no longer contains the
protocol version.

Two commits: T1a (ADR and encoding), T1b (migration runner).

### T1a. JSON in the store, and the ADR

**Files**

- `docs/decisions/0003-stored-format.md` (new)
- `crates/mdaw-server/Cargo.toml`
- `crates/mdaw-server/src/store.rs`
- `crates/mdaw-server/tests/hub.rs` (one test)
- `crates/mdaw-server/tests/commit_bench.rs` (one comment)
- `docs/protocol.md` (the "store" section near the end)

**Steps**

1. Write `docs/decisions/0003-stored-format.md`, in the form of the two existing ADRs (title,
   status "accepted, 2026-11-16", context, decision, consequences). The decision section must
   state these rules word for word in substance:

   - What the server keeps on disk (`events.data`, `snapshots.data`, `receipts.reject`,
     `meta.runs`) is JSON, written by serde from the model and protocol types as they are.
   - **Free changes**, no version bump, no migration: adding a struct field that has
     `#[serde(default)]`; adding an enum variant; removing a struct field (old rows keep the
     key, which is ignored).
   - **Every other change** (renaming or moving a field or variant, changing a field's type or
     meaning, removing an enum variant) bumps `STORE_VERSION` and adds one migration step.
   - A removed field's name is never used again with another meaning.
   - On the stored types, never use `#[serde(skip_serializing_if)]`, `flatten`, `untagged` or
     `deny_unknown_fields`: the first three break postcard on the wire, the last breaks rule 2.
   - The wire is postcard and positional. Any change to a wire type, free or not, bumps
     `PROTO_VERSION`; client and server must match.
   - A store older than version 8, or newer than the server, is refused.

   Put the measured numbers from "Decisions this plan makes" in the context section.

   Add a consequence, "The ceiling": a snapshot is one SQLite value, and SQLite's default limit
   for one value is 1 GB. At about 61 bytes per note as JSON that is roughly 16 million notes;
   the wire's limit for a snapshot frame (256 MiB of postcard, about 12 bytes per note) is
   reached at roughly 22 million. The model's limits per clip and per track allow more than
   either. A snapshot that cannot be stored is logged and the server goes on (it then replays
   more of the log when it starts); a snapshot that cannot be sent keeps new clients out. A
   limit on the whole project's size has to cover both and is not built yet.

2. `crates/mdaw-server/Cargo.toml`: add `serde_json.workspace = true` under `[dependencies]`.
   Keep `postcard`: `hub.rs` still uses it to measure wire sizes.

3. `store.rs`: add two private helpers and use them at every place that now calls postcard.

   ```rust
   fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
       serde_json::to_vec(value).context("encoding for the store")
   }

   fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T> {
       serde_json::from_slice(bytes).with_context(|| format!("decoding {what}"))
   }
   ```

   The call sites, all in `store.rs`: `receipt` (the refusal), `append` (the event), `reject`,
   `events_after`, `put_snapshot`, `latest_snapshot`, `start_run` (read and write). After this,
   `store.rs` contains no `postcard`. The table definitions do not change (the columns stay
   `BLOB`).

4. `store.rs`: change the version and the check in `init`.

   - `const STORE_VERSION: u32 = 8;` and remove the `PROTO_VERSION` import if it is now unused.
   - The `format` entry in `meta` becomes the decimal version alone: `b"8"`.
   - Replace the `match store.meta("format")?` block with this behaviour:

     | Found | Do |
     |---|---|
     | nothing | write the current version |
     | text containing `/` (an old `7/11` style entry), or not a number | fail with the existing message, reworded: "the project was written in format {found}, from before formats could be migrated: move the data directory away to start a new project." |
     | a number equal to `STORE_VERSION` | nothing |
     | a number greater than `STORE_VERSION` | fail: "the project was written by a newer server (format {found}, this server reads up to {STORE_VERSION})." |
     | a number less than `STORE_VERSION` | T1b fills this in; for T1a fail with "format {found} cannot be migrated" (no such store can exist yet) |

   Keep the words "move the data directory away" in the old-format message: a test looks for
   them.

5. `tests/hub.rs`, the test around line 520 that sets `format` to `b"0/0"`: it must still pass
   unchanged. Add one test next to it: a store whose `format` is `b"999"` is refused and the
   error contains "newer server".

6. `store.rs`, add a unit test `stored_rows_are_json`: open `Store::in_memory()`, `append` one
   event, read the `data` column back with a raw query and assert that it parses with
   `serde_json::from_slice::<serde_json::Value>` and has the key `"seq"`.

7. `tests/commit_bench.rs` line 38: the comment says "as postcard"; make it "as stored".

8. `docs/protocol.md`, the table near line 370 and the paragraph near line 378: say JSON instead
   of postcard for `events`, `receipts` and `snapshots`; replace the "`7/11` ... no migrations"
   paragraph with: the format entry is the store version, now `8`; how it changes is in
   `docs/decisions/0003-stored-format.md`.

**Do not** change `PROTO_VERSION`, anything in `mdaw-proto`, `mdaw-model` or the client, or the
SQL tables.

**Done when** the gates pass, and
`cargo test --release -p mdaw-server --test commit_bench -- --ignored --nocapture` still reports
a median commit under 1 ms on this machine (put the number in the commit message).

### T1b. The migration runner

**Files**

- `crates/mdaw-server/src/migrate.rs` (new)
- `crates/mdaw-server/src/lib.rs` (declare the module, private)
- `crates/mdaw-server/src/store.rs`
- `docs/decisions/0003-stored-format.md` (one paragraph: how to add a step)

**Design**

```rust
//! crates/mdaw-server/src/migrate.rs
use serde_json::Value;

/// Brings stored rows from one store version to the next. Each function changes one decoded
/// row in place.
pub(crate) struct Step {
    /// Every row of `events.data` (an `Event`).
    pub event: fn(&mut Value),
    /// Every row of `snapshots.data` (a `Project`).
    pub project: fn(&mut Value),
}

/// The first version that can be migrated from.
pub(crate) const FIRST: u32 = 8;

/// `STEPS[i]` brings a store from version `FIRST + i` to `FIRST + i + 1`.
pub(crate) const STEPS: &[Step] = &[];
```

and a function

```rust
pub(crate) fn run(db: &rusqlite::Connection, from: u32, steps: &[Step]) -> anyhow::Result<()>
```

that does, inside **one** SQLite transaction (`db.unchecked_transaction()`, as `append` does):

1. For each step in `steps`, in order:
   - read every `(seq, data)` of `events`, parse `data` as `Value`, call `step.event`, write it
     back with `UPDATE events SET data = ?2 WHERE seq = ?1`;
   - the same for `snapshots` with `step.project`.
2. After the last step, check every row: each `events.data` must decode as
   `mdaw_proto::Event`, each `snapshots.data` as `mdaw_model::Project`. On the first that does
   not, return an error naming the table and `seq`; the transaction is dropped and nothing has
   changed.
3. Write `format` = `from + steps.len()` in the same transaction, then commit.

Read all rows of a table into a `Vec` first, then update: do not update while a query cursor on
the same table is open.

**Steps**

1. Add `migrate.rs` as above and `mod migrate;` in `lib.rs`.
2. `store.rs`: add `const _: () = assert!(STORE_VERSION == migrate::FIRST + migrate::STEPS.len() as u32);`
   so the version and the steps cannot drift apart.
3. `store.rs`, `init`: for a found version `n` with `migrate::FIRST <= n < STORE_VERSION`, call
   `migrate::run(&store.db, n, &migrate::STEPS[(n - migrate::FIRST) as usize..])`. A number
   below `FIRST` is refused with the old-format message.
4. `store.rs`, `Store::open` only (not `in_memory`): before `init`, if the file's format is a
   number below `STORE_VERSION`, copy the database first with
   `db.execute("VACUUM INTO ?1", [backup_path])`, where the backup is the database path with
   `.v<n>` appended (`project.db.v8`). If that file exists already, skip the copy. To read the
   format before `init`, the `meta` table may not exist yet in a new file: treat "no such
   table" as "no format".
5. Tests, as unit tests in `migrate.rs`, on `rusqlite::Connection::open_in_memory()` with the
   tables created by hand (copy the three `CREATE TABLE` statements needed):
   - `a_step_rewrites_every_row`: insert two events and one snapshot produced with
     `serde_json::to_vec` from real values (`Event` with one `Op::MasterSetGain`,
     `Project::default()`); run with a synthetic step whose `event` sets `wall_ts` to 7 and
     whose `project` sets `name` to `"migrated"`; assert both, and that `format` is `from + 1`.
   - `a_step_that_breaks_a_row_changes_nothing`: a synthetic step whose `event` removes the key
     `seq`; `run` returns an error, and the rows and `format` are as before.
   - `no_steps_only_checks`: `run(db, 8, &[])` on valid rows succeeds and writes `format` 8.
6. ADR 0003: add "Adding a migration" (bump `STORE_VERSION`, append one `Step` to `STEPS`, write
   the two functions against `serde_json::Value`, add a test that feeds a literal row of the
   old version through the step and decodes it with the current types).

**Do not** write a migration from `7/11`. Do not add a step yet: T6 adds the first.

**Done when** the gates pass.

---

## T2. One way from the model to the engine

**Why.** `mdaw-tracktion` carries the model into the engine in two ways: `sync_project` (by
difference, `src/sync.rs`) and `apply_op` (one op at a time, the 140-line `Sync::op` in
`src/apply.rs`). The client uses `apply_op` only twice, each time with an op it makes up for
the purpose (`Worker::retry` and `Worker::blob` in `crates/mdaw-client/src/audio_tracktion.rs`).
Two mappings must be kept equal by hand, and `sync.rs` builds a fake one-track project
(`single_track`) only to reuse a helper written for the op path.

**Outcome.** `apply_op` and `Sync::op` are gone. Two small functions replace the two real uses.
A missing blob is its own `Problem` variant instead of a matched error text.

One commit.

**Files**

- `crates/mdaw-tracktion/src/apply.rs`
- `crates/mdaw-tracktion/src/sync.rs`
- `crates/mdaw-tracktion/src/lib.rs` (re-exports and the module doc at the top)
- `crates/mdaw-tracktion/tests/engine.rs`
- `crates/mdaw-client/src/audio_tracktion.rs` (`Worker::retry`, `Worker::blob`,
  `Worker::publish_statuses` only)
- `docs/tracktion-integration.md`, `docs/architecture.md` (only sentences that name `apply_op`)

**Steps**

1. **Tests first.** In `tests/engine.rs`, `World::op` (near line 214) becomes: clone the model,
   apply the op to the model, then call
   `sync_project(&self.engine, &before, &self.model, &blobs)` and return its problems (panic on
   an error as now). Delete `World::engine_op`, the function `repeated_ops_are_idempotent` and
   its call in `the_engine_follows_the_model`. Remove `apply_op` from the imports. Run
   `cargo test -p mdaw-tracktion --test engine`. **If anything fails, stop and report** (rule
   4): it would be a real difference between the two paths.

   One known, accepted difference: in `builtin_instrument`, the assertion
   `assert_eq!(state_calls, 3, ...)` after `set(world, id::SUB_ON, Some(1.0))` becomes
   `assert_eq!(state_calls, 4, ...)`. Through `sync_project` the same change makes one more
   engine call: the `engine.has` per device in the ordering step of `sync.rs`'s `devices`
   (the state is still read once and written once). Change only the number, and make the
   message say "is the device there, read the state, write it, is it there for the order".
   The same holds for the per-parameter loop over continuous parameters further down in
   `builtin_instrument`: its count `2` ("is the device there, set one parameter") becomes `3`,
   with the message "{}: is the device there, set one parameter, is it there for the order".
   Any other failure still means stop and report.

2. `apply.rs`: change `Sync::device_predecessor` to take the track's content instead of a
   project:

   ```rust
   pub(crate) fn device_predecessor(&self, content: &TrackContent, device: DeviceId) -> u64
   ```

   The body is the same from `let Some(index) = ...` on, using `content.devices`.

3. `sync.rs`, function `devices`: call `sync.device_predecessor(new, device.id)`; delete the
   `project_new` variable and the whole `single_track` function.

4. `apply.rs`: add two public functions next to `load_project`.

   ```rust
   /// Puts a device of `project` that the engine lacks into it, behind its nearest left
   /// neighbour the engine has: a plugin that was not installed, or failed, when the project
   /// was last brought to the engine. Does nothing if the project has no such device or the
   /// engine has it already.
   pub fn add_device(
       engine: &Engine,
       project: &Project,
       track: TrackId,
       device: DeviceId,
       blobs: BlobResolver<'_>,
   ) -> Result<Vec<Problem>, Error>

   /// Gives a plugin device the state `project` holds for it (its blob has arrived), then its
   /// parameter values on top, as on a load. Does nothing for a built-in device or one the
   /// engine lacks.
   pub fn restore_device_state(
       engine: &Engine,
       project: &Project,
       track: TrackId,
       device: DeviceId,
       blobs: BlobResolver<'_>,
   ) -> Result<Vec<Problem>, Error>
   ```

   `add_device` is the body of the `Op::DeviceAdd` arm of `Sync::op` (look the device up in
   `project`, `device_predecessor`, `put_device`), with the extra check
   `!engine.has(EntityKind::Device, raw_device(device))`. `restore_device_state` is the body of
   the `Op::DeviceSetState` arm (state `Some` → `put_state`, `None` → `device_reset_state`,
   then `put_params`). Both create a `Sync::new(engine, blobs)` and return `sync.problems`.

5. `audio_tracktion.rs`:
   - `Worker::retry`: replace the made-up `Op::DeviceAdd` and the `apply_op` call with
     `mdaw_tracktion::add_device(&self.engine, project, track, device, &blobs)`. The early
     returns before it that duplicate the new function's checks may go.
   - `Worker::blob`: replace the made-up `Op::DeviceSetState` and `apply_op` with
     `mdaw_tracktion::restore_device_state(&self.engine, project, track, id, &blobs)`.
   - Remove `Op` from the imports if nothing else uses it.

6. `apply.rs`: delete `apply_op`, `Sync::op`, `Sync::tempo_changed` and `track_predecessor`.
   Remove the imports this leaves unused (the compiler names them). Rewrite the module comment
   at the top of `apply.rs` and the one at the top of `lib.rs` so they describe `load_project`,
   `sync_project`, `add_device` and `restore_device_state`. In `lib.rs`, export the two new
   functions in place of `apply_op`.

7. **The typed problem.** In `apply.rs`, add to `Problem`:

   ```rust
   /// A plugin's saved state is not at hand yet (its blob has to be fetched). The device runs
   /// with the settings it has.
   StateMissing { track: TrackId, device: DeviceId, blob: BlobHash },
   ```

   `Sync::put_state` pushes `StateMissing` when the resolver returns `None`; `StateNotApplied`
   remains for a state the plugin refused, and its doc comment says only that. Add the
   `Display` arm (`"{device:?}: state {blob:?} is not at hand"`). In
   `Worker::publish_statuses`, the first loop becomes: `StateMissing { blob, .. }` → push to
   `wanted`; `StateNotApplied { reason, .. }` → the notice. The text match
   `reason.contains("not at hand")` is gone.

8. Docs: `grep -rn "apply_op" docs crates | grep -v docs/foundation-plan.md` must print
   nothing (this plan names `apply_op` itself and stays as it is). Reword the sentences found (the
   op path no longer exists; the engine follows the model by difference, and two functions put
   a single device or its state in).

**Do not** change what `sync_project` does, the bridge, or any other part of
`audio_tracktion.rs`.

**Done when** the gates pass, `grep -rn "apply_op\|single_track" crates` prints nothing, and
`grep -rn 'contains("not at hand")' crates` prints nothing (the words "not at hand" stay in
`StateMissing`'s doc comment and `Display` text; only the text match on them is gone).

---

## T2b. A reload that failed is not a project the engine holds

**Why.** `Worker::sync` in `crates/mdaw-client/src/audio_tracktion.rs` tries `sync_project`; if
that fails it clears the engine and loads the whole project (`load_project`). If the load fails
too, it tells the user, and then still runs `self.synced = Some(view)`. `self.synced` is what
the next difference is taken against. After a failed load the engine holds some unknown part of
the project, so the next sync skips everything the two views share and the engine stays
incomplete for good, while the document looks right.

**Outcome.** After a failed load, `self.synced` is `None`. `None` already means "load the whole
project next time" (see the `match &self.synced` above it), so no new state is needed.

One commit.

**Files**: `crates/mdaw-client/src/audio_tracktion.rs`, function `Worker::sync` only.

**Steps**

1. In the `Err(e)` arm of the second `load_project` (the one after the notice "The audio engine
   is reloading the project"), after its notice "The audio engine could not load the project":
   set `self.synced = None;`, publish no statuses, and `return`. The lines that follow in
   `sync` (`self.looks.remove`, `self.synced = Some(view)`, `reread_params`, `master_gain`,
   `publish_statuses`) then run only after a sync or a load that succeeded.
2. Before returning, mark every plugin device of `view` as failed for the UI, so the tracks do
   not claim to play: take `plugin_devices(&view)` and write
   `DeviceStatus::Failed("the audio engine could not load the project".into())` for each into
   `shared.statuses` (replace the map, as `publish_statuses` does).
3. Add a line to the comment above the `match result`: what `None` means after a failure.

There is no test: the engine cannot be made to fail a load on demand, and a switch for that is
not worth building for three lines. Say so in the commit message.

**Do not** add an enum for the engine's state, a retry timer, or a fault switch in the bridge.

**Done when** the gates pass.

---

## T3. Take history out of the tree

**Why.** The repository still carries a native test program from before the bridge existed and
plans for an engine that was removed. Git keeps all of it.

One commit. No Rust code changes.

**Delete**

- `native/spike/` (the whole folder)
- `docs/optimizations-plan.md`, `docs/plan.md`, `docs/pre-plan.md`
- `docs/manual-tests/m2.md`, `m3.md`, `m4.md`, `m5.md`, `m6.md`, `tracktion-spike.md`
- `docs/spikes/` (the whole folder)

**Edit**

- `native/CMakeLists.txt`: remove the `MDAW_BUILD_SPIKE` option and its `if` block (the last
  four lines), and reword the comment above `mdaw_engine_config` ("so that the spike and the
  bridge cannot drift apart" → every target built on the engine uses one list).
- `README.md`:
  - the list at the top: remove the bullets for `pre-plan.md`, `plan.md` and `docs/spikes/`;
  - "Status": replace the M0–M6 bullets and the two history paragraphs with one short paragraph
    on what exists today (several clients edit one project over QUIC; each plays it on its own
    machine through Tracktion Engine; VST3, LV2 and CLAP plugins; Linux only) and a pointer to
    `docs/architecture.md`. Keep the "Linux only" paragraph;
  - the build section: remove the sentence and code block about the Tracktion spike.
- `docs/tracktion-integration.md`: remove the build lines for `mdaw-te-spike` (near lines
  68–76) and the commands in "The spike" (near line 96). Keep the measured results of that
  section; start it with one sentence: the spike was a console program under `native/spike/`,
  removed on 2026-11-16 and still in git history before that commit.
- `docs/migration-checklist.md`: the row and the list entry for `spikes/clap-host` say the
  report "stays as history"; say it is in git history.
- `docs/retire-rust-engine.md`: leave as is (it is a record; its mentions of removed files are
  history).

**Do not** delete `docs/migration-checklist.md`, `docs/retire-rust-engine.md`,
`docs/manual-tests/tracktion-client.md`, `docs/manual-tests/plugin-windows.md`, the ADRs or the
release notes.

**Done when**

- the gates pass (the native part reconfigures by itself);
- `cmake --build target/native --target mdaw-bridge-test` succeeds;
- this prints nothing:

  ```sh
  grep -rnE "mdaw-te-spike|native/spike|docs/plan\.md|pre-plan|optimizations-plan|docs/spikes|manual-tests/m[2-6]|tracktion-spike" \
    README.md docs native/CMakeLists.txt native/tracktion-bridge native/clap-format crates xtask .github \
    | grep -v "^docs/retire-rust-engine.md\|^docs/foundation-plan.md"
  ```

---

## T4. A playhead that moves every frame

**Why.** The bridge takes the transport position 30 times a second
(`native/tracktion-bridge/src/project.cpp`, `startTimerHz (30)`), and the client draws that raw
value (`TracktionAudio::position`). The window redraws 60 to 144 times a second while playing,
so the playhead moves in steps and most frames redraw it where it was. The engine already
reports how old the position is (`position_age_seconds`).

**Outcome.** While the device plays, the playhead is drawn where the transport is now: the
last reported position plus its age, through the tempo map, wrapped at the loop end.

One commit.

**Files**

- `crates/mdaw-client/src/audio.rs`
- `crates/mdaw-client/src/audio_tracktion.rs`
- `crates/mdaw-client/src/session.rs`
- `docs/manual-tests/foundation.md` (new)

**Steps**

1. `audio.rs`, trait `Audio`, after `position`:

   ```rust
   /// The playhead in ticks as the engine last reported it, and how many seconds ago that was.
   /// The age is 0 when the position is exact or the transport stands.
   fn position_aged(&self) -> (f64, f64) {
       (self.position(), 0.0)
   }
   ```

   `SilentAudio` keeps the default: its position is exact.

2. `audio_tracktion.rs`: add a constant and the method on `impl Audio for TracktionAudio`.

   ```rust
   /// A reported position older than this is not carried forward: the engine's thread is
   /// busy, and the playhead waits for it rather than run ahead on a guess.
   const POSITION_AGE_MAX: f64 = 0.1;

   fn position_aged(&self) -> (f64, f64) {
       if let Some((tick, _)) = self.seek.get() {
           return (tick as f64, 0.0);
       }
       let state = self.read();
       let moving = state.playing && state.device_open;
       let age = if moving && state.position_age_seconds <= POSITION_AGE_MAX {
           state.position_age_seconds.max(0.0)
       } else {
           0.0
       };
       (state.position_tick as f64, age)
   }
   ```

   `device_open` matters: without a device the transport does not move, and carrying a standing
   position forward would make it jitter.

3. `session.rs`: a free function and its use.

   ```rust
   /// Where a transport that was at `tick`, `age` seconds ago, is now.
   fn carried_forward(tempo: &TempoMap, loop_range: Option<TickRange>, tick: f64, age: f64) -> f64 {
       if age <= 0.0 {
           return tick;
       }
       let now = tempo.seconds_to_tick(tempo.tick_to_seconds(tick) + age);
       match loop_range {
           Some(range) if tick < range.end.0 as f64 && now >= range.end.0 as f64 => {
               let length = (range.end.0 - range.start.0) as f64;
               range.start.0 as f64 + (now - range.end.0 as f64) % length
           }
           _ => now,
       }
   }
   ```

   `Session::position` becomes:

   ```rust
   pub fn position(&self) -> f64 {
       let (tick, age) = self.audio.position_aged();
       carried_forward(&self.tempo, self.loop_range, tick, age)
   }
   ```

   As of `9e7525e`: `TickRange { pub start: Tick, pub end: Tick }`, `Tick(pub i64)`,
   `TempoMap::tick_to_seconds(&self, tick: f64) -> f64` and
   `TempoMap::seconds_to_tick(&self, seconds: f64) -> f64`, all in
   `crates/mdaw-model/src/time.rs`. Do not change that file. `Session::tick_at` (recording) is not touched.

4. Unit tests in `session.rs` (add a `#[cfg(test)] mod tests` if there is none) for
   `carried_forward`, with `TempoMap::from_points`:
   - age 0 returns the tick unchanged;
   - at 120 bpm, 0.5 s on from tick 0 is `PPQ` ticks (one beat), within 1e-6;
   - with a loop from bar 0 to bar 1, a tick 10 before the loop end carried 30 ticks' worth of
     time lands 20 ticks after the loop start;
   - a tick already past the loop end is not wrapped.

5. Create `docs/manual-tests/foundation.md` in the form of
   `docs/manual-tests/tracktion-client.md` (start, numbered checkboxes, what to report), with
   section **A. Playhead**:
   - A.1 Start `cargo xtask client Ann --release`, put some notes in a clip, play. The playhead
     in the arrangement and in the piano roll moves smoothly, without steps.
   - A.2 Loop one bar and play for ten rounds. The playhead never shows past the loop end and
     never jumps backwards except at the wrap.
   - A.3 Stop. The playhead stands still (no trembling).
   - A.4 Press play, then seek by clicking the ruler while playing. The playhead goes where
     clicked and continues from there.
   - A.5 Record a few notes with the computer keyboard while playing; they land where they were
     played (recording timing is unchanged).

**Do not** change the bridge, the 30 Hz timer, `position_heard`, `tick_at` or any UI file.

**Done when** the gates pass. Then **pause**: ask the user to run section A.

---

## T5. A release profile

**Why.** `Cargo.toml` has no `[profile.release]`, so release builds use sixteen code generation
units and no link-time optimisation across the workspace's crates.

**Outcome.** Either a measured improvement and the profile, or no change.

**Files**: `Cargo.toml` only.

**Steps**

1. Before: `cargo build --release --locked -p mdaw-client -p mdaw-server`. Note the size of
   `target/release/mdaw` and `target/release/mdaw-server` (`ls -l`), and the median commit time
   from `cargo test --release -p mdaw-server --test commit_bench -- --ignored --nocapture`.
2. Add after `[profile.dev.package."*"]`:

   ```toml
   [profile.release]
   lto = "thin"
   codegen-units = 1
   ```

3. Build and measure again, the same way.
4. **Keep the change only if** the build and the gates pass and at least one of the three
   numbers improved by 5 % or more with none worse by 5 %. Put the before and after numbers in
   the commit message. Otherwise revert `Cargo.toml` and report the numbers; there is then no
   commit for T5.

**Do not** set `target-cpu`, `panic`, `strip` or `opt-level`, and do not touch the dev profile
or the CMake flags.

---

## T6. Device settings by kind

**Why.** Every `Device` has a `params` map and, if it is a plugin, a `state` blob. The client
writes `params` only for the built-in instrument (`crates/mdaw-client/src/ui/devices.rs`,
`two_osc_param`); a plugin's settings travel as its state. So a plugin device has two places
for its settings, one of them never used, and `mdaw-tracktion` carries code for the unused one
(`put_params`, `put_param`, a branch in `sync.rs`). `dsp_version` on the built-in is read by
nothing.

**Outcome.**

```rust
pub struct Device {
    pub id: DeviceId,
    pub kind: DeviceKind,
    pub bypass: bool,
}

pub enum DeviceKind {
    /// The engine's own instrument: its settings are parameter values by parameter ID. A
    /// missing entry means the parameter's default.
    Builtin { kind: BuiltinKind, params: OrdMap<u32, f64> },
    /// A plugin installed on the client's machine: its settings are an opaque state blob.
    Plugin { format: PluginFormat, id: String, name: String, state: Option<BlobHash> },
}
```

Protocol 12, store 9 with the first real migration step.

One commit: the workspace does not compile between the steps, so do all of them, then commit.
Let the compiler lead: after step 1, `cargo check --workspace --all-targets` lists every site.

**Files**

- `crates/mdaw-model/src/doc.rs`, `op.rs`, `apply.rs`, `two_osc.rs`, `tests/properties.rs`
- `crates/mdaw-proto/src/lib.rs` (the version)
- `crates/mdaw-server/src/store.rs`, `src/migrate.rs`, and the server tests that build devices
- `crates/mdaw-sync/tests/replica.rs`
- `crates/mdaw-tracktion/src/apply.rs`, `src/sync.rs`, `tests/engine.rs`, and the two scan tests
  if they build a built-in device
- `crates/mdaw-client/src/session.rs`, `src/ui/devices.rs`, `src/outbox.rs` (its test helper),
  `src/bin/mdaw-slice.rs`, and the client tests the compiler names
- `docs/protocol.md`, `docs/decisions/0003-stored-format.md`

**Steps**

Work in this order. The workspace does not compile from step 1 until step 9 is done; that is
expected. After each step run the check named in it, which compiles only what that step
finished.

Every place that has to change is listed below, found by search at `9e7525e`. T2 and T2b will
have moved some lines; find the code by the text quoted. If the compiler names a place that is
not in these lists, fix it by the same patterns and name it in the report.

The three patterns that cover almost every site:

| Before | After |
|---|---|
| `Device::new(id, DeviceKind::Builtin { kind: BuiltinKind::TwoOsc, dsp_version: ... })` | `Device::builtin(id, BuiltinKind::TwoOsc)` |
| `device.params.insert(p, v)` on a built-in device | `device.params_mut().expect("a built-in device").insert(p, v)` |
| `device.params.get(&p).copied()` | `device.param(p)` |

A match or `matches!` on `DeviceKind::Builtin { .. }` or `DeviceKind::Builtin { kind: ..., .. }`
with `..` compiles unchanged. Nothing about `DeviceKind::Plugin` changes anywhere.

#### Step 1. The model's types: `crates/mdaw-model/src/doc.rs`

Replace `Device`, its `impl` and `DeviceKind` (the `Builtin` variant only; `Plugin` and its doc
comments stay word for word):

```rust
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub kind: DeviceKind,
    pub bypass: bool,
}

impl Device {
    pub fn new(id: DeviceId, kind: DeviceKind) -> Self {
        Self { id, kind, bypass: false }
    }

    /// A built-in device with every parameter at its default.
    pub fn builtin(id: DeviceId, kind: BuiltinKind) -> Self {
        Self::new(
            id,
            DeviceKind::Builtin {
                kind,
                params: OrdMap::new(),
            },
        )
    }

    /// The parameter values of a built-in device; `None` for a plugin, whose settings are
    /// its state.
    pub fn params(&self) -> Option<&OrdMap<u32, f64>> {
        match &self.kind {
            DeviceKind::Builtin { params, .. } => Some(params),
            DeviceKind::Plugin { .. } => None,
        }
    }

    pub fn params_mut(&mut self) -> Option<&mut OrdMap<u32, f64>> {
        match &mut self.kind {
            DeviceKind::Builtin { params, .. } => Some(params),
            DeviceKind::Plugin { .. } => None,
        }
    }

    /// The value a built-in device holds for a parameter; `None` if it is at its default, or
    /// the device is a plugin.
    pub fn param(&self, id: u32) -> Option<f64> {
        self.params()?.get(&id).copied()
    }
}
```

```rust
    /// The engine's own instrument: only its parameter values are synced, by parameter ID. A
    /// missing entry means the parameter's default.
    Builtin {
        kind: BuiltinKind,
        params: OrdMap<u32, f64>,
    },
```

#### Step 2. The model's ops: `crates/mdaw-model/src/apply.rs`, `op.rs`, `two_osc.rs`

`apply.rs`, the `Op::DeviceSetParam` arm becomes:

```rust
        Op::DeviceSetParam {
            track,
            id,
            param,
            value,
        } => {
            // A plugin device has no parameters in the document: `params()` is `None` there,
            // and the op does nothing.
            let old = project
                .tracks
                .get(track)?
                .content
                .device(*id)?
                .params()?
                .get(param)
                .copied();
            if old == *value {
                return None;
            }
            let params = device_mut(project, *track, *id)?.params_mut()?;
            match value {
                Some(v) => params.insert(*param, *v),
                None => params.remove(param),
            };
            Some(Op::DeviceSetParam {
                track: *track,
                id: *id,
                param: *param,
                value: old,
            })
        }
```

`op.rs`:

- Doc comment of `Op::DeviceSetParam`: "Sets a parameter of a built-in device; `value: None`
  returns it to its default. Does nothing on a plugin device, whose settings are its state."
- Doc comment of `Op::DeviceSetState`: "Sets a plugin device's saved state. Does nothing on a
  built-in device."
- In `fits`, the last arm of `let full = match self` (the one for `Op::DeviceSetParam`): its
  closure becomes

  ```rust
  .is_some_and(|d| {
      d.params()
          .is_some_and(|p| !p.contains_key(param) && p.len() >= MAX_PARAMS_PER_DEVICE)
  }),
  ```

  The block above it in `fits` (the `two_osc::check` for a `TwoOsc` device) compiles
  unchanged.
- `check_device` becomes:

  ```rust
  fn check_device(device: &Device) -> Result<(), Invalid> {
      match &device.kind {
          DeviceKind::Plugin { id, name, .. } => {
              if id.is_empty() || id.len() > MAX_PLUGIN_ID_BYTES {
                  return Err(Invalid("bad plugin id"));
              }
              if name.len() > MAX_NAME_BYTES {
                  return Err(Invalid("plugin name too long"));
              }
          }
          DeviceKind::Builtin { kind, params } => {
              if params.len() > MAX_PARAMS_PER_DEVICE {
                  return Err(Invalid("too many parameters"));
              }
              if !params.values().all(|v| v.is_finite()) {
                  return Err(Invalid("parameter value is not finite"));
              }
              match kind {
                  BuiltinKind::TwoOsc => {
                      for (param, value) in params {
                          two_osc::check(*param, *value)?;
                      }
                  }
              }
          }
      }
      Ok(())
  }
  ```

`two_osc.rs`:

- Delete the constant `DSP_VERSION` and its doc comment (lines 13–14).
- In the test `ops_with_values_out_of_range_are_refused` (near line 514): replace the `kind`
  variable and `Device::new(device, kind)` with `Device::builtin(device, BuiltinKind::TwoOsc)`,
  the two `synth.params.insert(...)` with the second pattern, and remove `DeviceKind` from the
  test's `use` if it is then unused.

Check: `cargo check -p mdaw-model` (the library only; its tests come next).

#### Step 3. The model's tests: `crates/mdaw-model/tests/properties.rs`

The device generator (near line 75). Parameters now belong to the built-in kind:

```rust
fn device() -> impl Strategy<Value = Device> {
    let kind = prop_oneof![
        prop::collection::vec(param(), 0..3).prop_map(|params| DeviceKind::Builtin {
            kind: BuiltinKind::TwoOsc,
            params: params.into_iter().collect(),
        }),
        prop::option::of(any::<[u8; 32]>().prop_map(BlobHash)).prop_map(|state| DeviceKind::Plugin {
            format: PluginFormat::Clap,
            id: "test.plugin".into(),
            name: "Synth".into(),
            state
        }),
    ];
    (device_id(), kind, any::<bool>()).prop_map(|(id, kind, bypass)| Device { id, kind, bypass })
}
```

Shorten the comment above `param()` that ends "so that a device carrying them is valid whatever
its kind": they are valid for the built-in instrument. Nothing else in the file changes: the
`DeviceSetParam` generator and the invalid-op list stay.

Check: `cargo test -p mdaw-model`. All property tests must pass as they are. If one fails,
stop and report (rule 4).

#### Step 4. The protocol version: `crates/mdaw-proto/src/lib.rs`

`pub const PROTO_VERSION: u32 = 12;`. In the same crate, `src/wire.rs` has a test with the
literal `proto_version: 8`; leave it.

Check: `cargo test -p mdaw-proto -p mdaw-sync --no-run` will fail in `mdaw-sync`'s test; fix it
now:

`crates/mdaw-sync/tests/replica.rs`:

- near line 32, the `Device::new(DEVICE(), DeviceKind::Builtin { ... })` → first pattern;
- the helper `fn param(project: &Project) -> Option<f64>` (near line 96): its chain
  `.device(DEVICE())?.params.get(&two_osc::id::FILTER_FREQ).copied()` becomes
  `.device(DEVICE())?.param(two_osc::id::FILTER_FREQ)`;
- remove imports the compiler then reports unused.

Check: `cargo test -p mdaw-proto -p mdaw-sync`.

#### Step 5. The store: `crates/mdaw-server/src/store.rs` and `src/migrate.rs`

`store.rs`: `const STORE_VERSION: u32 = 9;`.

`migrate.rs`: add the step for 8 → 9. A device as stored in version 8 (from the real encoder):

```json
{"id":4294967298,"kind":{"Builtin":{"kind":"TwoOsc","dsp_version":1}},"params":{"3":0.25},"bypass":false}
{"id":4294967299,"kind":{"Plugin":{"format":"Clap","id":"com.x.y","name":"Y","state":[7,7,7]}},"params":{},"bypass":false}
```

and in version 9:

```json
{"id":4294967298,"kind":{"Builtin":{"kind":"TwoOsc","params":{"3":0.25}}},"bypass":false}
{"id":4294967299,"kind":{"Plugin":{"format":"Clap","id":"com.x.y","name":"Y","state":[7,7,7]}},"bypass":false}
```

The code, complete:

```rust
/// Version 8 to 9: a device's parameter values move into the built-in kind; a plugin
/// device's (never used) go; `dsp_version` goes.
fn device_8_to_9(device: &mut Value) {
    let params = device.as_object_mut().and_then(|d| d.remove("params"));
    if let Some(builtin) = device
        .pointer_mut("/kind/Builtin")
        .and_then(Value::as_object_mut)
    {
        builtin.remove("dsp_version");
        builtin.insert("params".into(), params.unwrap_or_else(|| serde_json::json!({})));
    }
}

/// The devices of a track's content, as stored (`{"gain":..,"devices":[..],..}`).
fn content_8_to_9(content: &mut Value) {
    if let Some(devices) = content.get_mut("devices").and_then(Value::as_array_mut) {
        devices.iter_mut().for_each(device_8_to_9);
    }
}

/// An op is stored as `{"<Variant>": {..}}`. Two variants carry devices.
fn op_8_to_9(op: &mut Value) {
    if let Some(content) = op.pointer_mut("/TrackAdd/content") {
        content_8_to_9(content);
    }
    if let Some(device) = op.pointer_mut("/DeviceAdd/device") {
        device_8_to_9(device);
    }
}

fn event_8_to_9(event: &mut Value) {
    if let Some(ops) = event.get_mut("ops").and_then(Value::as_array_mut) {
        ops.iter_mut().for_each(op_8_to_9);
    }
    if let Some(superseded) = event.get_mut("superseded").and_then(Value::as_array_mut) {
        for entry in superseded {
            if let Some(lost) = entry.get_mut("lost") {
                op_8_to_9(lost);
            }
        }
    }
}

fn project_8_to_9(project: &mut Value) {
    if let Some(tracks) = project.get_mut("tracks").and_then(Value::as_object_mut) {
        for track in tracks.values_mut() {
            if let Some(content) = track.get_mut("content") {
                content_8_to_9(content);
            }
        }
    }
}
```

and `STEPS` becomes
`&[Step { event: event_8_to_9, project: project_8_to_9 }]`.

Tests in `migrate.rs`:

- `devices_move_from_8_to_9`: parse the two literal rows below, run `project_8_to_9` and
  `event_8_to_9` on them, decode with `serde_json::from_value::<Project>` and `::<Event>`, and
  assert:
  - in the project: the track's first device has `param(3) == Some(0.25)`; the second is a
    `DeviceKind::Plugin` with `state: Some(_)` and `params()` is `None`;
  - in the event: `ops[0]` is `Op::TrackAdd` whose content's first device has
    `param(3) == Some(0.25)`; `ops[1]` is `Op::DeviceAdd` with a plugin device;
    `superseded[0].lost` is `Op::DeviceSetParam { param: 3, value: Some(0.5), .. }`.

  ```json
  {"name":"Untitled","tempo":{"4294967301":{"tick":0,"milli_bpm":120000}},"master_gain":1.0,"track_order":[4294967297],"tracks":{"4294967297":{"shell":{"name":"T","color":255},"content":{"gain":1.0,"pan":0.0,"mute":false,"clips":{},"devices":[{"id":4294967298,"kind":{"Builtin":{"kind":"TwoOsc","dsp_version":1}},"params":{"3":0.25},"bypass":false},{"id":4294967299,"kind":{"Plugin":{"format":"Clap","id":"com.x.y","name":"Y","state":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}},"params":{},"bypass":false}]}}}}
  ```

  ```json
  {"seq":1,"actor":1,"user":1,"wall_ts":5,"txn":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,9],"ops":[{"TrackAdd":{"id":4294967297,"shell":{"name":"T","color":255},"content":{"gain":1.0,"pan":0.0,"mute":false,"clips":{},"devices":[{"id":4294967298,"kind":{"Builtin":{"kind":"TwoOsc","dsp_version":1}},"params":{"3":0.25},"bypass":false}]},"after":null}},{"DeviceAdd":{"track":4294967297,"device":{"id":4294967299,"kind":{"Plugin":{"format":"Clap","id":"com.x.y","name":"Y","state":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}},"params":{},"bypass":false},"after":4294967298}}],"superseded":[{"entity":{"Device":[4294967297,4294967298]},"prop":{"Param":3},"lost":{"DeviceSetParam":{"track":4294967297,"id":4294967298,"param":3,"value":0.5}},"lost_actor":2,"lost_user":2,"lost_seq":1}]}
  ```

  Put each in the test as a raw string, `r#"..."#`.

- `a_version_8_store_opens`: on `rusqlite::Connection::open_in_memory()` with the tables made
  as in T1b's tests, insert `format` = `b"8"`, the event above into `events` at seq 1 and the
  project above into `snapshots` at seq 0; call `run(&db, 8, STEPS)`; assert `format` is
  `b"9"` and that both rows decode with the current types.

Check: `cargo test -p mdaw-server --lib`.

#### Step 6. The server's tests: `crates/mdaw-server/tests/`

- `sim.rs`, in the test that ends near line 716, delete the one line
  `assert_eq!(plugin.params.get(&1), Some(&9.0));`. That test sets a parameter on a *plugin*
  device (edit kind 16) and checked that the later writer won; a plugin device has no
  parameters in the document any more, and the same rule is still checked there by the gain,
  the note's pitch and the state. Leave edit kind 16 in the generator: on a plugin device it
  now changes nothing, and `Replica::submit` drops an edit that changes nothing, which the
  simulation already handles for other kinds.
- `edits.rs`, `hub.rs`, `receipts.rs`, `sequencer.rs`: they build plugin devices only and match
  `DeviceKind::Builtin { .. }` with `..`; they should compile unchanged. If one does not, apply
  the patterns.

Check: `cargo test -p mdaw-server`. The simulation and property tests there run many random
edit sequences; all must pass. If one fails, stop and report.

#### Step 7. The engine mapping: `crates/mdaw-tracktion/src/apply.rs` and `sync.rs`

`apply.rs`:

1. In `put_device`, the last `match &device.kind` becomes:

   ```rust
   match &device.kind {
       DeviceKind::Builtin { .. } => self.put_builtin(track, device)?,
       DeviceKind::Plugin { state, .. } => {
           if let Some(blob) = state {
               self.put_state(track, device.id, *blob);
           }
       }
   }
   ```

2. Delete the functions `put_params` and `put_param` whole.
3. In `restore_device_state` (added by T2): delete the call to `put_params`, and the words
   "then its parameter values on top, as on a load" from its doc comment.
4. `put_builtin_changes`: its opening `let DeviceKind::Builtin { ... } = &device.kind else`
   binds the map, and the values are read from it:

   ```rust
   let DeviceKind::Builtin {
       kind: BuiltinKind::TwoOsc,
       params: held,
   } = &device.kind
   else {
       return Ok(());
   };
   let raw = raw_device(device.id);
   let values = TwoOscValues::from_map(held);
   ```

5. `put_two_osc`: start with
   `let Some(held) = device.params() else { return Ok(()) };`, use
   `TwoOscValues::from_map(held)`, and the last loop iterates `held.keys()`.
6. `put_builtin` compiles unchanged (its match has `..`).
7. Doc comment of `Problem::ParamNotMapped`: "A parameter the built-in instrument does not
   have." Remove `ParamInfo` from the `use` at the top if it is now unused.
8. The module comment at the top: delete the paragraph that begins "A plugin device's settings
   are its state blob and its parameter values" and put in its place: "A plugin device's
   settings are its state blob. The built-in instrument's are its parameter values."

`sync.rs`, function `devices`, the loop under the comment "Changed settings of those that
stayed": keep everything up to and including the `device_set_bypass` call, and replace the
`match &device.kind { ... }` that follows with:

```rust
        match (&was.kind, &device.kind) {
            (DeviceKind::Builtin { params: old, .. }, DeviceKind::Builtin { params: new, .. }) => {
                if old != new {
                    let changed: Vec<u32> = old
                        .diff(new)
                        .map(|item| match item {
                            DiffItem::Remove(param, _) | DiffItem::Add(param, _) => *param,
                            DiffItem::Update { new: (param, _), .. } => *param,
                        })
                        .collect();
                    sync.put_builtin_changes(track, device, changed)?;
                }
            }
            (DeviceKind::Plugin { state: old, .. }, DeviceKind::Plugin { state: new, .. }) => {
                if old != new {
                    match new {
                        Some(blob) => sync.put_state(track, device.id, *blob),
                        None => engine.device_reset_state(raw_device(device.id))?,
                    }
                }
            }
            // Another kind under the same id was removed and put anew above (`renew`).
            _ => {}
        }
```

Check: `cargo check -p mdaw-tracktion` (library only).

#### Step 8. The engine tests: `crates/mdaw-tracktion/tests/engine.rs`

| Near line | Text to find | Change |
|---|---|---|
| 265–272 | `let mut synth = Device::new(ids.synth, DeviceKind::Builtin {` ... `synth.params.insert(two_osc::id::OSC1_WAVE, 1.0);` | `let mut synth = Device::builtin(ids.synth, BuiltinKind::TwoOsc);` and the second pattern for the insert |
| 582–589 | `device: Device::new(extra, DeviceKind::Builtin {` | `device: Device::builtin(extra, BuiltinKind::TwoOsc),` |
| 885–895 | the closure `let instrument = \|id\| {` | `let mut device = Device::builtin(id, BuiltinKind::TwoOsc);` and the second pattern for the insert |
| 1249–1252 | `let kind = DeviceKind::Builtin { kind: BuiltinKind::TwoOsc, dsp_version: ... };` | `let kind = DeviceKind::Builtin { kind: BuiltinKind::TwoOsc, params: Default::default() };` (it is cloned into `Device::new(synth, kind.clone())` further down, which stays) |
| 1552–1555 | three `device.params.insert(id::..., ...)` | the second pattern, three times |

Then, in the function `plugins` (starts near line 1593), delete the part that sets the CLAP
test synth's parameters through the document. It starts at the comment line

```rust
    // Level 0.1 (plain = normalised for 0..1): a fifth of the default level.
```

and ends with the `assert!` whose message begins `"clap: back to the default level`. Delete
both and everything between. That part ended by putting both parameters back to their
defaults, so the code after it sees the same engine as before.

Also delete the block at the end of `plugins` that begins with the comment

```rust
    // A stored parameter value stays on top of a reset, as on a load.
```

and ends with the `assert!` whose message begins `"clap: model value kept over the reset`.
It uses the `set` closure of the part above and checks that a plugin's parameter values go on
top of its state, which this task removes. Everything before it stays (the
parameter list, `device_param_text`).

After the deletion the compiler may report unused variables (`balance`, `base_right`, `raw`,
`missing`): if a variable is still used further down, keep it; if not, remove its `let` and,
for `balance`, keep the two assertions about its range and text by inlining the lookup. Do not
silence a warning with a leading underscore.

The scan tests `tests/scan_concurrency.rs` and `tests/scanner.rs` build plugin devices only and
should compile unchanged.

Check: `cargo test -p mdaw-tracktion`. This runs the engine and takes a while. If an assertion
after the deleted part fails, stop and report: it means that part was not self-contained.

#### Step 9. The client: `crates/mdaw-client`

Sources:

- `src/session.rs`, `builtin_kind` (near line 1141) becomes:

  ```rust
  /// A built-in device of this kind, as a new one is made: every parameter at its default.
  pub fn builtin_kind(kind: BuiltinKind) -> DeviceKind {
      DeviceKind::Builtin {
          kind,
          params: imbl::OrdMap::new(),
      }
  }
  ```

  It stays public: `src/bin/mdaw-slice.rs` and `tests/playback.rs` call it. Nothing else in
  `session.rs` changes (`is_instrument`, `new_track` and `use_builtin` compile as they are).
- `src/ui/devices.rs`, `two_osc_panel` (near line 282):
  `let values = two_osc::Values::from_map(&device.params);` becomes

  ```rust
  let Some(params) = device.params() else { return };
  let values = two_osc::Values::from_map(params);
  ```

- `src/audio_silent.rs`, `src/audio_tracktion.rs`, `src/bin/mdaw-slice.rs`, `src/outbox.rs`:
  only matches with `..` and plugin devices; they should compile unchanged.

Tests:

| File | Near line | Change |
|---|---|---|
| `tests/net.rs` | 667 | `let kind = DeviceKind::Builtin { ..., dsp_version: ... }; ... Device::new(ann.ids().device(), kind)` → `device: Device::builtin(ann.ids().device(), BuiltinKind::TwoOsc)`, and delete the `kind` variable |
| `tests/engine_scheduling.rs` | 189 | `Device::new(synth, DeviceKind::Builtin { ... })` → `Device::builtin(synth, BuiltinKind::TwoOsc)` |
| `tests/ui_tracktion.rs` | 211 | `instrument(&h).params.get(&two_osc::id::FILTER_FREQ).copied()` → `instrument(&h).param(two_osc::id::FILTER_FREQ)` |
| `tests/ui_tracktion.rs` | 230 | `instrument(&h).params.get(&two_osc::id::FILTER_FREQ), None,` → `instrument(&h).param(two_osc::id::FILTER_FREQ), None,` |
| `tests/ui_tracktion.rs` | 241 | `instrument(h).params.get(&two_osc::id::SUB_ON).copied()` → `instrument(h).param(two_osc::id::SUB_ON)` |
| `tests/ui.rs`, `tests/plugin_session.rs`, `tests/playback.rs` | | matches with `..` and `builtin_kind`; unchanged |

Remove imports the compiler reports unused; add `BuiltinKind` where `Device::builtin` needs it.

Check: `cargo check --workspace --all-targets`, then the full gates.

#### Step 10. Docs

- `docs/protocol.md`:
  - the version paragraph at the top: add "Version 12 puts a device's settings into its kind:
    a built-in device has parameter values, a plugin device has its state; `dsp_version` is
    gone.";
  - the type listing near line 296: `Device { id, kind, bypass }`,
    `DeviceKind::Builtin { kind: BuiltinKind, params: map of u32 to f64 }`, `Plugin` as it is;
  - where `DeviceSetParam` is described: it acts on built-in devices only;
  - the store section: format `9`.
- `docs/decisions/0003-stored-format.md`, under "Adding a migration": "The step from 8 to 9 in
  `mdaw-server/src/migrate.rs` is the worked example: it moves a field and removes two."

**Do not** touch `TrackShell` / `TrackContent`, the shapes of the ops, the generic sliders for a
running plugin (`Session::set_plugin_param`, which talks to the engine, not the document), or
the bridge.

**Done when** the gates pass and
`grep -rn "dsp_version\|DSP_VERSION\|put_params\|fn put_param\b" crates | grep -v migrate.rs`
prints nothing (`migrate.rs` names `dsp_version` on purpose).

---

## T7. Disk syncs off the UI thread

**Why.** `Session::frame` calls `Outbox::keep` every frame; that forces the pending file to the
disk every 250 ms while edits are unconfirmed, and rewrites the file (with two more syncs) up
to once a second. `Session::put_blob` syncs every saved plugin state inline. A sync is short on
a fast, idle disk and long on a slow or busy one, and the frame waits for it.

**Outcome.** The UI thread only writes (into the page cache). A thread owned by the outbox does
every `sync_data`. Confirmed edits are dropped by truncating the file when nothing is pending,
instead of rewriting it now and then.

What stays true: an edit is written before it is sent, so a crash of the client loses nothing;
a power cut may take the last moments (now about 50 ms instead of 250).

One commit.

**Files**

- `crates/mdaw-client/src/outbox.rs`
- `crates/mdaw-client/src/session.rs` (`next_frame_in` only)
- `docs/protocol.md` (the outbox paragraph near line 197)
- `docs/decisions/0004-session-thread.md` (new, short)
- `docs/manual-tests/foundation.md` (section B)

**Design**

```rust
/// What the sync thread is asked to do, in order.
enum Disk {
    /// Force this file's data to the disk (a handle to `pending`, or a blob just written).
    Sync(File),
    /// Answer when everything asked before this is done.
    Flush(Sender<()>),
}
```

- `Outbox` gains `disk: Sender<Disk>`, `thread: Option<JoinHandle<()>>` and
  `failed: Arc<Mutex<Option<String>>>`; it loses `unsynced`, `synced_at`, `compacted_at`.
- The thread, named `mdaw-outbox-sync`:

  ```text
  loop:
      wait for a job (the channel closing ends the loop)
      take every job queued right now as well
      for each, in order:
          Sync(file)   -> file.sync_data(); on error store the text in `failed`
          Flush(reply) -> reply.send(())
      if the batch held no Flush: sleep SYNC_GAP (50 ms)
  ```

  The sleep bounds the syncs to twenty a second during a long drag; while it sleeps, further
  appends pile up in the channel and are covered by the next round. In a batch with several
  `Sync` jobs it is fine to sync each; they are few.
- `SYNC_EVERY` and `COMPACT_EVERY` are deleted; `const SYNC_GAP: Duration = Duration::from_millis(50)`
  is private.

**Steps**

1. `Outbox::open`: after the repair (`rewrite`) at the end, start the thread and store the
   sender. The repair itself stays as it is, synchronous: it runs once at start.
2. `append`: write the record, and only after the write succeeded push the id and send
   `Disk::Sync(self.file.try_clone()?)`. Make the write safe against stopping halfway. `write_all` may have written part of a record when it
   returns an error; a later record appended behind that part would be unreadable, because
   reading stops at the first record that does not check out (`parse`). So:

   ```rust
   if self.broken {
       bail!("the file of unsent edits is damaged; edits are no longer saved on this computer");
   }
   let before = self.file.metadata().context("saving an edit")?.len();
   if let Err(err) = self.file.write_all(&record) {
       // Take the part of a record back, so the next one starts where a record may start.
       if self.file.set_len(before).is_err() {
           self.broken = true;
       }
       return Err(err).context("saving an edit");
   }
   ```

   `broken: bool` is a new field, false at `open`. To test this without a failing disk, move
   the block into a private function generic over the file,
   `fn append_record<F: Write + Rollback>(file: &mut F, record: &[u8], broken: &mut bool) -> Result<()>`,
   with a small private trait `Rollback { fn size(&self) -> io::Result<u64>; fn cut_to(&self, size: u64) -> io::Result<()>; }`
   implemented for `File`. `Session::send` already tells the user when `append` fails and
   sends the edit all the same; that stays. A send error to the sync thread (thread gone) is
   returned as an error.
3. `put_blob`: if the file exists, return. Otherwise write the bytes **directly to the final
   path** (`File::create(&path)`, `write_all`), then send `Disk::Sync(file)`. No `.part` file,
   no rename, no `sync_dir`. This is safe because `Outbox::open` already ignores a blob whose
   bytes do not hash to its name. Since `put_blob` returns early when the file exists, a torn
   file must not stay: in `Outbox::open`, in the loop that reads the blobs, remove a blob file
   whose bytes do not hash to its name (`let _ = std::fs::remove_file(...)`). It needs `&self` only, as now.
4. `keep(&mut self, pending: &[&Txn])`:
   - take an error out of `failed` and return it (`bail!("{text}")`);
   - if `pending` is empty and `self.ids` is not: `self.file.set_len(0)`, clear `self.ids`,
     and remove every file in the `blobs` folder (ignore errors, as `rewrite` does);
   - otherwise nothing. Delete `has_stale`.

   Why truncating without a sync is enough: what a power cut could bring back are edits the
   server has confirmed, and sent again they are answered from their receipts.
5. Delete `unsynced()` and `sync()`. Add:

   ```rust
   /// Waits until everything written so far is on the disk. For tests and for shutdown.
   pub fn flush(&self) {
       let (reply, done) = std::sync::mpsc::channel();
       if self.disk.send(Disk::Flush(reply)).is_ok() {
           let _ = done.recv();
       }
   }
   ```

6. `impl Drop for Outbox`: call `flush()`, then drop the sender (replace it or wrap it in an
   `Option` so it can be taken) and join the thread.
7. `rewrite` (now used only by `open`): remove the lines that set the deleted fields.
8. The module comment at the top of `outbox.rs`: rewrite the `pending` and `blobs` bullets and
   the paragraph "Confirmed transactions are dropped..." to say what is true now (written
   before sending; forced to the disk by a thread within about 50 ms; the file is emptied when
   nothing is pending; a blob is written in place and checked by its hash when read).
9. `session.rs`, `next_frame_in`: remove the `outbox` term and the comment above it; the result
   is `[plugins, others].into_iter().flatten().min()`.
10. Tests in `outbox.rs`:
    - replace calls to `sync()` with `flush()`;
    - `confirmed_transactions_and_their_blobs_are_dropped` becomes: append `a` (naming a blob)
      and `b`; `keep(&[&b])` leaves `len() == 2` and the blob in place; `keep(&[])` leaves it
      empty and the blob gone; after reopening, nothing is saved;
    - new `a_torn_blob_is_not_restored`: append an edit naming a blob, write garbage under the
      blob's file name by hand, drop and reopen: the edit is there, `saved.blobs` is empty;
    - new `the_file_is_on_disk_after_flush`: append, `flush()`, then read `pending` through a
      second `Outbox::open` on the same folder after dropping the first: the edit is there.
      (This cannot prove the sync; it proves `flush` returns and the order holds.)
    - adapt the ignored `append_cost` measurement so it compiles: it measures `append` alone
      and `append` followed by `flush`;
    - new `a_write_cut_short_is_taken_back`: a test type that wraps a real `File` in a
      temporary folder, implements `Write` and `Rollback`, and on its first `write` call writes
      half the bytes and then returns an error. Through `append_record`: the first record
      fails; a second, written with a plain `File` on the same path, succeeds; `parse` of the
      file's bytes gives exactly the second edit. A second test type whose `cut_to` also
      fails: `broken` is true after the failed append.
11. `docs/protocol.md`, the outbox paragraph: the same facts as step 8, in one or two
    sentences.
12. `docs/decisions/0004-session-thread.md`: status "deferred, 2026-11-16". Context: the
    session (network receive, replica, outbox, engine hand-off) runs inside the UI frame; the
    audit proposed moving it to its own thread with the UI sending commands and drawing
    snapshots. Decision: not now; the disk syncs, the one measurable cost, moved to a thread
    (this task). Take it up again when one of these shows: a frame over 16 ms traced to
    `Session::frame`; the hidden-window receive delay (about 250 ms) mattering to users; a
    second window or view needing the same session. Note the size: about 78 uses of
    `app.session` in `src/ui` and `src/app.rs`.
13. `docs/manual-tests/foundation.md`, section **B. Unsent edits**, with two clients and a
    server (`cargo xtask server`, `cargo xtask client Ann`, `cargo xtask client Ben`):
    - B.1 Ann drags a knob of the built-in instrument for ten seconds: no stutter, Ben sees it.
    - B.2 Stop the server. Ann adds three notes. Kill Ann's client (`kill -9`). Start the
      server, start Ann again: the three notes are there and reach Ben.
    - B.3 Ann loads a plugin and changes a preset, with the server stopped. Kill Ann's client,
      start the server and Ann again: the preset is there and reaches Ben.
    - B.4 After everything is confirmed, the file `pending` in Ann's project folder under her
      profile is empty (say how to find it: the xtask prints the profile folder).

**Do not** move anything else off the UI thread, change `Session::send`, `put_blob` in
`session.rs`, the link, or the outbox's record format.

**Done when** the gates pass (the reconnect and restart tests in
`crates/mdaw-client/tests/net.rs` are the ones that matter) and
`grep -n "SYNC_EVERY\|COMPACT_EVERY\|unsynced\|sync_data" crates/mdaw-client/src/session.rs`
prints nothing. Then **pause**: ask the user to run section B.

---

## T8. The arrangement's repaint: measure, then decide

**Why.** `paint_clip` in `crates/mdaw-client/src/ui/arrange.rs` walks a clip's notes three
times per frame (lowest pitch, highest pitch, drawing) and emits one rectangle shape per note,
also for notes outside the visible part of the clip. While playing, every frame repaints. This
was read from the code, not measured.

**Outcome.** A number. Only if the number is bad, a cheaper `paint_clip`.

### T8a. Measure (one commit)

**Files**: `crates/mdaw-client/tests/arrange_bench.rs` (new).

1. An ignored test, in the manner of `tests/editor_latency.rs` (a header comment with the
   command to run it, `#[ignore = "a measurement, run by hand"]`):
   - build a `Project` with 16 tracks, each with one clip from tick 0 of 64 bars holding 2000
     notes spread evenly over the clip and over pitches 36 to 84 (32 000 notes); use
     `mdaw_model::apply_all` with `TrackAdd` and `ClipAdd` ops, as `Session::starter` does;
   - a second project, the same without notes;
   - for each: `DawApp::new(Session::local(project, SilentAudio::manual()))` in a kittest
     harness built like `harness_on` in `tests/ui.rs` (1280 × 800); 20 warm-up steps; then 200
     times: time `harness.step()`, then time
     `harness.ctx.tessellate(harness.output().shapes.clone(), harness.output().pixels_per_point)`;
   - print, for each project, the median and the 95th percentile of both times in
     milliseconds, and the number of shapes of the last frame, as one `RESULT` line each.
2. Run it: `cargo test --release -p mdaw-client --test arrange_bench -- --ignored --nocapture`,
   three times. Report the three sets of numbers.

**Decision rule.** Let *cost* be the median of (step + tessellate) with notes minus the same
without notes.

- *cost* under 1.0 ms: stop. Commit the measurement with the numbers in the commit message.
  T8 is finished; do not do T8b.
- *cost* of 1.0 ms or more: do T8b.

### T8b. Cheaper clips (only if T8a says so; one commit)

**Files**: `crates/mdaw-client/src/ui/arrange.rs`, function `paint_clip` only.

1. Find the lowest and highest pitch in **one** pass (a fold over `clip.notes.values()`).
2. Build one `egui::epaint::Mesh` for all the clip's notes with `Mesh::add_colored_rect(bar,
   color)` and add it once with `notes.add(egui::Shape::mesh(mesh))`, instead of one
   `rect_filled` per note.
3. Skip a note whose bar lies wholly left or right of `notes.clip_rect()`.
4. Everything else in `paint_clip` (background, name, outline) stays as it is.
5. Run T8a's measurement again, three times, and put before and after in the commit message.
   If *cost* did not drop by at least half, revert and report instead of committing.

A cache of the mesh per clip (keyed by the clip's `Arc` pointer, the rectangle and the colour)
is the next step if T8b is not enough. It is **not** part of this plan: report the numbers and
stop.

**Do not** change the piano roll, the layout, the hit testing or any other paint function.

---

## T9. Live notes keep their source (optional)

**Why.** `LiveSink::note_on` and `note_off` take a `key` that says who plays the note (the
keyboard and MIDI input use `OWN_KEY | pitch`, the piano roll's audition uses `PREVIEW_KEY`).
`TeLive` in `crates/mdaw-client/src/audio_tracktion.rs` ignores it, and the bridge sends plain
MIDI on channel 1. If the same pitch on the same track is held by both, the first one released
ends the note for both.

**Outcome.** The engine gets a note-on when the first source starts a pitch on a track and a
note-off when the last source lets go.

One commit. Skip the task if the user says so; nothing depends on it.

**Files**: `crates/mdaw-client/src/audio_tracktion.rs` only.

**Steps**

1. `TeLive` becomes a struct with the engine and
   `held: Mutex<HashMap<(TrackId, u8), Vec<u64>>>` (the keys holding each pitch).
2. `note_on(track, key, pitch, vel)`: under the lock, look up `(track, pitch)`. If `key` is in
   the list already, do nothing. Otherwise add it; if the list was empty before, send the
   note-on to the engine. (A second source on a sounding pitch does not strike it again.)
3. `note_off(track, key, pitch)`: under the lock, remove `key` from the list; if it was there
   and the list is now empty, remove the entry and send the note-off. A `key` that is not in
   the list changes nothing.
4. Send to the engine while holding the lock, so two threads cannot reorder an on and an off
   of one pitch. `track_live_note` only posts to the engine's thread and returns; it does not
   wait.
5. `TracktionAudio::live()` now makes a new `TeLive` on every call, and `Session::preview_on`
   calls it each time. The list must be shared: make one `Arc<TeLive>` in
   `TracktionAudio::new`, keep it in a field, and return a clone from `live()`.
6. Unit tests need no engine if the sending is separated: give `TeLive` a private method
   `fn change(&self, track, key, pitch, on: bool) -> Option<bool>` that updates the list and
   returns what to send (`Some(true)` on, `Some(false)` off, `None` nothing); `note_on` and
   `note_off` call it and send. For the test, build the list alone (put it in a small struct
   `Held` with the `change` method, owned by `TeLive`). Cases: two sources on one pitch give
   one on, and one off only after the second release, in either release order; the same key
   twice gives one on; an off without an on gives nothing; different pitches and different
   tracks do not affect each other.

**Do not** change the `LiveSink` trait, `live.rs`, the piano roll, the bridge or its header.

**Done when** the gates pass.

---

## T10. A plugin that took the application down is not started again

**Why.** Plugins run inside the client (ADR 0002). One that crashes while it loads, or in its
first moments of processing, ends the client; the project then loads the same plugin again on
the next start, and the client ends again. ADR 0002's failure policy, item 2, answers this and
is designed but not built: note which plugins are being started, and after a start that never
got past them, leave them off and say so.

**Outcome.** A plugin that was being started when the client last died is switched off on this
computer. Its devices show as failed with that reason, the rest of the project plays, and "Try
again" in the device tab switches it on again.

This is built in the Rust client only. The bridge, its header and `mdaw-tracktion` do not
change.

**What "started" means here.** The bridge does not report when a plugin has processed its first
blocks, so the client uses time: a plugin counts as started `SETTLE` (5 s) after the call that
added it returned, with the client still alive. Two limits follow, and both go into the ADR:

- a plugin that crashes later than that, or hangs the audio and is killed by hand after more
  than 5 s, is not caught;
- a client that is killed for another reason within 5 s of starting plugins switches those
  plugins off wrongly. The notice says which, and "Try again" brings each back.

One commit. Do this after T2, T2b, T6 and T9: it edits `Worker::sync`, `Worker::retry` and
`Worker::publish_statuses` as they leave them. Line numbers below are those of `9e7525e`; find
the code by the text quoted.

**Files**

- `crates/mdaw-client/src/plugin_guard.rs` (new)
- `crates/mdaw-client/src/lib.rs` (`pub mod plugin_guard;`, in the alphabetical list)
- `crates/mdaw-client/src/audio_tracktion.rs`
- `crates/mdaw-client/tests/plugin_session.rs` (one new test)
- `docs/decisions/0002-plugin-hosting.md`
- `docs/manual-tests/foundation.md` (section C)

### Step 1. The guard: `crates/mdaw-client/src/plugin_guard.rs`

A module of file logic that knows nothing of the engine. Write it as given.

```rust
//! Remembers which plugins were being started when the client last ended, so that one that
//! takes the client down is not started again by itself (ADR 0002, failure policy, item 2).
//!
//! Two text files in the engine's settings folder, one plugin per line as
//! `<format name><TAB><plugin id>`:
//!
//! - `plugins-starting`: the plugins being started right now. Written (and forced to the
//!   disk) before a plugin is started, removed once the started plugins have run for a while.
//!   A client that ends in between leaves it behind.
//! - `plugins-off`: the plugins switched off because an earlier run left them in
//!   `plugins-starting`. They stay off until the user switches them on again.
//!
//! All of this is about this computer only; none of it is in the document.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use mdaw_model::PluginFormat;

/// A plugin as the document names it: its format and its own id in that format.
pub type PluginKey = (PluginFormat, String);

const STARTING: &str = "plugins-starting";
const OFF: &str = "plugins-off";

pub struct PluginGuard {
    dir: PathBuf,
    starting: BTreeSet<PluginKey>,
    off: BTreeSet<PluginKey>,
}

impl PluginGuard {
    /// Reads the two files in `dir` (created if missing). Plugins found in `plugins-starting`
    /// were being started when the client last ended: they are added to the switched-off
    /// ones. Returns the guard and the plugins switched off just now.
    pub fn open(dir: &Path) -> io::Result<(Self, Vec<PluginKey>)> {
        std::fs::create_dir_all(dir)?;
        let left = read(&dir.join(STARTING))?;
        let mut off = read(&dir.join(OFF))?;
        let newly: Vec<PluginKey> = left.into_iter().filter(|key| off.insert(key.clone())).collect();
        if !newly.is_empty() {
            write(dir, OFF, &off)?;
        }
        // After `plugins-off` is on the disk: a crash between the two loses nothing.
        write(dir, STARTING, &BTreeSet::new())?;
        let guard = Self {
            dir: dir.to_path_buf(),
            starting: BTreeSet::new(),
            off,
        };
        Ok((guard, newly))
    }

    pub fn is_off(&self, format: PluginFormat, id: &str) -> bool {
        self.off.iter().any(|(f, i)| *f == format && i == id)
    }

    /// Call before starting these plugins: when this returns, they are in `plugins-starting`
    /// on the disk. Nothing is written when all of them are in it already. A plugin whose id
    /// holds a tab or a line break cannot be written and is left out.
    pub fn starting(&mut self, plugins: impl IntoIterator<Item = PluginKey>) -> io::Result<()> {
        let mut added = false;
        for key in plugins {
            if !key.1.contains(['\t', '\n', '\r']) {
                added |= self.starting.insert(key);
            }
        }
        if added {
            write(&self.dir, STARTING, &self.starting)?;
        }
        Ok(())
    }

    /// The plugins started have run for a while: removes `plugins-starting`.
    pub fn settled(&mut self) -> io::Result<()> {
        if self.starting.is_empty() {
            return Ok(());
        }
        self.starting.clear();
        write(&self.dir, STARTING, &self.starting)
    }

    /// Switches a plugin on again.
    pub fn switch_on(&mut self, format: PluginFormat, id: &str) -> io::Result<()> {
        let before = self.off.len();
        self.off.retain(|(f, i)| !(*f == format && i == id));
        if self.off.len() != before {
            write(&self.dir, OFF, &self.off)?;
        }
        Ok(())
    }

    /// The client ends in the normal way: plugins still in their first moments are not to
    /// blame for that. Errors are ignored; the worst is a plugin switched off wrongly.
    pub fn ended_normally(dir: &Path) {
        let _ = std::fs::remove_file(dir.join(STARTING));
    }
}

/// The plugins a file lists. A missing file lists none; lines that do not parse are skipped.
fn read(path: &Path) -> io::Result<BTreeSet<PluginKey>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(err) => return Err(err),
    };
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let (format, id) = line.split_once('\t')?;
            let format = PluginFormat::from_name(format)?;
            (!id.is_empty()).then(|| (format, id.to_string()))
        })
        .collect())
}

/// Writes `set` as the file `name` in `dir`, whole: to the side, forced to the disk, then
/// renamed over the old one. An empty set removes the file.
fn write(dir: &Path, name: &str, set: &BTreeSet<PluginKey>) -> io::Result<()> {
    let path = dir.join(name);
    if set.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    let mut text = String::new();
    for (format, id) in set {
        text.push_str(format.name());
        text.push('\t');
        text.push_str(id);
        text.push('\n');
    }
    let part = dir.join(format!("{name}.part"));
    let mut file = File::create(&part)?;
    file.write_all(text.as_bytes())?;
    file.sync_data()?;
    std::fs::rename(&part, &path)?;
    File::open(dir)?.sync_all()
}
```

`PluginFormat` already derives `Ord`, and `PluginFormat::from_name` / `name` exist
(`crates/mdaw-model/src/doc.rs`).

Unit tests at the bottom of the module (`#[cfg(test)] mod tests`), each on
`tempfile::tempdir()` (a dev-dependency of the crate already). With
`let synth = || (PluginFormat::Clap, "org.example.synth".to_string());` and a second key
`(PluginFormat::Vst3, "ABCDEF".to_string())`:

| Test | Steps | Assert |
|---|---|---|
| `nothing_there` | `open` | no plugins switched off; `is_off` false; neither file exists |
| `a_start_that_never_settled_switches_off` | `open`; `starting([synth])`; drop the guard; `open` again | the second `open` returns `[synth]`; `is_off` true; `plugins-starting` gone; `plugins-off` exists; a third `open` returns nothing new and `is_off` is still true |
| `a_settled_start_leaves_nothing` | `open`; `starting([synth])`; `settled()`; drop; `open` | nothing switched off; `plugins-starting` gone |
| `switching_on_again` | make both keys off (as in the second test, with both); `switch_on` the synth; drop; `open` | the synth is on, the other is off, nothing newly switched off |
| `the_last_one_switched_on_removes_the_file` | one key off; `switch_on` it | `plugins-off` does not exist |
| `lines_that_do_not_parse_are_skipped` | write `plugins-off` by hand: a good line, `garbage`, `NOPE\tx`, a good line; `open` | exactly the two good ones are off |
| `ended_normally_forgets_the_start` | `open`; `starting([synth])`; `PluginGuard::ended_normally(dir)`; drop; `open` | nothing switched off |

Check: `cargo test -p mdaw-client --lib plugin_guard`.

### Step 2. New state in `audio_tracktion.rs`

Imports: add `use crate::plugin_guard::{PluginGuard, PluginKey};`.

Constants, next to the others at the top:

```rust
/// A plugin started this long ago, with the client still running, is past its first moments.
const SETTLE: Duration = Duration::from_secs(5);
/// What a device of a switched-off plugin says.
const OFF_REASON: &str = "switched off, because mdaw ended while starting it; Try again switches it on";
```

`struct Shared`, one new field:

```rust
    /// When the plugins started last are past their first moments (see `Worker::guard`); the
    /// window's thread then asks the worker to forget that they were starting.
    settle_at: Option<Instant>,
```

`enum Job`, one new variant:

```rust
    /// The plugins started are past their first moments.
    Settled,
```

`enum Coalesce`: add the variant `Settled`; in `Coalesce::device`, add it to the arm that
returns `None` (`Coalesce::OutputGain | Coalesce::Catalog(_) | Coalesce::Settled => None`).

`fn spec`: one new arm:

```rust
        Job::Settled => Spec::ordered(Normal, Key::Global, "settled").unique(Coalesce::Settled),
```

`struct TracktionAudio`, one new field: `settings_dir: PathBuf,` (set from
`options.settings_dir.clone()` in `new`).

`struct Worker`, one new field:

```rust
    /// Which plugins are switched off here, and which are being started; `None` when plugins
    /// are off or the guard's files cannot be used.
    guard: Option<PluginGuard>,
```

### Step 3. Start-up: `TracktionAudio::new`

After `let shared = Arc::new(Mutex::new(Shared::default()));` and before the `Worker` is built:

```rust
        let guard = if options.enable_plugins {
            match PluginGuard::open(&options.settings_dir) {
                Ok((guard, newly_off)) => {
                    if !newly_off.is_empty() {
                        let names: Vec<String> = newly_off
                            .iter()
                            .map(|(format, id)| format!("{} {id}", format.name()))
                            .collect();
                        lock(&shared).notices.push(format!(
                            "mdaw ended while starting these plugins the last time: {}. They are \
                             switched off on this computer, and tracks that use them are silent \
                             here. \"Try again\" in the device tab switches one on.",
                            names.join(", ")
                        ));
                    }
                    Some(guard)
                }
                Err(err) => {
                    lock(&shared).notices.push(format!(
                        "Plugins that fail cannot be remembered on this computer: {err}."
                    ));
                    None
                }
            }
        } else {
            None
        };
```

Add `guard,` to the `Worker { ... }` literal and `settings_dir: options.settings_dir.clone(),`
to the `Self { ... }` literal at the end. (`Engine::new` runs before this and creates the
settings folder; `PluginGuard::open` creates it too if needed.)

### Step 4. A view without the switched-off plugins

A free function next to `set_state` at the bottom of the file:

```rust
/// `view` without the plugin devices whose plugin is switched off, so that the engine is
/// never asked to start them. Tracks without such a device are not touched and stay shared.
fn without_off(mut view: Project, guard: Option<&PluginGuard>) -> Project {
    let Some(guard) = guard else { return view };
    let is_off = |d: &mdaw_model::Device| {
        matches!(&d.kind, DeviceKind::Plugin { format, id, .. } if guard.is_off(*format, id))
    };
    let tracks: Vec<TrackId> = view
        .tracks
        .iter()
        .filter(|(_, t)| t.content.devices.iter().any(is_off))
        .map(|(id, _)| *id)
        .collect();
    for id in tracks {
        if let Some(track) = view.tracks.get_mut(&id) {
            let content: &mut TrackContent = Arc::make_mut(&mut Arc::make_mut(track).content);
            content.devices.retain(|d| !is_off(d));
        }
    }
    view
}
```

In `Worker::sync`, the line `let Some(view) = view else { return };` becomes:

```rust
        let Some(view) = view else { return };
        let view = without_off(view, self.guard.as_ref());
```

From here on `view`, and `self.synced` after it, never hold a device of a switched-off plugin.
Everything else in the worker that walks `self.synced` (`capture`, `look`, `blob`, `retry`)
therefore leaves those devices alone without further changes.

### Step 5. Marking a start, and when it settles

Two methods on `Worker`:

```rust
    /// Notes the plugins of `project` that the engine does not have, and so is about to
    /// start, on the disk. True if there are any: the caller says when they have started.
    fn mark_starting(&mut self, project: &Project) -> bool {
        let Some(guard) = &mut self.guard else {
            return false;
        };
        let engine = &self.engine;
        let starting: Vec<PluginKey> = plugin_devices(project)
            .filter(|(_, d)| !engine.has(EntityKind::Device, raw_device(d.id)))
            .filter_map(|(_, d)| match &d.kind {
                DeviceKind::Plugin { format, id, .. } => Some((*format, id.clone())),
                DeviceKind::Builtin { .. } => None,
            })
            .collect();
        if starting.is_empty() {
            return false;
        }
        if let Err(err) = guard.starting(starting) {
            lock(&self.shared).notices.push(format!(
                "Plugins that fail cannot be remembered on this computer: {err}."
            ));
        }
        true
    }

    /// The plugins marked were started, or could not be: from now they have `SETTLE` to show
    /// that they do not take the client down.
    fn started(&self) {
        lock(&self.shared).settle_at = Some(Instant::now() + SETTLE);
    }
```

In `Worker::sync`: immediately before the line `let engine = self.engine.clone();` that
precedes `let result = { ... sync_project / load_project ... }`, add

```rust
        let marked = self.mark_starting(&view);
```

and immediately after the whole `let problems = match result { ... };` statement (after its
closing `};`, so on every path that did not `return`), add

```rust
        if marked {
            self.started();
        }
```

T2b added a `return` inside that `match` for a load that failed. Put the same two lines
(`if marked { self.started(); }`) before that `return` as well.

In `Worker::retry`: change `fn retry(&self, ...)` to `fn retry(&mut self, ...)`. It reads the
project through `let Some(project) = &self.synced`; change that to
`let Some(project) = self.synced.clone() else { return Vec::new() };` (a clone is cheap and
ends the borrow), fix the uses of `project` that followed to take `&project`, and around the
call that adds the device (after T2: `mdaw_tracktion::add_device(...)`) put:

```rust
        let marked = self.mark_starting(&project);
        let result = /* the add_device call, as it is */;
        if marked {
            self.started();
        }
```

then `match result { ... }` as before. `mark_starting` on the whole project marks every plugin
the engine lacks, not only this device; that is fine, they are the ones `catalog_changed` is
about to retry one by one.

`catalog_changed` calls `self.retry` in a loop over a `Vec` it collected first, so it compiles
with `&mut self` as it is (it already takes `&mut self`).

### Step 6. Settling

In `Worker::job`, a new arm:

```rust
            Job::Settled => {
                let due = {
                    let mut shared = lock(&self.shared);
                    match shared.settle_at {
                        Some(at) if at <= Instant::now() => {
                            shared.settle_at = None;
                            true
                        }
                        // Not set, or a newer start moved it: the window asks again then.
                        _ => false,
                    }
                };
                if due
                    && let Some(guard) = &mut self.guard
                    && let Err(err) = guard.settled()
                {
                    self.notice(format!(
                        "Plugins that fail cannot be remembered on this computer: {err}."
                    ));
                }
            }
```

(If the borrow checker objects to `self.notice` while `guard` is borrowed, take the result
first: `let result = self.guard.as_mut().map(|g| g.settled());` and report after.)

On the window's thread, in `impl Audio for TracktionAudio`:

- `frame`, right after `let state = self.read();` and the `seek` block:

  ```rust
        // Plugins started a while ago are past their first moments: the worker forgets them.
        let settle = lock(&self.shared).settle_at;
        if settle.is_some_and(|at| at <= Instant::now()) {
            self.send(Job::Settled);
        }
  ```

- `next_frame_in`: the function's present body computes one `Option<Duration>` with an
  `if / else if / else`. Bind that to a variable and combine it with the time left until
  settling, so that a frame comes to send the job even when nothing else happens:

  ```rust
    fn next_frame_in(&self) -> Option<Duration> {
        let (busy, settle) = {
            let shared = lock(&self.shared);
            (
                shared.scanning || shared.sync_queued || !shared.captured.is_empty(),
                shared.settle_at,
            )
        };
        let work = if !self.pending.is_empty() || busy {
            Some(Duration::from_millis(100))
        } else if !self.editors.is_empty() || !self.watched.is_empty() {
            // Changes made in a plugin's window come as events, or are found by looking.
            Some(Duration::from_millis(50))
        } else {
            None
        };
        // Not sooner than 100 ms, also once it has passed: the worker may be busy for a while
        // before it takes the job, and the window must not spin until then.
        let settle = settle
            .map(|at| at.saturating_duration_since(Instant::now()).max(Duration::from_millis(100)));
        [work, settle].into_iter().flatten().min()
    }
  ```

### Step 7. What the devices say

In `Worker::publish_statuses`, after the `for (_, device) in plugin_devices(project) { ... }`
loop that fills `statuses` and before `shared.statuses = statuses;`:

```rust
        // Devices of switched-off plugins are not in the synced project (`without_off`); the
        // view the session gave still has them.
        if let (Some(guard), Some(view)) = (&self.guard, &shared.view) {
            for (_, device) in plugin_devices(view) {
                if let DeviceKind::Plugin { format, id, .. } = &device.kind
                    && guard.is_off(*format, id)
                {
                    statuses.insert(device.id, DeviceStatus::Failed(OFF_REASON.into()));
                }
            }
        }
```

`shared` is the lock taken earlier in that function; it is still held there.

The window's side needs no change: `status_of` returns what `shared.statuses` holds,
`track_sound` turns `Failed` into "plugin failed", and the device tab shows the reason with a
"Try again" button for any `Failed` device (`crates/mdaw-client/src/ui/devices.rs`, near line
121).

### Step 8. Switching on again

"Try again" calls `TracktionAudio::restart_plugin`, which sends `Job::Retry { track, device }`.
Add a method on `Worker`:

```rust
    /// If `device`'s plugin is switched off, switches it on again. True if it was.
    fn switch_on(&mut self, device: DeviceId) -> bool {
        let kind = {
            let shared = lock(&self.shared);
            shared.view.as_ref().and_then(|view| {
                plugin_devices(view)
                    .find(|(_, d)| d.id == device)
                    .map(|(_, d)| d.kind.clone())
            })
        };
        let (Some(guard), Some(DeviceKind::Plugin { format, id, .. })) = (&mut self.guard, kind) else {
            return false;
        };
        if !guard.is_off(format, &id) {
            return false;
        }
        if let Err(err) = guard.switch_on(format, &id) {
            lock(&self.shared).notices.push(format!(
                "Plugins that fail cannot be remembered on this computer: {err}."
            ));
        }
        true
    }
```

and the `Job::Retry` arm in `Worker::job` becomes:

```rust
            Job::Retry { track, device } => {
                if self.switch_on(device) {
                    // The device is not in the synced project: a sync from the view adds it,
                    // marking its start like any other.
                    self.sync();
                } else {
                    self.retry(track, device);
                    self.publish_statuses(&[]);
                }
            }
```

### Step 9. A normal end

In `impl Drop for TracktionAudio`, as the last statement, after the worker has been joined:

```rust
        // Ending in the normal way is not a crash: plugins still in their first moments are
        // not to blame.
        PluginGuard::ended_normally(&self.settings_dir);
```

Check: `cargo clippy -p mdaw-client --all-targets -- -D warnings`.

### Step 10. The session test: `crates/mdaw-client/tests/plugin_session.rs`

One new test, `a_plugin_that_was_starting_when_mdaw_ended_is_switched_off`. Model it on the
first test of that file, which loads the test synth in a local session: copy how it takes
`support::tracktion()`, calls `clap()`, builds `TracktionOptions`, makes the session, waits for
the scan, finds the catalog entry of the test synth and adds it to a track, and how it uses
`until`, `plugin_device` and `listen`. Differences:

1. A settings folder of its own, `scratch().join("engine-guard")`. Before the engine is made:
   remove the folder (`let _ = std::fs::remove_dir_all(&dir);`), create it, and write the file
   `plugins-starting` in it with the content
   `format!("CLAP\t{}\n", mdaw_test_plugin::ID)`.
2. Make the engine on that folder and the session; wait for the scan to end; add the test synth
   to the first track.
3. `until(&mut session, "the plugin is switched off", |s| ...)`: the device's status is
   `DeviceStatus::Failed(reason)` with `reason.contains("switched off")`. Then assert:
   - `session.notices().iter().any(|n| n.contains(mdaw_test_plugin::ID))`;
   - `dir.join("plugins-off")` exists and `dir.join("plugins-starting")` does not;
   - `session.track_sound(track) == TrackSound::Failed`.
4. `session.restart_plugin(track, device)`, then
   `until(&mut session, "the plugin runs", |s| s.plugin_running(track, device))`. Assert that
   `dir.join("plugins-off")` no longer exists. Put one note on the track's clip as the
   neighbouring tests do and assert with `listen` that the track sounds.
5. Keep calling `session.frame()` for six seconds (a loop with a 20 ms sleep and a deadline);
   then assert that `dir.join("plugins-starting")` does not exist.
6. Drop the session before the `support::tracktion()` guard, as the other tests do.

If step 3's notice assertion fails because the notice strip holds only the last four notices,
check the notice right after the first `session.frame()` instead; do not raise the limit.

Check: `cargo test -p mdaw-client --test plugin_session`, then the full gates.

### Step 11. Docs

- `docs/decisions/0002-plugin-hosting.md`:
  - in the paragraph that starts "Item 1 is built and tested": items 2 and 3 are no longer
    "designed, not built". Item 3 was built with the bridge's stall event
    (`MDAW_EVENT_AUDIO_STALLED`, shown as a notice naming the loaded plugins). Item 2 is built
    in the client, `mdaw-client/src/plugin_guard.rs`.
  - add at the end, before "Consequences", a section "Addendum 2026-11-16: the crash marker
    (failure policy, item 2)" with: the two files and where they are; that a start is marked
    before the engine is asked to add a plugin and forgotten 5 s after that call returned;
    that the marker is by time, not by counted blocks, because the bridge does not report
    blocks per plugin; the two limits under "What 'started' means here" above; that the list
    is per computer and not in the document; that "Try again" switches a plugin on.
- `docs/manual-tests/foundation.md`, section **C. A plugin that takes mdaw down**. No plugin
  at hand crashes on demand after loading (the bridge's `bad-crash.clap` crashes while it is
  examined, so the scan already refuses it), so the test kills the client by hand:
  - C.1 Start the client, put any plugin on a track, and within 5 seconds kill the client
    (`kill -9`, or `pkill -9 mdaw`). Start it again: a notice names the plugin, the track says
    "plugin failed", the device tab gives the reason, the other tracks play.
  - C.2 In the device tab, "Try again": the plugin loads and sounds. Restart the client the
    normal way: it is still on.
  - C.3 Put a plugin on a track and close the window the normal way right away. Start again:
    nothing is switched off.
  - C.4 Put a plugin on a track, wait ten seconds, kill the client, start it: nothing is
    switched off.
  - C.5 With two clients on a server: Ann's plugin is switched off as in C.1. Ben, who has the
    plugin, still hears the track; nothing about it reached the document.

**Do not** change the bridge or its header, `mdaw-tracktion`, the plugin scan, the catalog (a
switched-off plugin is still offered in the menu), the model, the protocol or any UI file. Do
not keep the switched-off list in the document.

**Done when** the gates pass. Then **pause**: ask the user to run section C.

---

---

## After the last task

1. Update `docs/architecture.md`: the status line at the top, the table row for `mdaw-server`
   (store format 9, JSON) and `mdaw-proto` (protocol 12), and add to "Collaboration target" one
   sentence pointing to ADR 0003 for how the stored format changes.
2. Report, per task: the commit, what was measured, what was left out and why, and any test
   whose expectation changed.
3. The user's hand tests (`docs/manual-tests/foundation.md`, sections A, B and C) decide
   whether the slice is done.
