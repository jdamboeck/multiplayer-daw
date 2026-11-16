# Fixes from the review of the foundation work

A review of `9e7525e..56c6b8b` (the work of `docs/foundation-plan.md`) found ten possible
defects and three test problems. Each was checked again on `56c6b8b`, by reading the code and,
where it could be done, by driving real `mdaw-server` and `mdaw-slice` processes. This plan fixes
the ones that held up. The ones that did not, or are left for later, are listed at the end with
the reason.

Function names and file paths are those of `56c6b8b`. Find code by the names given, not by line
numbers.

## Rules for whoever carries this out

1. **One task at a time, in order.** T2 uses the helper T1 adds.
2. **Stay inside the task.** Each task has a "Files" list and a "Do not" list. If the work seems
   to need a file that is not listed, stop and report instead of widening the task.
3. **Gates after every task**, all green before committing:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo build -p mdaw-test-plugin
   env -u DISPLAY -u WAYLAND_DISPLAY cargo test --workspace
   ```

   Never run window tests on the real display. `editor_gui` is ignored by default; leave it so.
4. **Tests first where a task says so.** Write the test, run it on the unfixed code and keep its
   failing output for the report. A test that passes before the fix does not prove the fix:
   report that instead of going on.
5. **A failing test is information.** If a test fails that the task does not say will change,
   stop and report it with its output. Do not edit the assertion, add a special case or mark it
   ignored.
6. **Commit on the current branch, never push.** One commit per task. Commit messages are one
   plain sentence saying what changed, like the existing ones (`git log --oneline -10`).
7. **Docs are part of the task.** Where a task lists doc edits, they go in the same commit.

---

## T1. The crash test waits for the guard, not for a clock

**Why.** `edits_survive_a_crash_and_arrive_once` in `crates/mdaw-client/tests/slice.rs` sleeps
until 6 s after a point it guesses the synth started, so that `ann.crash()` falls after the
plugin guard's 5 s settle time. Measured on an idle machine, five restarts in a row: the
guard's file `plugins-starting` was gone 5.02 s after `Client::start` returned, every time. That
leaves about 1 s of margin; under the load of the full test run a slower start makes the crash
land inside the window, the synth is switched off for Ann, and the test fails for a reason that
has nothing to do with what it tests.

**Outcome.** The test waits until the guard has let go, as seen on the disk.

**Files**

- `crates/mdaw-client/tests/slice.rs`

**Steps**

1. Add a helper next to the other free functions of the file:

   ```rust
   /// Waits until the plugin guard of the client with this profile has let go of the plugins
   /// it started (`plugins-starting` gone or empty), so that killing the client now does not
   /// switch them off. The client keeps running between commands, so its timer runs.
   fn wait_settled(profile: &Path) {
       let file = profile.join("engine").join("plugins-starting");
       let deadline = Instant::now() + Duration::from_secs(30);
       while std::fs::metadata(&file).is_ok_and(|m| m.len() > 0) {
           assert!(Instant::now() < deadline, "the plugin guard never settled: {}", file.display());
           std::thread::sleep(Duration::from_millis(20));
       }
   }
   ```

2. In `edits_survive_a_crash_and_arrive_once`, replace each of the two
   `std::thread::sleep(Duration::from_millis(6000).saturating_sub(ann_started.elapsed()))` with
   `wait_settled(&ann_profile)`, keeping each where it is (the second stays before the
   `add-notes` edit, so the crash still comes right after an edit). Remove the two
   `let ann_started = ...` lines and update the two comments to say that the test waits for the
   guard.

**Do not** change anything else in the file.

**Done when** the gates pass and `grep -n "6000" crates/mdaw-client/tests/slice.rs` prints
nothing.

---

## T2. The guard marks only the plugins a call really starts

**Why.** `Worker::mark_starting` (`crates/mdaw-client/src/audio_tracktion.rs`) marks every plugin
device of the project that the engine does not have. That includes devices that are never going
to be started: a plugin that is not installed here, or one that was skipped. `sync_project` adds
only devices that are new in the diff, so those stay missing, and every later sync marks them
again. `Worker::retry`, called by `catalog_changed` once per missing device, marks all of them
too.

Reproduced with real processes: Ann adds the CLAP test synth; Ben, who does not have it, joins
and makes one edit a second for twelve seconds. Ben's `plugins-starting` lists the synth the
whole time; it goes only after Ben is quiet for 5 s, and comes back with his next edit. Ben then
crashes right after an edit and is started again with the synth installed: the notice says "mdaw
ended while starting these plugins the last time", and the synth is switched off on his
computer although it never ran there.

So while a project holds a plugin the client lacks (common when a collaborator has plugins you
do not), the settle window never closes during editing, any plugin really started meanwhile
stays listed with it, and an unrelated crash switches them all off. It also makes the ADR's
statement "removed 5 s after that call returned" false.

A second, rarer gap: when `sync_project` fails, `Worker::sync` falls back to `load_project`,
which clears the engine and starts every plugin again. `marked` was computed before the sync,
while the engine still held those plugins, so the reload is not marked, and a plugin that
crashes during it is not caught.

**Outcome.** A call marks exactly the installed plugins it is about to add, and the fallback
reload marks all of them.

**Files**

- `crates/mdaw-client/src/audio_tracktion.rs` (`Worker::sync`, `Worker::mark_starting`,
  `Worker::retry`, and a new free function with its unit tests)
- `crates/mdaw-client/tests/slice.rs` (one new test)
- `docs/decisions/0002-plugin-hosting.md` (the crash-marker addendum: one sentence)

**Steps**

1. **Test first.** In `slice.rs`, add
   `a_plugin_this_client_lacks_does_not_hold_the_guard_open`, with the file's helpers:
   - a server; Ann with `clap_dir()`, Ben with `empty_clap_dir()` and his own profile;
     `build_song(&mut ann)`; `ben.wait_seq(seq)`;
   - Ben makes six edits about a second apart (for example `add-notes Keys k <beat>:<pitch>`
     with different pitches, then `sleep 1000`). After each, assert that Ben's
     `engine/plugins-starting` is absent or empty;
   - Ben makes one more edit and crashes at once (`ben.crash()`);
   - Ben is started again on the same profile, this time with `clap_dir()`. Assert that his
     notices (`notices`) do not contain "switched off" or "ended while starting", and that
     `device-status Synth` reports `Running`.

   Run `env -u DISPLAY -u WAYLAND_DISPLAY cargo test -p mdaw-client --test slice
   a_plugin_this_client_lacks` and keep the failing output.

2. Add a free function in `audio_tracktion.rs`, near `plugin_devices`:

   ```rust
   /// The plugins a sync from `before` to `view` is about to start: the plugin devices of
   /// `view` whose plugin is installed and that `before` does not hold as they are (no device
   /// under that id, or one of another kind or plugin). All installed ones when there is no
   /// `before` (a whole load).
   fn starting_plugins(
       view: &Project,
       before: Option<&Project>,
       installed: impl Fn(PluginFormat, &str) -> bool,
   ) -> Vec<PluginKey>
   ```

   "Of another kind or plugin" mirrors what `mdaw_tracktion`'s `sync.rs` treats as a new device
   (its private `same_kind`): for two plugin devices, a different `format` or `id`. Read that
   function and match it.

3. `mark_starting` takes `before: Option<&Project>` and uses `starting_plugins`, with
   `installed` looking the plugin up in `shared.catalog` (as `publish_statuses` does). Avoid
   holding the `shared` lock while writing the guard's file: collect the catalog keys first.

4. `Worker::sync`: call `mark_starting(&view, self.synced.as_ref())` (borrow as needed). In the
   `Err` branch, before the fallback `load_project`, mark again with `before = None` and remember
   that something was marked, so that `started()` runs after the fallback whether it worked or
   not.

5. `Worker::retry`: mark only the device being retried, and only if its plugin is installed.
   `catalog_changed` stays as it is.

6. Unit tests for `starting_plugins` in the module's tests (add a `#[cfg(test)] mod tests` if the
   file has none): a device whose plugin is not installed is not listed; a new installed device
   is; an unchanged device is not; with no `before` every installed one is; a device under the
   same id whose plugin changed is.

