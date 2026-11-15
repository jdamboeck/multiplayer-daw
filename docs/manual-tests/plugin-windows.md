# Manual test session: plugin windows and a busy engine (ABI 5)

Plugin windows now open through an explicit lifecycle (Closed → Opening → Open → Closing), the
plugin scan no longer holds the engine, and the engine's worker puts window and knob requests
ahead of background polling. The automated tests cover this on a virtual display without a
window manager, with a test plugin whose window is an empty area. This session is for what they
cannot judge: **real VST3, LV2 and CLAP windows under your window manager (XWayland), how quickly
they appear and draw, and whether the app stays responsive while plugins are scanned.**

About 25 minutes. One machine with an audio device; plugins with windows in at least two formats
(here: Vital or ChowKick as CLAP, vitOTT or Fasttracker II as VST3, Dexed or ChowKick as LV2).

| | |
|---|---|
| Date | |
| Tester | |
| Commit (`git rev-parse --short HEAD`) | |
| OS, window manager / compositor | |
| Plugins used, and their formats | |

**How to fill it in.** Tick a step when what you see matches *Expect*. If not, leave it unticked
and write what happened under [Findings](#findings) with the step's number. Things under
[Known gaps](#known-gaps-not-failures) are not failures. Every step that says "trace" means: the
client was started with `MDAW_TRACE=1` and its terminal shows `mdaw-trace` lines; copy the lines
around a slow moment into Findings.

## 0. Preparation

- [ ] **0.1** `cmake --build target/native` — Expect: finishes without errors.
- [ ] **0.2** `MDAW_TRACE=1 cargo xtask demo --fresh`
      Expect: two windows, Ann and Ben. "Scanning plugins…" shows for a few seconds (a cold scan
      of everything installed; about 4 s on the development machine) and goes away. Both
      windows stay responsive meanwhile (scroll, add a track).
- [ ] **0.3** Close both clients and the server (Ctrl-C in the terminal). Start again without
      `--fresh`: `MDAW_TRACE=1 cargo xtask demo`.
      Expect: the plugin menus are full at once (the catalog from the last run is shown before the
      new scan ends); "Scanning plugins…" is short.

## A. Windows open, close and reopen

In Ann's window: add a track per plugin you test, and put one plugin on each.

- [ ] **A.1** Device tab of the first plugin: click "Open window".
      Expect: the button reads "Opening…" for a moment at most, then "Close window"; the plugin's
      window appears, drawn, with your window manager's title bar ("<plugin> - mdaw").
      Trace: a line `show editor … hasEditor … createEditorIfNeeded … window and setVisible …` and
      `first loop turn after show …`; note the createEditorIfNeeded time per plugin in Findings.
- [ ] **A.2** Click "Close window", then "Open window" again, quickly, five times in a row.
      Expect: at the end the window is open and the button says "Close window"; never two windows
      for one plugin; no window left behind after the last close.
- [ ] **A.3** Close the plugin's window with its title-bar close button (or your window manager's
      shortcut). Expect: within a frame or two the button says "Open window" again.
- [ ] **A.4** Close it with the title-bar button and at once click "Open window" in the app.
      Expect: the window is open afterwards and stays open (wait 2 s); the button says "Close
      window".
- [ ] **A.5** Open the windows of two plugins of different formats at once, turn a knob in each.
      Expect: both windows stay open and react; the device tab's sliders follow the knobs within
      about a second.
- [ ] **A.6** With a window open, click "Remove" on that device.
      Expect: the window disappears; the device is gone; no error notice.
- [ ] **A.7** Repeat A.1–A.3 with a VST3 and an LV2 plugin.
      Expect: the same. LV2 windows that animate (meters, envelope displays) keep animating
      while the window is open (the LV2 idle callback is driven by the message loop).

## B. While the engine is busy

- [ ] **B.1** A slow scan with plugins known from before. Close the demo. Then:
      ```sh
      mkdir -p /tmp/mdaw-hang && cp target/native/tracktion-bridge/bad-hang.clap /tmp/mdaw-hang/ \
        && touch /tmp/mdaw-hang/bad-hang.clap
      MDAW_TRACE=1 MDAW_SCAN_TIMEOUT_MS=20000 CLAP_PATH=/tmp/mdaw-hang cargo xtask demo
      ```
      (`bad-hang.clap` is a test plugin that never finishes loading; the scanner gives up on it
      after 20 s. If the file is elsewhere, `find target/native -name bad-hang.clap`.)
      Expect: "Scanning plugins…" shows for about 20 s. Meanwhile: the plugins of section A play
      and their devices are not "Loading"; their windows open as quickly as in A.1; knobs and
      play/stop react at once; adding a track or a note reaches Ben's window at once.
- [ ] **B.2** Still during that scan: quit Ann's client (close the window).
      Expect: it ends within about a second, and `pgrep -f mdaw-plugin-scan` afterwards finds
      nothing.
- [ ] **B.3** After the scan, look at the failed plugins (if the app lists them) or the trace.
      Expect: bad-hang is recorded as hung ("no result within 20000 ms").
- [ ] **B.4** A plugin window open while you edit a lot: open one plugin's window, then drag
      notes around in a clip for ten seconds while turning a knob in the plugin's window now and
      then. Expect: the window and the app stay fluid; after you stop, the plugin's settings reach
      Ben's window within about 2 s.
- [ ] **B.5** Remove `/tmp/mdaw-hang` and restart normally. Expect: no "Scanning plugins…" longer
      than in 0.3.

## Known gaps (not failures)

- A plugin that hangs inside a call (loading, its window, saving its state) still freezes the
  engine and, for that time, every other plugin request. Nothing can interrupt a plugin's code.
- A plugin whose state takes long to save delays the next engine job by that long, once per
  look at its window (every 500 ms at most), no longer piling up.
- The very first window of a plugin can take longer than later ones (vitOTT: 0.8 s the first
  time after a cold start, 60 ms afterwards).
- Plugin windows are not resizable from the host side yet; plugins that resize themselves may.
- lilv prints "Duplicate plugin" warnings in the terminal while scanning LV2.

## Findings

| Step | What happened | Expected instead |
|---|---|---|
| | | |
