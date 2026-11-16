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

## Findings

| Step | What happened | Expected instead |
|---|---|---|
| | | |
