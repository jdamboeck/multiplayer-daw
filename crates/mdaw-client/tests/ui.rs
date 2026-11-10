//! Drives the whole client headlessly: real pointer and key events in, document and audio out.

use egui::{Key, Modifiers, PointerButton, Pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use mdaw_client::ui::{arrange, piano_roll};
use mdaw_client::{DawApp, Session, Tab};
use mdaw_engine::Engine;
use mdaw_model::{Clip, ClipId, Note, PPQ, Tick, TrackId};

type App<'a> = Harness<'a, DawApp>;

fn harness<'a>() -> App<'a> {
    let app = DawApp::new(Session::starter(Engine::manual(48_000, false)));
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(1.0 / 60.0)
        .with_context_setup(DawApp::setup)
        .build_ui_state(|ui, app: &mut DawApp| app.ui(ui), app);
    harness.run_steps(3);
    harness
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

fn right_click(h: &mut App, pos: Pos2) {
    h.hover_at(pos);
    h.step();
    for pressed in [true, false] {
        let button = PointerButton::Secondary;
        h.event(egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        });
        h.step();
    }
    h.run_steps(2);
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

fn undo(h: &mut App) {
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(2);
}

fn track(h: &App) -> TrackId {
    h.state()
        .session
        .project()
        .ordered_tracks()
        .next()
        .expect("the starter project has a track")
        .0
}

fn clips(h: &App) -> Vec<(ClipId, Clip)> {
    let project = h.state().session.project();
    let track = project.ordered_tracks().next().unwrap().1;
    track
        .content
        .clips
        .iter()
        .map(|(id, clip)| (*id, (**clip).clone()))
        .collect()
}

fn notes(h: &App) -> Vec<Note> {
    clips(h)
        .first()
        .map(|(_, clip)| clip.notes.values().copied().collect())
        .unwrap_or_default()
}

/// Screen position in the arrangement, a little right of a tick so it cannot round into the previous cell.
fn lane(h: &App, row: usize, tick: Tick) -> Pos2 {
    h.state()
        .arrange
        .layout
        .expect("the arrangement is laid out")
        .pos(row, tick)
        + vec2(3.0, 0.0)
}

fn cell(h: &App, tick: Tick, pitch: u8) -> Pos2 {
    let layout = h.state().roll.layout.expect("a clip is open in the note editor");
    let pos = layout.pos(tick, pitch) + vec2(3.0, 0.0);
    assert!(
        layout.grid.contains(pos),
        "{pos:?} is outside the visible note grid {:?}",
        layout.grid
    );
    pos
}

/// Draws a two-bar clip on the first track and opens it.
fn with_open_clip<'a>() -> App<'a> {
    let mut h = harness();
    let (from, to) = (
        lane(&h, 0, Tick::ZERO),
        lane(&h, 0, Tick::bars(2)) - vec2(8.0, 0.0),
    );
    drag(&mut h, from, to);
    let at = lane(&h, 0, Tick::beats(1));
    double_click(&mut h, at);
    assert!(
        h.state().roll.layout.is_some(),
        "double-clicking the clip opens it"
    );
    h
}

fn peak(h: &mut App, frames: usize) -> f32 {
    let audio = h
        .state_mut()
        .session
        .engine_mut()
        .pump(frames)
        .expect("a manual engine");
    audio.iter().fold(0.0, |peak, sample| peak.max(sample.abs()))
}

#[test]
fn draw_a_clip_hear_it_undo_it() {
    let mut h = harness();
    assert!(clips(&h).is_empty());

    // Draw a clip over the first two bars.
    let (from, to) = (
        lane(&h, 0, Tick::ZERO),
        lane(&h, 0, Tick::bars(2)) - vec2(8.0, 0.0),
    );
    drag(&mut h, from, to);
    let drawn = clips(&h);
    assert_eq!(drawn.len(), 1);
    assert_eq!((drawn[0].1.start, drawn[0].1.length), (Tick::ZERO, Tick::bars(2)));
    assert_eq!(h.state().sel.clip, Some((track(&h), drawn[0].0)));

    // Open it and click a note in.
    let at = lane(&h, 0, Tick::beats(1));
    double_click(&mut h, at);
    assert_eq!(
        clips(&h).len(),
        1,
        "a double click on a clip must not add another"
    );
    let at = cell(&h, Tick::ZERO, 60);
    h.hover_at(at);
    h.step();
    h.drag_at(at);
    h.step();
    assert!(
        peak(&mut h, 4_800) > 0.01,
        "the key sounds while the pointer is down"
    );
    h.drop_at(at);
    h.run_steps(3);
    assert_eq!(
        notes(&h),
        [Note {
            start: Tick::ZERO,
            dur: Tick(PPQ / 4),
            pitch: 60,
            vel: 100
        }]
    );
    peak(&mut h, 96_000);
    assert!(
        peak(&mut h, 4_800) < 1e-4,
        "the preview note is released with the pointer"
    );

    // Hear it.
    h.key_press(Key::Space);
    h.run_steps(2);
    assert!(h.state().session.engine().is_playing());
    assert!(peak(&mut h, 12_000) > 0.01, "the drawn note is audible");
    h.run_steps(2);
    assert!(h.state().session.engine().position() > 0.0, "the playhead moves");
    h.key_press(Key::Space);
    h.run_steps(2);
    assert!(!h.state().session.engine().is_playing());

    // Undo the note, then the clip.
    undo(&mut h);
    assert!(notes(&h).is_empty());
    assert_eq!(clips(&h).len(), 1);
    undo(&mut h);
    assert!(clips(&h).is_empty());
    assert_eq!(h.state().sel.clip, None, "a removed clip is no longer selected");
    assert!(h.state().roll.layout.is_none());

    // And back again.
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
    h.run_steps(2);
    assert_eq!(notes(&h).len(), 1);
}

#[test]
fn clips_move_and_resize() {
    let mut h = harness();
    let (from, to) = (
        lane(&h, 0, Tick::ZERO),
        lane(&h, 0, Tick::bars(2)) - vec2(8.0, 0.0),
    );
    drag(&mut h, from, to);

    // Move by one bar, grabbing the body.
    let (from, to) = (lane(&h, 0, Tick::beats(2)), lane(&h, 0, Tick::beats(6)));
    drag(&mut h, from, to);
    let clip = &clips(&h)[0].1;
    assert_eq!((clip.start, clip.length), (Tick::bars(1), Tick::bars(2)));

    // Lengthen by a bar from the right edge.
    let (from, to) = (
        lane(&h, 0, Tick::bars(3)) - vec2(6.0, 0.0),
        lane(&h, 0, Tick::bars(4)) - vec2(3.0, 0.0),
    );
    drag(&mut h, from, to);
    let clip = &clips(&h)[0].1;
    assert_eq!((clip.start, clip.length), (Tick::bars(1), Tick::bars(3)));

    // Each gesture is one undo step.
    undo(&mut h);
    assert_eq!(clips(&h)[0].1.length, Tick::bars(2));
    undo(&mut h);
    assert_eq!(clips(&h)[0].1.start, Tick::ZERO);

    // Delete removes the selected clip.
    h.key_press(Key::Delete);
    h.run_steps(2);
    assert!(clips(&h).is_empty());
}

#[test]
fn notes_add_move_resize_velocity_delete() {
    let mut h = with_open_clip();
    let sixteenth = PPQ / 4;

    // Drag out a note of three sixteenths.
    let (from, to) = (
        cell(&h, Tick(sixteenth), 62),
        cell(&h, Tick(4 * sixteenth), 62) - vec2(8.0, 0.0),
    );
    drag(&mut h, from, to);
    assert_eq!(
        notes(&h),
        [Note {
            start: Tick(sixteenth),
            dur: Tick(3 * sixteenth),
            pitch: 62,
            vel: 100
        }]
    );

    // Move it two sixteenths later and three semitones up.
    let (from, to) = (
        cell(&h, Tick(2 * sixteenth), 62),
        cell(&h, Tick(4 * sixteenth), 65),
    );
    drag(&mut h, from, to);
    assert_eq!(
        notes(&h),
        [Note {
            start: Tick(3 * sixteenth),
            dur: Tick(3 * sixteenth),
            pitch: 65,
            vel: 100
        }]
    );

    // Shorten it by a sixteenth from its right edge.
    let (from, to) = (
        cell(&h, Tick(6 * sixteenth), 65) - vec2(6.0, 0.0),
        cell(&h, Tick(5 * sixteenth), 65) - vec2(6.0, 0.0),
    );
    drag(&mut h, from, to);
    assert_eq!(notes(&h)[0].dur, Tick(2 * sixteenth));

    // Pull its velocity down in the velocity lane; the whole drag is one undo step.
    let layout = h.state().roll.layout.unwrap();
    let x = layout.pos(Tick(3 * sixteenth), 65).x + 1.0;
    let (top, bottom) = (layout.velocity.top() + 8.0, layout.velocity.bottom() - 4.0);
    drag(&mut h, Pos2::new(x, top), Pos2::new(x, bottom));
    let vel = notes(&h)[0].vel;
    assert!(vel < 20, "velocity after dragging to the bottom: {vel}");
    undo(&mut h);
    assert_eq!(notes(&h)[0].vel, 100);

    // A click adds a note of the length used last.
    let at = cell(&h, Tick(8 * sixteenth), 60);
    click(&mut h, at);
    assert_eq!(notes(&h).len(), 2);
    assert!(
        notes(&h)
            .iter()
            .any(|n| n.start == Tick(8 * sixteenth) && n.dur == Tick(2 * sixteenth))
    );

    // Right-click deletes one; Delete removes the selection.
    let at = cell(&h, Tick(8 * sixteenth), 60);
    right_click(&mut h, at);
    assert_eq!(notes(&h).len(), 1);
    let at = cell(&h, Tick(3 * sixteenth), 65);
    click(&mut h, at);
    assert_eq!(notes(&h).len(), 1, "clicking a note selects it");
    h.key_press(Key::Delete);
    h.run_steps(2);
    assert!(notes(&h).is_empty());
    assert_eq!(
        clips(&h).len(),
        1,
        "Delete in the note editor leaves the clip alone"
    );
}

#[test]
fn tracks_and_devices() {
    let mut h = harness();
    h.get_by_label("+ Track").click();
    h.run_steps(3);
    let project = h.state().session.project().clone();
    assert_eq!(project.track_order.len(), 2);
    let second = project.ordered_tracks().nth(1).unwrap().0;
    assert_eq!(h.state().sel.track, Some(second));
    assert_eq!(arrange::row_of(&project, second), Some(1));

    // A clip drawn on the second row lands on the second track.
    let (from, to) = (
        lane(&h, 1, Tick::bars(1)),
        lane(&h, 1, Tick::bars(2)) - vec2(8.0, 0.0),
    );
    drag(&mut h, from, to);
    let project = h.state().session.project().clone();
    assert_eq!(project.track(second).unwrap().content.clips.len(), 1);
    assert!(clips(&h).is_empty());

    // The device tab shows the synth; turning it off is an edit like any other.
    h.state_mut().tab = Tab::Device;
    h.run_steps(3);
    h.get_by_label("Cutoff");
    h.get_by_label("On").click();
    h.run_steps(3);
    let project = h.state().session.project().clone();
    assert!(project.track(second).unwrap().content.devices[0].bypass);
    undo(&mut h);
    let project = h.state().session.project().clone();
    assert!(!project.track(second).unwrap().content.devices[0].bypass);

    assert_eq!(piano_roll::PREVIEW_KEY, 1);
}

/// Writes PNGs of the main views. Run with
/// `MDAW_SHOTS=/some/dir cargo test -p mdaw-client --test ui -- --ignored screenshots`.
#[test]
#[ignore = "needs a GPU adapter; for looking at the UI"]
fn screenshots() {
    let Ok(dir) = std::env::var("MDAW_SHOTS") else {
        return;
    };
    let mut h = with_open_clip();
    let sixteenth = PPQ / 4;
    for (step, pitch) in [
        (0, 60),
        (2, 63),
        (4, 67),
        (6, 63),
        (8, 65),
        (10, 68),
        (12, 72),
        (14, 67),
    ] {
        let (from, to) = (
            cell(&h, Tick(step * sixteenth), pitch),
            cell(&h, Tick((step + 2) * sixteenth), pitch) - vec2(8.0, 0.0),
        );
        drag(&mut h, from, to);
    }
    h.get_by_label("+ Track").click();
    h.run_steps(3);
    let (from, to) = (
        lane(&h, 1, Tick::bars(1)),
        lane(&h, 1, Tick::bars(3)) - vec2(8.0, 0.0),
    );
    drag(&mut h, from, to);
    let at = lane(&h, 0, Tick::beats(1));
    double_click(&mut h, at);
    h.run_steps(3);
    h.render()
        .expect("render")
        .save(format!("{dir}/notes.png"))
        .expect("save");
    h.state_mut().tab = Tab::Device;
    h.run_steps(3);
    h.render()
        .expect("render")
        .save(format!("{dir}/device.png"))
        .expect("save");
}
