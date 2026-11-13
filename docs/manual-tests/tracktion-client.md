# Manual test session: two clients on Tracktion

The client now plays through Tracktion Engine (built with the `tracktion` feature). Shared
transport, checkout and network MIDI are gone: two people edit one project, and each plays it
on their own. The automated tests cover this with two client processes, no audio device and
no windows: same document, matching renders, concurrent edits, a crash and reconnect without
duplicates. This session is for what they cannot judge: **sound from a real device, playing
live, recording, real plugin windows, and how it feels with two windows side by side.**

About 30 minutes. One machine with speakers or headphones; a MIDI keyboard if you have one;
Vital (or another instrument) installed as VST3 or CLAP.

| | |
|---|---|
| Date | |
| Tester | |
| Commit (`git rev-parse --short HEAD`) | |
| OS, audio system | |
| Plugin used, and its format | |

**How to fill it in.** Tick a step when what you see and hear matches *Expect*. If not, leave
it unticked and write what happened under [Findings](#findings) with the step's number.
Things under [Known gaps](#known-gaps-not-failures) are not failures.

## 0. Preparation

- [ ] **0.1** `cmake --build target/native --target mdaw_tracktion_bridge mdaw-plugin-scan`
      Expect: finishes without errors (quick if the native part was built before).
- [ ] **0.2** `cargo xtask demo --fresh --tracktion`
      Expect: after the build, a server line in the terminal and two windows, Ann and Ben.
      Each terminal log has a line like `mdaw: Tracktion Engine, audio device open at … Hz`.
- [ ] **0.3** Look at both windows for the first seconds.
      Expect: "Scanning plugins…" shows for a moment and goes away; the windows stay
      responsive meanwhile.

## A. One project, two players

- [ ] **A.1** In Ann's window: add a track, double-click its lane to make a clip, double-click
      the clip and draw four or five notes.
      Expect: track, clip and notes appear in Ben's window as you make them.
- [ ] **A.2** In Ann's window press Space.
      Expect: Ann's playhead runs and you hear the notes (the built-in Pocket instrument).
      Ben's playhead does not move and Ben's Play button is not active.
- [ ] **A.3** While Ann plays, press Space in Ben's window, then click in Ben's ruler to
      another place.
      Expect: both play at once, each from its own position; you hear both. Seeking in one
      does nothing to the other.
- [ ] **A.4** Stop both. In Ben's window Shift-drag in the ruler to set a loop and switch
      "Loop" on; play Ben.
      Expect: Ben loops; Ann's window shows no loop and is not affected.
- [ ] **A.5** While Ben plays, drag a note to another pitch in Ann's window.
      Expect: Ben hears the new pitch on the next pass, without a gap or a restart.
- [ ] **A.6** Change the tempo in Ann's window while Ben plays.
      Expect: Ben's playback changes speed and carries on.

## B. Editing at the same time

- [ ] **B.1** Both windows, same clip: Ann adds notes at the start while Ben adds notes at
      the end, at the same time as well as you can.
      Expect: all notes are in both windows; none doubled, none missing.
- [ ] **B.2** Ann and Ben both drag the *same* note to different pitches.
      Expect: both windows end with the same pitch (whoever was last). The Changelog tab of
      the one who lost lists the displaced edit.
- [ ] **B.3** Ctrl+Z in Ann's window.
      Expect: Ann's last edit is undone in both windows; Ben's edits stay.

## C. Playing live and recording

- [ ] **C.1** Select the track in Ann's window and play the computer keyboard (the "Keys"
      panel shows the layout), stopped and while playing.
      Expect: you hear each note promptly, once; no note hangs after you let go. Ben hears
      nothing of it.
- [ ] **C.2** With a MIDI keyboard: choose it under "MIDI in" and play.
      Expect: as C.1 — and each note sounds once, not doubled.
- [ ] **C.3** Start recording in Ann's window, play a few notes on the beat against the
      playing clip, stop.
      Expect: a clip with your notes appears in both windows; played back, the notes are
      where you played them, not noticeably early or late.

## D. Plugins

- [ ] **D.1** In Ann's window select a track, open its devices, choose "Instrument…" and pick
      your plugin (VST3, LV2 or CLAP).
      Expect: "Starting the plugin…" briefly, then the device shows its parameters; the track
      now sounds like the plugin in Ann's window, and — the plugin being installed here — in
      Ben's too.
- [ ] **D.2** "Open window" on the device in Ann's window.
      Expect: the plugin's own window opens, drawn completely, and reacts to the mouse.
- [ ] **D.3** Turn a clearly audible knob in the plugin's window (a filter cutoff, say), or
      load another preset. Play Ben.
      Expect: within about two seconds Ben's sound has changed the same way. Nothing flips
      back afterwards in either window.
- [ ] **D.4** Move one of the device's sliders in the device tab instead.
      Expect: the sound changes at once in Ann's window and within about two seconds in Ben's.
- [ ] **D.5** Ctrl+Z in Ann's window after D.3.
      Expect: the plugin goes back to its previous settings in both windows.
- [ ] **D.6** Close the plugin's window with its close button.
      Expect: the device's button reads "Open window" again.
- [ ] **D.7** Close Ben's window. In another terminal start Ben again without plugins:
      `cargo xtask client Ben --tracktion --no-plugins`
      Expect: Ben's window shows the same project; the plugin's track is marked
      "plugin not installed" and is silent there; the other tracks play.

## E. Losing the connection, and a crash

- [ ] **E.1** Start the pieces separately for this: close everything, then
      `cargo xtask server` in one terminal and `cargo xtask client Ann --tracktion`,
      `cargo xtask client Ben --tracktion` in two others. Stop the server with Ctrl+C.
      Expect: both windows show "Reconnecting…" (or "Disconnected…").
- [ ] **E.2** With the server stopped, add three notes in Ann's window. Then kill Ann's
      client hard: `pkill -9 -f 'mdaw.*Ann'` (or kill its terminal).
      Expect: Ann's window is gone.
- [ ] **E.3** Start the server again, then Ann again (same commands).
      Expect: Ann's window comes up with her three notes in it, and they appear in Ben's
      window — each once.
- [ ] **E.4** Play both.
      Expect: both sound the same.

## F. Old engine, for comparison (optional)

- [ ] **F.1** `cargo run -p mdaw-client --features tracktion -- --old-engine`, make a track
      with a few notes; then the same without `--old-engine`.
      Expect: both play. The built-in instrument sounds similar but not the same (see gaps).
      Note under Findings how far apart they are for you.

## Known gaps (not failures)

- The built-in Pocket instrument is played by Tracktion's 4osc with mapped parameters: an
  approximation. Velocity response and filter character differ.
- A plugin's settings travel as a whole (its state), about 0.5–2 s after a change, not knob
  movement by knob movement.
- Plugins run inside the client: a plugin that crashes takes that client down. Restart it;
  edits the server had not confirmed come back from the outbox.
- Without an audio device Tracktion does not play at all (the status line says
  "Tracktion, no audio device").
- No stems: a track whose plugin is not installed is silent on that machine.
- The Changelog tab lists only edits displaced by someone else's, not a full history.
- lilv prints "Duplicate plugin" warnings in the terminal while scanning LV2.

## Findings

| Step | What happened | Expected instead |
|---|---|---|
| | | |
