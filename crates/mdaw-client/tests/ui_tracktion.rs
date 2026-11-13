//! The whole client headlessly on Tracktion Engine: pointer and key events in, document and a
//! render out. One engine per process, so one test. Needs the `tracktion` feature. No audio
//! device: playing is what the transport was asked for; sound is checked by rendering.

use egui::{Key, Pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use mdaw_client::audio_tracktion::{TracktionAudio, TracktionOptions};
use mdaw_client::{DawApp, Session, Tab};
use mdaw_model::{BuiltinKind, DeviceKind, Note, PPQ, Tick, two_osc};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

type App<'a> = Harness<'a, DawApp>;

fn scratch() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/phase5-scratch/ui-tracktion");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn drag(h: &mut App, from: Pos2, to: Pos2) {
    h.hover_at(from);
    h.step();
    h.drag_at(from);
    h.step();
    for step in 1..=6 {
        h.hover_at(from + (to - from) * (step as f32 / 6.0));
        h.step();
    }
    h.drop_at(to);
    h.run_steps(2);
}

fn click(h: &mut App, pos: Pos2) {
    h.hover_at(pos);
    h.step();
    h.drag_at(pos);
    h.step();
    h.drop_at(pos);
    h.run_steps(2);
}

fn double_click(h: &mut App, pos: Pos2) {
    h.hover_at(pos);
    h.step();
    for _ in 0..2 {
        h.drag_at(pos);
        h.step();
        h.drop_at(pos);
        h.step();
    }
    h.run_steps(2);
}

fn lane(h: &App, row: usize, tick: Tick) -> Pos2 {
    h.state()
        .arrange
        .layout
        .expect("the arrangement is laid out")
        .pos(row, tick)
        + vec2(3.0, 0.0)
}

fn cell(h: &App, tick: Tick, pitch: u8) -> Pos2 {
    h.state().roll.layout.expect("a clip is open").pos(tick, pitch) + vec2(3.0, 0.0)
}

