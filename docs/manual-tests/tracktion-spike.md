# Manual test session: Tracktion spike

The spike under `native/` runs Tracktion Engine with VST3, LV2 and our own CLAP host. Its
automated checks cover loading, rendering, state, automation and latency by numbers. This
session is for what numbers cannot judge: **how it sounds**, what the **plugin windows show**,
and whether **turning a knob with the mouse** reaches the engine.

About 15 minutes. One machine with speakers or headphones and Vital installed as VST3 and CLAP
(any other plugin with a window will do; change the paths).

| | |
|---|---|
| Date | |
| Tester | |
| Commit (`git rev-parse --short HEAD`) | |
| OS, audio system | |

**How to fill it in.** Tick a step when what you see and hear matches *Expect*. If not, leave
it unticked and write what happened under [Findings](#findings) with the step's number.

## 0. Preparation

- [ ] **0.1** Build, as in `docs/tracktion-integration.md`:
      ```sh
      git -c url."https://github.com/".insteadOf="git@github.com:" submodule update --init --recursive --depth 1
      cmake -S native -B target/native -G Ninja -DCMAKE_BUILD_TYPE=RelWithDebInfo
      cmake --build target/native --target mdaw-te-spike
      S=target/native/spike/mdaw-te-spike_artefacts/RelWithDebInfo/mdaw-te-spike
      ```
      Expect: it finishes without errors (the first build takes several minutes).

## A. Sound

- [ ] **A.1** `$S play`
      Expect: about two seconds of a rising and falling arpeggio, a pause of about two seconds,
      then a held chord for about a second and a half, joined partway by a higher note and a
      bass note. No crackling. The last line is `RESULT ok=1`.
- [ ] **A.2** `$S play --type JACK`
      Expect: the same sound, `RESULT device_type=JACK`, last line `RESULT ok=1`.
- [ ] **A.3** `$S render --out /tmp/mdaw-demo.wav`, then play the file (`pw-play /tmp/mdaw-demo.wav`).
      Expect: `RESULT device_open=0` and `RESULT ok=1`; the file is the whole phrase — eight
      arpeggio notes, then the chord — about nine seconds or more.

## B. Plugin windows

Each command opens the plugin's window twice, on purpose: the second opening checks that a
window can be closed and opened again. Each stays for 30 seconds. Closing the window with its
close button ends that round early.

- [ ] **B.1** `$S editor --file /usr/lib/vst3/Vital.vst3 --format VST3 --hold 30000`
      Expect: a window titled with the plugin's name showing Vital's full interface, drawn
      completely, nothing cut off or black.
- [ ] **B.2** In that window, drag two or three knobs, then close the window.
      Expect: the knobs move as you drag. After closing, the terminal shows a `TURNED` line for
      each knob you moved, with its old and new value, and
      `RESULT round0_closed_by_hand=1`.
- [ ] **B.3** The window opens a second time on its own.
      Expect: the knobs you turned are where you left them. Close it. The run ends with
      `RESULT ok=1` and no crash message.
- [ ] **B.4** `$S editor --file /usr/lib/clap/Vital.clap --format CLAP --hold 30000`, and repeat
      B.1 to B.3.
      Expect: the same. This is the CLAP host written for this project, so look closely: the
      interface fills the window, reacts to the mouse, and redraws while you drag.
- [ ] **B.5** `$S editor --file /usr/lib/clap/ChowKick.clap --format CLAP --hold 30000`, turn a
      knob, close both rounds.
      Expect: as B.4.

## C. A plugin that misbehaves

These use the project's test synth, built as a CLAP plugin:

```sh
cargo build -p mdaw-test-plugin && cp target/debug/libmdaw_test_plugin.so /tmp/mdaw-test-synth.clap
```

- [ ] **C.1** `$S misbehave --file /tmp/mdaw-test-synth.clap --mode none`
      Expect: an arpeggio over a low drone for about four and a half seconds. `RESULT ok=1`.
- [ ] **C.2** `... --mode slow`
      Expect: normal for two seconds, then everything — drone included — breaks into a
      stutter. `RESULT max_cpu_after_trigger=1` (or close to it).
- [ ] **C.3** `... --mode hang`
      Expect: normal for two seconds, then silence or a stuck buzz; the program still ends by
      itself with `RESULT position_advanced_seconds=0` and `RESULT stop_returned=1`.
- [ ] **C.4** `... --mode crash`
      Expect: normal for two seconds, then the program dies (the shell reports "aborted").
      This is the expected result: Tracktion runs plugins in its own process.

C.2 to C.4 are not failures of the spike; they show what ADR 0002 has to live with. Say under
Findings if this is acceptable to you or if per-plugin isolation should be kept.

## Known gaps (not failures)

- Plugin windows cannot be resized from the host side yet.
- chipsounds and chipsynth-C64 do not restore parameters after reload, in any format.
- The spike is a console program: no project view, no mixer.

## Findings

| Step | What happened | Expected instead |
|---|---|---|
| | | |

Decisions (answered 2026-11-12): in-process plugins now, optional isolation as a later phase;
AGPLv3 for the client; ADR 0001 accepted after a second analysis.