7. ADR 0002, crash-marker addendum: add one sentence saying that only the plugins a call
   actually adds are noted, not devices whose plugin is not installed or that were skipped
   before.

**Do not** change `plugin_guard.rs`, `catalog_changed`, `publish_statuses` or the bridge.

**Done when** the gates pass, the new slice test passes, and it failed before step 2.

---

## T3. A normal exit with plugins off keeps the crash marker

**Why.** `TracktionAudio::new` opens the guard (and so moves a leftover `plugins-starting` to
`plugins-off`) only when plugins are enabled, but `Drop for TracktionAudio` calls
`PluginGuard::ended_normally` every time, which deletes `plugins-starting`.

Reproduced with real processes: Ann adds the synth and crashes at once; `plugins-starting` lists
the synth. Ann starts with `--no-plugins` (the natural way back in after a crash) and quits
normally: `plugins-starting` is gone and `plugins-off` was never written. Her next normal start
loads the synth again with no notice. With a plugin that really crashes, that is the crash loop
T10 of the foundation plan exists to stop.

**Outcome.** The marker is cleared on exit only by a client that opened the guard.

**Files**

- `crates/mdaw-client/src/audio_tracktion.rs` (`TracktionAudio`, `TracktionAudio::new`,
  `Drop for TracktionAudio`)