/// Renders the first bar through the session's engine; peak, a measure of brightness (the
/// mean absolute difference between neighbouring samples, relative to the level) and one of low
/// end (the share of the energy that passes two one-pole low-passes at 100 Hz).
fn listen(h: &mut App, name: &str) -> (f64, f64, f64) {
    let path = scratch().join(format!("{name}.wav"));
    let _ = std::fs::remove_file(&path);
    h.run_steps(2);
    let audio = h.state_mut().session.audio_mut();
    let te = audio.as_any_mut().downcast_mut::<TracktionAudio>().unwrap();
    te.wait_idle();
    h.state_mut()
        .session
        .render(&path, Tick::ZERO, Tick::bars(1))
        .expect("render");
    let data = std::fs::read(&path).unwrap();
    let pcm = &data[data.windows(4).position(|w| w == b"data").unwrap() + 8..];
    let samples: Vec<f64> = pcm
        .chunks_exact(6)
        .map(|s| (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f64 / 8_388_608.0)
        .collect();
    let peak = samples.iter().fold(0.0f64, |m, s| m.max(s.abs()));
    let level = samples.iter().map(|s| s.abs()).sum::<f64>().max(1e-12);
    let change = samples.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>();
    let a = 1.0 - (-2.0 * std::f64::consts::PI * 100.0 / 44_100.0f64).exp();
    let (mut y1, mut y2, mut low, mut all) = (0.0, 0.0, 0.0, 0.0);
    for x in &samples {
        y1 += a * (x - y1);
        y2 += a * (y1 - y2);
        low += y2 * y2;
        all += x * x;
    }
    let lowness = low / all.max(1e-12);
    println!(
        "RESULT {name}: peak={peak:.4} brightness={:.4} lowness={lowness:.4}",
        change / level
    );
    (peak, change / level, lowness)
}

/// The first track's first device.
fn instrument(h: &App) -> mdaw_model::Device {
    let project = h.state().session.project();
    let track = project.ordered_tracks().next().unwrap().1;
    track.content.devices[0].clone()
}

#[test]
fn the_window_plays_through_tracktion() {
    let started = Instant::now();
    let mut options = TracktionOptions::new(scratch().join("engine"));
    options.scan = Vec::new();
    let audio = TracktionAudio::new(options).expect("the engine starts");
    let app = DawApp::new(Session::starter(audio));
    let mut h = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(1.0 / 60.0)
        .with_context_setup(DawApp::setup)
        .build_ui_state(|ui, app: &mut DawApp| app.ui(ui), app);
    h.run_steps(3);
    assert!(
        h.state().session.audio().description().starts_with("Tracktion"),
        "the window runs on Tracktion: {}",
        h.state().session.audio().description()
    );

    // A clip drawn in the arrangement, opened, and a note clicked in.
    let (from, to) = (
        lane(&h, 0, Tick::ZERO),
        lane(&h, 0, Tick::bars(2)) - vec2(8.0, 0.0),
    );
    drag(&mut h, from, to);
    let at = lane(&h, 0, Tick::beats(1));
    double_click(&mut h, at);
    assert!(h.state().roll.layout.is_some(), "the clip is open");
    let (from, to) = (
        cell(&h, Tick::ZERO, 60),
        cell(&h, Tick(PPQ * 2), 60) - vec2(6.0, 0.0),
    );
    drag(&mut h, from, to);
    let at = cell(&h, Tick(PPQ * 2), 67);
    click(&mut h, at);
    let notes: Vec<Note> = {
        let project = h.state().session.project();
        let track = project.ordered_tracks().next().unwrap().1;
        track
            .content
            .clips
            .values()
            .flat_map(|c| c.notes.values().copied())
            .collect()
    };
    assert_eq!(notes.len(), 2, "{notes:?}");
    let (peak, bright, low) = listen(&mut h, "notes");
    assert!(
        peak > 0.05,
        "the notes drawn in the window sound through Tracktion"
    );

    // Space plays: without a device, that is the wish the engine keeps.
    h.key_press(Key::Space);
    h.run_steps(3);
    assert!(h.state().session.is_playing());
    let audio = h.state_mut().session.audio_mut();
    let te = audio.as_any_mut().downcast_mut::<TracktionAudio>().unwrap();
    te.wait_idle();
    assert!(te.engine().state().playing, "the engine was told to play");
    h.key_press(Key::Space);
    h.run_steps(3);
    assert!(!h.state().session.is_playing());

    // The track's instrument is the built-in one for Tracktion, shown as "Pocket" in its
    // sections; turning its filter frequency down is an edit the engine follows, and the whole
    // drag is one undo step.
    assert!(
        matches!(
            instrument(&h).kind,
            DeviceKind::Builtin {
                kind: BuiltinKind::TwoOsc,
                ..
            }
        ),
        "the new project's track has the Tracktion instrument"
    );
    h.state_mut().tab = Tab::Device;
    h.run_steps(3);
    h.get_by_label("Pocket");
    for group in [
        "OSC 1",
        "OSC 2",
        "SUB",
        "FILTER",
        "FILTER ENV",
        "AMP ENV",
        "GLOBAL",
    ] {
        h.get_by_label(group);
    }
    let knob = h.get_by_label("FILTER Freq").rect().center();
    drag(&mut h, knob, knob + vec2(0.0, 120.0));
    let freq = instrument(&h).params.get(&two_osc::id::FILTER_FREQ).copied();
    println!("RESULT freq={freq:?}");
    assert!(
        freq.is_some_and(|c| c < 1000.0),
        "the knob wrote the frequency: {freq:?}"
    );
    let (peak2, bright2, _) = listen(&mut h, "darker");
    assert!(peak2 > 0.01, "still sounding");
    assert!(
        bright2 < 0.7 * bright,
        "duller after the frequency went down ({bright} -> {bright2})"
    );

    // One undo puts it back, in the engine too.
    h.key_press_modifiers(egui::Modifiers::COMMAND, Key::Z);
    h.run_steps(3);
    assert_eq!(
        instrument(&h).params.get(&two_osc::id::FILTER_FREQ),
        None,
        "the drag was one undo step"
    );
    let (_, bright3, _) = listen(&mut h, "undone");
    assert!(
        bright3 > 0.85 * bright,
        "bright again after undo ({bright} -> {bright3})"
    );

    // SUB On, clicked: an edit, and more low end.
    let sub_on = |h: &App| instrument(h).params.get(&two_osc::id::SUB_ON).copied();
    let toggle = h.get_by_label("SUB On").rect().center();
    click(&mut h, toggle);
    assert_eq!(sub_on(&h), Some(1.0), "SUB On is an edit");
    let (_, _, low_on) = listen(&mut h, "sub_on");
    assert!(low_on > 1.5 * low, "SUB On: more low end ({low} -> {low_on})");
    if let Ok(dir) = std::env::var("MDAW_SHOTS") {
        // Needs a GPU adapter, as the screenshots of `tests/ui.rs`. The editor panel is pulled
        // up first, so the whole instrument shows.
        let edge = h.get_by_label("Notes").rect().top() - 1.0;
        drag(&mut h, egui::pos2(640.0, edge), egui::pos2(640.0, edge - 200.0));
        h.run_steps(2);
        let path = format!("{dir}/two_osc.png");
        h.render().expect("render").save(&path).expect("save");
        println!("RESULT screenshot={path}");
    }
    let toggle = h.get_by_label("SUB On").rect().center();
    click(&mut h, toggle);
    assert_eq!(sub_on(&h), Some(0.0), "SUB Off again");
    let (_, _, low_off) = listen(&mut h, "sub_off");
    assert!(
        low_off < 0.7 * low_on,
        "SUB Off: low end gone ({low_on} -> {low_off})"
    );

    // A new track gets the same instrument.
    h.get_by_label("+ Track").click();
    h.run_steps(3);
    let project = h.state().session.project().clone();
    let (_, second) = project.ordered_tracks().nth(1).expect("a second track");
    assert!(matches!(
        second.content.devices[0].kind,
        DeviceKind::Builtin {
            kind: BuiltinKind::TwoOsc,
            ..
        }
    ));
    let first = project.ordered_tracks().next().unwrap().0;
    h.state_mut().sel.track = Some(first);
    h.run_steps(2);

    // Keys on the computer keyboard play the track live (heard only with a device; here, no
    // error and the note is held and let go).
    h.key_down(Key::A);
    h.run_steps(2);
    assert_eq!(h.state().session.live().held().len(), 1);
    h.key_up(Key::A);
    h.run_steps(2);
    assert!(h.state().session.live().held().is_empty());
    assert!(started.elapsed() < Duration::from_secs(120));
    println!("RESULT ui_tracktion: {:?}", started.elapsed());
}
