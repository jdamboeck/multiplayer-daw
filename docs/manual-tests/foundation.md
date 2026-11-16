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

## Findings

| Step | What happened | Expected instead |
|---|---|---|
| | | |
