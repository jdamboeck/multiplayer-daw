# Manual test session: the foundation plan

The foundation plan (`docs/foundation-plan.md`) changes things the automated tests cannot
judge: how the playhead looks while it moves, and later how the client behaves with real
devices and plugins. This session checks them, one section per stage of the plan.

A few minutes per section. One machine with speakers or headphones.

| | |
|---|---|
| Date | |
| Tester | |
| Commit (`git rev-parse --short HEAD`) | |
| OS, audio system | |

**How to fill it in.** Tick a step when what you see matches *Expect*. If not, leave it
unticked and write what happened under [Findings](#findings) with the step's number.

## 0. Preparation

- [ ] **0.1** `cargo build --release` (it configures and builds the native part in
      `target/native`)
      Expect: finishes without errors (quick if the native part was built before).

## A. Playhead

The playhead is drawn where the transport is now: the last position the engine reported (about
30 times a second) carried forward by how old it is.

- [ ] **A.1** Start `cargo xtask client Ann --release`, put some notes in a clip, play.
      Expect: the playhead in the arrangement and in the piano roll moves smoothly, without
      steps.
- [ ] **A.2** Loop one bar and play for ten rounds.
      Expect: the playhead never shows past the loop end and never jumps backwards except at
      the wrap.
- [ ] **A.3** Stop.
      Expect: the playhead stands still (no trembling).
- [ ] **A.4** Press play, then seek by clicking the ruler while playing.
      Expect: the playhead goes where clicked and continues from there.
- [ ] **A.5** Record a few notes with the computer keyboard while playing.
      Expect: they land where they were played (recording timing is unchanged).

## B. Unsent edits

Edits not yet confirmed by the server are kept in a file before they are sent; a thread now
forces that file to the disk, so the frame never waits for it. Two clients and a server, each in
its own terminal: `cargo xtask server`, `cargo xtask client Ann`, `cargo xtask client Ben`.

- [ ] **B.1** Ann drags a knob of the built-in instrument for ten seconds.
      Expect: no stutter in Ann's window; Ben sees the knob move.
- [ ] **B.2** Stop the server. Ann adds three notes. Kill Ann's client (`kill -9 <pid>`, the pid
      from `pgrep -af "mdaw.*--name Ann"`). Start the server again, then Ann.
      Expect: the three notes are there in Ann's window and reach Ben.
- [ ] **B.3** Stop the server. Ann loads a plugin and changes its preset. Kill Ann's client,
      start the server and Ann again.
      Expect: the plugin with that preset is there and reaches Ben.
- [ ] **B.4** After everything is confirmed (wait a few seconds), look at Ann's file of unsent
      edits: `ls -l target/demo/profiles/ann/projects/*/pending` (the xtask keeps each client's
      profile under `target/demo/profiles/<name>`; the folder under `projects` is the server's
      fingerprint).
      Expect: the file is empty (size 0), and `target/demo/profiles/ann/projects/*/blobs` holds
      no files.

## C. A plugin that takes mdaw down

A plugin that was being started when mdaw last ended is switched off on this computer, and
"Try again" in its device tab switches it on. A plugin counts as started 5 seconds after it was
loaded. No plugin at hand crashes on demand after loading (the bridge's `bad-crash.clap` crashes
while it is examined, so the scan already refuses it), so these steps kill the client by hand.

- [ ] **C.1** Start the client, put any plugin on a track, and within 5 seconds kill the client
      (`kill -9 <pid>`, or `pkill -9 mdaw`). Start it again.
      Expect: a notice names the plugin; the track says "plugin failed"; the device tab gives
      the reason (switched off, because mdaw ended while starting it); the other tracks play.
- [ ] **C.2** In the device tab, press "Try again". Then restart the client the normal way.
      Expect: the plugin loads and sounds; after the restart it is still on.
- [ ] **C.3** Put a plugin on a track and close the window the normal way right away. Start
      again.
      Expect: nothing is switched off.
- [ ] **C.4** Put a plugin on a track, wait ten seconds, kill the client, start it.
      Expect: nothing is switched off.
- [ ] **C.5** With two clients on a server (`cargo xtask server`, `cargo xtask client Ann`,
      `cargo xtask client Ben`): switch Ann's plugin off as in C.1.
      Expect: Ben, who has the plugin, still hears the track; nothing about it reached the
      document.

## Findings

| Step | What happened | Expected instead |
|---|---|---|
| | | |
