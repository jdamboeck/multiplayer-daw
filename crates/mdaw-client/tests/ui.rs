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

/// Opens a submenu of an open menu, the way a pointer does: by resting on its button.
fn open_submenu(h: &mut App, label: &str) {
    let at = h.get_by_label(label).rect().center();
    h.hover_at(at);
    h.run_steps(3);
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

/// Where a track's name sits in its header.
fn header_name(h: &App, row: usize) -> Pos2 {
    let layout = h.state().arrange.layout.expect("the arrangement is laid out");
    egui::pos2(layout.headers.left() + 30.0, layout.row_top(row) + 17.0)
}

fn playing(h: &App) -> bool {
    h.state().session.engine().is_playing()
}

#[test]
fn a_window_without_a_server_shows_no_server_controls() {
    let h = harness();
    assert!(h.query_by_label_contains("Online").is_none());
}

#[test]
fn space_plays_the_loop_goes_round_and_stop_twice_goes_back_to_the_start() {
    let mut h = with_open_clip();
    let at = cell(&h, Tick::ZERO, 60);
    click(&mut h, at);
    // Shift-drag in the ruler sets the loop region to bar 1, and turns looping on.
    let layout = h.state().arrange.layout.unwrap();
    let ruler = |tick: Tick| egui::pos2(layout.axis.x(tick) + 3.0, layout.ruler.center().y);
    h.event(egui::Event::ModifiersChanged(Modifiers::SHIFT));
    drag(&mut h, ruler(Tick::ZERO), ruler(Tick::bars(1)));
    h.event(egui::Event::ModifiersChanged(Modifiers::NONE));
    assert!(h.state().loop_on);
    assert_eq!(
        h.state().loop_range,
        mdaw_model::TickRange::new(Tick::ZERO, Tick::bars(1))
    );
    h.get_by_label("Loop").click();
    h.run_steps(2);
    assert!(!h.state().loop_on, "the button turns it off");
    h.get_by_label("Loop").click();
    h.run_steps(2);
    assert!(h.state().loop_on);
    let loop_end = h.state().loop_range.end.0 as f64;

    h.key_press(Key::Space);
    h.run_steps(2);
    assert!(playing(&h));
    // Twice round a loop of one bar at 120 bpm: 4 seconds.
    for _ in 0..10 {
        assert!(peak(&mut h, 24_000) >= 0.0);
        let at = h.state().session.engine().position();
        assert!(at < loop_end, "the playhead stays inside the loop, at {at}");
    }
    h.key_press(Key::Space);
    h.run_steps(2);
    assert!(!playing(&h), "space again pauses");
    assert!(h.state().session.engine().position() > 0.0);

    for _ in 0..2 {
        h.get_by_label("Stop").click();
        h.run_steps(2);
    }
    assert_eq!(h.state().session.engine().position(), 0.0);
    assert!(h.query_by_label_contains("1.1.1").is_some());
}

#[test]
fn a_track_is_renamed_from_its_header_and_silent_when_muted() {
    let mut h = with_open_clip();
    let at = cell(&h, Tick::ZERO, 60);
    click(&mut h, at);

    let name = header_name(&h, 0);
    // A third click within egui's double-click time would make this a triple click.
    h.run_steps(30);
    double_click(&mut h, name);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.event(egui::Event::Text("Keys".into()));
    h.key_press(Key::Enter);
    h.run_steps(3);
    let track = track(&h);
    let renamed = |h: &App| {
        h.state()
            .session
            .project()
            .track(track)
            .unwrap()
            .shell
            .name
            .clone()
    };
    assert_eq!(renamed(&h), "Keys");

    h.get_by_label("M").click();
    h.run_steps(2);
    h.key_press(Key::Space);
    h.run_steps(2);
    assert_eq!(peak(&mut h, 24_000), 0.0, "a muted track is silent");
    h.get_by_label("M").click();
    for label in ["Pause", "Stop", "Play"] {
        h.run_steps(2);
        h.get_by_label(label).click();
    }
    h.run_steps(2);
    assert!(peak(&mut h, 24_000) > 0.01, "and sounds again");
}

#[test]
fn undo_and_redo_go_one_gesture_at_a_time() {
    let mut h = with_open_clip();
    for beat in 0..3 {
        let at = cell(&h, Tick::beats(beat), 60 + beat as u8);
        click(&mut h, at);
    }
    assert_eq!(notes(&h).len(), 3);
    let redo = |h: &mut App| {
        h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
        h.run_steps(2);
    };
    for left in [2, 1, 0] {
        undo(&mut h);
        assert_eq!(notes(&h).len(), left);
    }
    undo(&mut h);
    assert!(clips(&h).is_empty(), "then the clip");
    redo(&mut h);
    assert_eq!((clips(&h).len(), notes(&h).len()), (1, 0));
    for count in [1, 2, 3] {
        redo(&mut h);
        assert_eq!(notes(&h).len(), count);
    }
    let pitches: Vec<u8> = notes(&h).iter().map(|n| n.pitch).collect();
    assert_eq!(pitches.len(), 3);
    assert!(pitches.contains(&60) && pitches.contains(&62));
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
    h.state_mut()
        .session
        .enable_plugins(mdaw_plugin::Host::test_synth_only());
    h.run_steps(3);
    let add = h.get_by_label("Instrument…").rect().center();
    click(&mut h, add);
    open_submenu(&mut h, "CLAP (1)");
    h.render()
        .expect("render")
        .save(format!("{dir}/instrument_menu.png"))
        .expect("save");
}

#[test]
fn the_computer_keyboard_plays_the_selected_track() {
    let mut h = harness();
    let id = track(&h);
    let held = |h: &App| h.state().session.live().held();

    h.key_down(Key::A);
    h.run_steps(2);
    assert_eq!(held(&h), [(id, 60)], "A is middle C");
    // Holding a key down does not start the note again.
    h.key_down(Key::A);
    h.key_down(Key::W);
    h.run_steps(2);
    assert_eq!(held(&h), [(id, 60), (id, 61)]);
    h.key_up(Key::A);
    h.key_up(Key::W);
    h.run_steps(2);
    assert!(held(&h).is_empty());

    // X goes an octave up.
    h.key_press(Key::X);
    h.key_down(Key::K);
    h.run_steps(2);
    assert_eq!(held(&h), [(id, 84)]);
    // A note ends where it started, even after the octave changed.
    h.key_press(Key::Z);
    h.key_up(Key::K);
    h.run_steps(2);
    assert!(held(&h).is_empty());

    // With a modifier, a key is a shortcut and plays nothing.
    h.key_down_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(2);
    assert!(held(&h).is_empty());
    h.key_up_modifiers(Modifiers::COMMAND, Key::A);

    // Leaving the window lets go of everything.
    h.key_down(Key::D);
    h.run_steps(2);
    assert_eq!(held(&h), [(id, 64)]);
    h.event(egui::Event::WindowFocused(false));
    h.run_steps(2);
    assert!(held(&h).is_empty());

    // Switched off, the keys play nothing.
    let keys = h.get_by_label("Keys").rect().center();
    click(&mut h, keys);
    h.key_down(Key::A);
    h.run_steps(2);
    assert!(held(&h).is_empty());
}

#[test]
fn typing_a_track_name_plays_nothing() {
    let mut h = harness();
    let name = header_name(&h, 0);
    h.run_steps(30);
    double_click(&mut h, name);
    assert!(h.ctx.egui_wants_keyboard_input(), "the name is being edited");
    h.key_down(Key::A);
    h.run_steps(2);
    assert!(h.state().session.live().held().is_empty());
}

#[test]
fn what_is_played_while_recording_becomes_a_clip_when_playback_stops() {
    let mut h = harness();
    let rec = h.get_by_label("Rec").rect().center();
    click(&mut h, rec);
    assert!(h.state().session.is_recording());
    h.key_press(Key::Space);
    h.run_steps(2);
    h.key_down(Key::A);
    h.run_steps(5);
    h.key_up(Key::A);
    h.key_down(Key::D);
    h.run_steps(2);
    assert!(clips(&h).is_empty(), "nothing is written while playing");
    h.key_press(Key::Space);
    h.run_steps(2);
    let written = clips(&h);
    assert_eq!(written.len(), 1);
    let take = &written[0].1;
    assert_eq!(take.name, "Take");
    let mut pitches: Vec<u8> = take.notes.values().map(|n| n.pitch).collect();
    pitches.sort();
    assert_eq!(
        pitches,
        [60, 64],
        "the note still held when playback stopped is in it too"
    );
    h.key_up(Key::D);
    undo(&mut h);
    assert!(clips(&h).is_empty(), "the take is one undo step");
}

#[test]
fn a_plugin_is_added_from_the_device_tab_and_its_settings_shown() {
    let mut session = Session::starter(Engine::manual(48_000, false));
    session.enable_plugins(mdaw_plugin::Host::test_synth_only());
    let app = DawApp::new(session);
    let mut h = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(1.0 / 60.0)
        .with_context_setup(DawApp::setup)
        .build_ui_state(|ui, app: &mut DawApp| app.ui(ui), app);
    h.run_steps(3);
    let id = track(&h);

    h.get_by_label("Device").click();
    h.run_steps(2);
    let add = h.get_by_label("Instrument…").rect().center();
    click(&mut h, add);
    open_submenu(&mut h, "CLAP (1)");
    let synth = h.get_by_label_contains("mdaw Test Synth").rect().center();
    click(&mut h, synth);
    h.run_steps(4);
    let devices = |h: &App| {
        h.state()
            .session
            .project()
            .track(id)
            .unwrap()
            .content
            .devices
            .clone()
    };
    assert_eq!(devices(&h).len(), 1, "one instrument: the synth replaced Pocket");
    assert!(
        matches!(&devices(&h)[0].kind, mdaw_model::DeviceKind::Plugin { id, .. } if id == mdaw_plugin::TEST_SYNTH),
        "a plugin device"
    );
    // The plugin is simply there, and runs.
    assert!(
        h.query_all_by_label_contains("Balance").count() > 0,
        "its settings are shown"
    );
    assert!(
        h.query_by_label("Open window").is_none(),
        "the test synth has no window of its own"
    );
    // It is a device like the others: undo takes it away again, and Pocket is back.
    undo(&mut h);
    assert_eq!(h.query_all_by_label_contains("Balance").count(), 0);
    assert!(matches!(
        devices(&h)[0].kind,
        mdaw_model::DeviceKind::Builtin { .. }
    ));

    // Pocket can be picked again from the same menu.
    for choice in ["mdaw Test Synth", "Pocket (built in)"] {
        let add = h.get_by_label("Instrument…").rect().center();
        click(&mut h, add);
        if choice != "Pocket (built in)" {
            open_submenu(&mut h, "CLAP (1)");
        }
        let item = h.get_by_label_contains(choice).rect().center();
        click(&mut h, item);
        h.run_steps(4);
    }
    assert_eq!(devices(&h).len(), 1);
    assert!(matches!(
        devices(&h)[0].kind,
        mdaw_model::DeviceKind::Builtin { .. }
    ));
}