- `crates/mdaw-client/tests/plugin_session.rs` (one new test)

**Steps**

1. **Test first.** In `plugin_session.rs`, add
   `a_start_without_plugins_keeps_the_crash_marker`, modelled on
   `a_plugin_that_was_starting_when_mdaw_ended_is_switched_off`: write `plugins-starting` naming
   the test synth into a fresh settings folder; create `TracktionAudio` with
   `enable_plugins = false` and drop it; assert that `plugins-starting` still names the synth.
   Then create `TracktionAudio` on the same folder with plugins enabled and assert that a notice
   names the synth and `plugins-off` exists. Keep the failing output.

2. Add a field `guarded: bool` to `TracktionAudio`: true only when `PluginGuard::open`
   succeeded. If opening failed, leaving the file is the safe choice.

3. In `Drop`, call `PluginGuard::ended_normally` only when `guarded`.

**Do not** change `plugin_guard.rs`.

**Done when** the gates pass and the new test passes, having failed before step 2.

---

## T4. Parameter values come back from the store bit for bit

**Why.** The server stores events and snapshots as JSON (ADR 0003). `serde_json` writes an `f64`
in its shortest exact form, but without its `float_roundtrip` feature it reads some decimals
back one bit off. Reproduced through a real server: Ann set the built-in instrument's AMP
sustain (param 27) to `0.9856906946328695` on one track and 19 random values on others. The row
in `project.db` holds `"value":0.9856906946328695` exactly. After a server restart a newly
joined client's document hash differed from Ann's (`9f3ac6b9…` against `c5c1d56d…`), while Ann,
who saw the edits live, kept hers. Clients then disagree on the document for good. The
difference cannot be heard, but anything that compares documents breaks.

Because the stored text is already exact, no migration is needed: reading with the feature on
gives the right values from existing stores.

**Outcome.** Every `f64` stored comes back as it went in.

**Files**

- `Cargo.toml` (the workspace's `serde_json` entry)
- `Cargo.lock` (only if cargo changes it)
- `crates/mdaw-server/src/store.rs` (one new unit test)
- `docs/decisions/0003-stored-format.md` (one sentence)

**Steps**

1. **Test first.** In `store.rs`'s tests, add `parameter_values_come_back_exactly`: append
   events whose ops are `DeviceSetParam` with `value: Some(v)` for `0.9856906946328695` and for
   1000 further values from a fixed-seed generator written in the test (no new dependency; for
   example an xorshift over `u64`, turned into an `f64` in `0..1`). Read them back with
   `events_after` and assert each value is bit-identical (`to_bits`). Keep the failing output.

2. In the workspace `Cargo.toml`, change `serde_json = "1"` to
   `serde_json = { version = "1", features = ["float_roundtrip"] }`.

3. ADR 0003: add one sentence to the decision: numbers are written in their shortest exact form
   and read with `serde_json`'s `float_roundtrip`, so they come back bit for bit; stores written
   before this change need nothing.

**Do not** change the store format number or add a migration step.

**Done when** the gates pass and the new test passes, having failed before step 2.

---

## T5. A migration backup is whole or absent

**Why.** `Store::back_up_before_migrating` (`crates/mdaw-server/src/store.rs`) skips the backup
if `<path>.v<n>` exists, and `VACUUM INTO` writes that file in place. A crash or a full disk
during the copy leaves a partial backup, which every later start then trusts, and the store is
migrated with no usable copy of the old one. Nothing tests the backup or a migration through
`Store::open`.

**Outcome.** The backup is written under a temporary name and renamed when whole.

**Files**

- `crates/mdaw-server/src/store.rs`
- `crates/mdaw-server/src/migrate.rs` (only to share its version-8 test rows with `store.rs`'s
  tests, if needed)

**Steps**

1. `back_up_before_migrating`: remove a leftover `<path>.v<n>.part` if there is one, `VACUUM
   INTO` the `.part` name, then `std::fs::rename` it to `<path>.v<n>`, then sync the directory
   (`File::open(dir)?.sync_all()`), as the plugin guard's `write` does. Keep the early return
   when `<path>.v<n>` exists.

2. Tests in `store.rs`, using a temporary directory and the version-8 rows of `migrate.rs`'s
   tests (`PROJECT_8`, `EVENT_8`; make them reachable from `store.rs`'s tests if they are not):
   - `an_old_store_is_backed_up_and_migrated`: a store at format 8 with those rows, opened with
     `Store::open`, reads as format 9 and its event; `<path>.v8` exists and its own `meta` row
     says format 8;
   - `a_partial_backup_is_replaced`: with a junk `<path>.v8.part` beside the store, the same
     open succeeds and leaves no `.part` file.

**Do not** change `migrate.rs`'s logic or `STORE_VERSION`.

**Done when** the gates pass.

---

## T6. Small outbox repairs

**Why.** Three things in `crates/mdaw-client/src/outbox.rs`:

- `Outbox::put_blob` returns early if the blob's file exists. A file cut short (a failed
  `write_all` leaves it; a power cut before the edit naming it was saved leaves it unchecked) is
  then never rewritten. If the client restarts before the server has the blob, `open` deletes
  the short file and the edit is refused for a missing blob.
- The `broken` flag, set when a torn record could not be cut off, is never cleared. Once `keep`
  has emptied the file (`set_len(0)`), appending is safe again, but every later edit in that
  session still fails with "edits are no longer saved".
- The test `the_file_is_on_disk_after_flush` cannot fail: `Drop` flushes too, and reopening
  reads the page cache whether or not anything was synced. Real durability cannot be tested
  here.

**Files**

- `crates/mdaw-client/src/outbox.rs`

**Steps**

1. `put_blob`: return early only when the existing file's length equals `bytes.len()`;
   otherwise write it again. When `write_all` fails, remove the file (ignore the error of the
   removal) before returning the error.
2. `keep`: after `set_len(0)` succeeds, set `self.broken = false`.
3. Rename `the_file_is_on_disk_after_flush` to `an_edit_is_read_back_after_flush` and give it a
   doc comment saying what it checks (that `flush` returns and a reopened outbox reads the edit)
   and that it cannot check that the bytes reached the disk.
4. Tests: `a_short_blob_file_is_written_again` (write half of a blob's bytes to its path by
   hand, call `put_blob`, assert the file now holds all of them);
   `appending_works_again_once_the_file_is_emptied` (set `broken` through the existing
   `CutShort` test type with `stuck`, call `keep(&[])`, then assert an `append` succeeds).
   If `broken` cannot be reached from the test that way, report it rather than adding test-only
   code to the outbox.

**Do not** change `session.rs` or the record format.

**Done when** the gates pass.

---

## Not fixed here

| Finding | Why not |
|---|---|
| `append` says "could not be saved" when the hand-off to the sync thread fails after the record was written | Not a defect: with the sync thread gone, the edit is not on the disk for sure, so the notice is right. |
| Two clients on one profile share `plugins-starting` | True, but a profile has no lock at all: two clients on one profile already share and break the outbox. That needs a profile lock, a task of its own. |
| On the failed-reload path (foundation T2b), switched-off devices show no "switched off" status | Cosmetic, on a path that cannot be reached on demand. |
| `Held` (foundation T9) after an engine reload while a key is held: a second source on that pitch is silent until all sources let go | Needs an engine reload during a held note; negligible. |
| No test for the sync thread's error path or the T2b failure path | Both need fault injection the code does not have. |

## After the last task

Report per task: the commit, what changed, the failing output kept in "tests first" steps, and
every deviation from this plan. No hand test is needed: T1 to T3 are covered by the automatic
tests they add. If wanted, step C.1 of `docs/manual-tests/foundation.md` can be repeated once
with a plugin you do not have in the project.
