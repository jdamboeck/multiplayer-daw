//! Two whole client windows against a real server in one process: pointer events in, the other
//! window's document and labels out.

use egui::{Pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use mdaw_client::{DawApp, Session, Status, Who};
use mdaw_engine::Engine;
use mdaw_model::{ClipId, ClipProp, Device, DeviceKind, EntityRef, Note, NoteProp, Op, Tick, TrackId};
use mdaw_server::{Config, Server};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

type App<'a> = Harness<'a, DawApp>;

fn serve(dir: &Path, listen: &str) -> Server {
    let config = Config {
        data_dir: dir.to_path_buf(),
        listen: listen.parse().unwrap(),
        host: None,
    };
    Server::start(config).unwrap()
}

fn window<'a>(server: &Server, name: &str) -> App<'a> {
    window_with(server, name, Engine::manual(48_000, false))
}

/// A window whose engine keeps time like one with an audio device.
fn live_window<'a>(server: &Server, name: &str) -> App<'a> {
    window_with(server, name, Engine::manual(48_000, true))
}

fn window_with<'a>(server: &Server, name: &str, engine: Engine) -> App<'a> {
    let mut user_key = [0u8; 16];
    user_key[..name.len()].copy_from_slice(name.as_bytes());
    let who = Who {
        user_key,
        name: name.into(),
    };
    let session = Session::join(server.join_info().clone(), who, engine, Arc::new(|| {})).unwrap();
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(1.0 / 60.0)
        .with_context_setup(DawApp::setup)
        .build_ui_state(|ui, app: &mut DawApp| app.ui(ui), DawApp::new(session));
    harness.run_steps(3);
    harness
}

/// Runs frames in both windows until `done`.
fn until(a: &mut App, b: &mut App, what: &str, done: impl Fn(&DawApp, &DawApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        a.step();
        b.step();
        if done(a.state(), b.state()) {
            // Once more, so that what the windows show is what `done` saw.
            a.step();
            b.step();
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Everything sent has been confirmed and both windows hold the same document.
fn settle(a: &mut App, b: &mut App) {
    until(a, b, "the windows agree", |a, b| {
        let (a, b) = (&a.session, &b.session);
        a.unconfirmed() == 0
            && b.unconfirmed() == 0
            && a.seq() == b.seq()
            && a.status() == Status::Online
            && b.status() == Status::Online
            && a.project() == b.project()
    });
}

fn lane(h: &App, row: usize, tick: Tick) -> Pos2 {
    h.state()
        .arrange
        .layout
        .expect("the arrangement is laid out")
        .pos(row, tick)
        + vec2(3.0, 0.0)
}

fn press(h: &mut App, pos: Pos2) {
    h.hover_at(pos);
    h.step();
    h.drag_at(pos);
    h.step();
}

fn pull(h: &mut App, from: Pos2, to: Pos2) {
    for step in 1..=6 {
        h.hover_at(from + (to - from) * (step as f32 / 6.0));
        h.step();
    }
}

fn drag(h: &mut App, from: Pos2, to: Pos2) {
    press(h, from);
    pull(h, from, to);
    h.drop_at(to);
    h.run_steps(2);
}

fn shows(h: &App, text: &str) -> bool {
    h.query_all_by_label_contains(text).next().is_some()
}

/// Ann adds a track and draws a two-bar clip at the start; both windows end up with it.
fn with_a_clip<'a>(server: &Server) -> (App<'a>, App<'a>, TrackId, ClipId) {
    let (mut ann, mut ben) = (window(server, "ann"), window(server, "ben"));
    ann.get_by_label("+ Track").click();
    ann.run_steps(3);
    let (from, to) = (
        lane(&ann, 0, Tick::ZERO),
        lane(&ann, 0, Tick::bars(2)) - vec2(8.0, 0.0),
    );
    drag(&mut ann, from, to);
    settle(&mut ann, &mut ben);
    let project = ben.state().session.project().clone();
    let (track, content) = project
        .ordered_tracks()
        .next()
        .map(|(id, t)| (id, t.content.clone()))
        .unwrap();
    let clip = *content.clips.keys().next().expect("ben has the clip ann drew");
    (ann, ben, track, clip)
}

fn clip_start(h: &App, track: TrackId, clip: ClipId) -> Tick {
    h.state().session.project().track(track).unwrap().content.clips[&clip].start
}

#[test]
fn two_windows_share_a_project_and_survive_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let addr = server.local_addr().unwrap();
    let (mut ann, mut ben, track, clip) = with_a_clip(&server);
    assert!(
        shows(&ann, "ann (you)") && shows(&ann, "ben"),
        "ann sees who is here"
    );
    assert!(
        shows(&ben, "ben (you)") && shows(&ben, "ann"),
        "ben sees who is here"
    );

    // Ben moves the clip ann drew.
    let (from, to) = (
        lane(&ben, 0, Tick::beats(2)),
        lane(&ben, 0, Tick::bars(1) + Tick::beats(2)),
    );
    drag(&mut ben, from, to);
    settle(&mut ann, &mut ben);
    assert_eq!(clip_start(&ann, track, clip), Tick::bars(1));

    server.stop();
    until(&mut ann, &mut ben, "both notice the server is gone", |a, b| {
        a.session.status() == Status::Reconnecting && b.session.status() == Status::Reconnecting
    });
    assert!(shows(&ann, "Reconnecting"));

    // Work goes on, and the window says that it has not arrived yet.
    let (from, to) = (
        lane(&ann, 0, Tick::bars(1) + Tick::beats(2)),
        lane(&ann, 0, Tick::bars(3) + Tick::beats(2)),
    );
    drag(&mut ann, from, to);
    assert_eq!(clip_start(&ann, track, clip), Tick::bars(3));
    assert!(shows(&ann, "1 edit waiting"));

    let _server = serve(dir.path(), &addr.to_string());
    settle(&mut ann, &mut ben);
    assert_eq!(clip_start(&ben, track, clip), Tick::bars(3));
    assert!(shows(&ann, "Online") && !shows(&ann, "waiting"));
}

#[test]
fn a_clip_someone_else_drags_cannot_be_dragged() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben, track, clip) = with_a_clip(&server);
    let the_clip = EntityRef::Clip(track, clip);

    // Ann picks the clip up and holds it.
    let (from, to) = (
        lane(&ann, 0, Tick::beats(2)),
        lane(&ann, 0, Tick::bars(2) + Tick::beats(2)),
    );
    press(&mut ann, from);
    pull(&mut ann, from, to);
    until(&mut ann, &mut ben, "ben sees ann holding the clip", |_, b| {
        b.session.locked_by(&the_clip).is_some()
    });
    assert_eq!(ben.state().session.locked_by(&the_clip), Some("ann"));

    // Ben's drag does nothing while she does.
    let (ben_from, ben_to) = (
        lane(&ben, 0, Tick::beats(1)),
        lane(&ben, 0, Tick::bars(5) + Tick::beats(1)),
    );
    drag(&mut ben, ben_from, ben_to);
    assert_eq!(clip_start(&ben, track, clip), Tick::ZERO);
    assert_eq!(ben.state().session.unconfirmed(), 0, "ben sent nothing");

    // Ann lets go: her move lands, and the clip is free again.
    ann.drop_at(to);
    ann.run_steps(2);
    settle(&mut ann, &mut ben);
    assert_eq!(clip_start(&ben, track, clip), Tick::bars(2));
    until(&mut ann, &mut ben, "the clip is free", |_, b| {
        b.session.locked_by(&the_clip).is_none()
    });

    let (ben_from, ben_to) = (
        lane(&ben, 0, Tick::bars(2) + Tick::beats(1)),
        lane(&ben, 0, Tick::bars(5) + Tick::beats(1)),
    );
    drag(&mut ben, ben_from, ben_to);
    settle(&mut ann, &mut ben);
    assert_eq!(clip_start(&ann, track, clip), Tick::bars(5));
}

#[test]
fn a_lost_edit_is_listed_and_comes_back_with_one_click() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben, track, clip) = with_a_clip(&server);
    let id = ann.state_mut().session.ids().note();
    let note = Note {
        start: Tick::ZERO,
        dur: Tick::beats(1),
        pitch: 60,
        vel: 100,
    };
    assert!(ann.state_mut().session.edit(vec![Op::NoteAdd {
        track,
        clip,
        id,
        note
    }]));
    settle(&mut ann, &mut ben);
    let pitch =
        |h: &App| h.state().session.project().track(track).unwrap().content.clips[&clip].notes[&id].pitch;
    let set = |h: &mut App, pitch: u8| {
        let op = Op::NoteSet {
            track,
            clip,
            id,
            prop: NoteProp::Pitch(pitch),
        };
        assert!(h.state_mut().session.edit(vec![op]));
    };

    // Both change the note before either hears of the other. Ann's change arrives first, so
    // ben's is the last one and wins.
    set(&mut ann, 62);
    let deadline = Instant::now() + Duration::from_secs(20);
    while ann.state().session.unconfirmed() > 0 {
        assert!(Instant::now() < deadline, "ann's edit was never confirmed");
        ann.step();
        std::thread::sleep(Duration::from_millis(2));
    }
    set(&mut ben, 65);
    settle(&mut ann, &mut ben);
    assert_eq!(pitch(&ann), 65);

    // Both changelogs say what happened, each from its own point of view.
    // The changelog follows the selected track, which is the only one.
    for h in [&mut ann, &mut ben] {
        h.get_by_label("Changelog").click();
        h.run_steps(3);
    }
    assert!(shows(
        &ann,
        "you: pitch D4 for a note on \"Track 1\", overwritten by ben"
    ));
    assert!(shows(
        &ben,
        "ann: pitch D4 for a note on \"Track 1\", overwritten by you"
    ));

    // A new track has nothing in it yet; the whole project still has the line.
    ben.get_by_label("+ Track").click();
    ben.run_steps(3);
    assert!(shows(&ben, "Nothing has happened to \"Track 2\" yet"));
    ben.get_by_label("Whole project").click();
    ben.run_steps(3);
    assert!(shows(&ben, "overwritten by you"));

    // Ben gives ann her note back.
    ben.get_by_label("Restore").click();
    ben.run_steps(3);
    settle(&mut ann, &mut ben);
    assert_eq!(pitch(&ann), 62);
    let lost = ann.state().session.displaced()[0].lost.clone();
    assert!(
        !ann.state().session.can_restore(&lost),
        "it is back, so there is nothing to restore"
    );
}

#[test]
fn a_refused_edit_shows_a_notice_that_can_be_dismissed() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben, track, clip) = with_a_clip(&server);

    // Ann moves the clip and, in the same edit, adds a plugin whose settings the server never
    // got: the server turns the whole edit down.
    let plugin = Device::new(
        ann.state_mut().session.ids().device(),
        DeviceKind::Plugin {
            format: mdaw_model::PluginFormat::Clap,
            id: "org.example.synth".into(),
            state: Some(mdaw_proto::blob_hash(b"never uploaded")),
        },
    );
    assert!(ann.state_mut().session.edit(vec![
        Op::ClipSet {
            track,
            id: clip,
            prop: ClipProp::Start(Tick::bars(1)),
        },
        Op::DeviceAdd {
            track,
            device: plugin,
            after: None,
        },
    ]));
    settle(&mut ann, &mut ben);
    assert!(shows(&ann, "An edit was not accepted"));
    assert!(
        !shows(&ben, "An edit was not accepted"),
        "it is nobody else's business"
    );

    ann.get_by_label("Dismiss").click();
    ann.run_steps(3);
    assert!(!shows(&ann, "An edit was not accepted"));
}

#[test]
fn play_in_one_window_plays_there_only() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben) = (live_window(&server, "ann"), live_window(&server, "ben"));
    until(&mut ann, &mut ben, "both are online", |a, b| {
        a.session.peers().len() == 2 && b.session.peers().len() == 2
    });
    assert!(!shows(&ann, "Link"), "there is nothing to link to");

    ann.get_by_label("Play").click();
    until(&mut ann, &mut ben, "it plays in ann's window", |a, _| {
        a.session.position() > 100.0
    });
    assert!(shows(&ann, "Pause"));
    assert!(shows(&ben, "Play"), "ben's transport is his own");
    assert!(!ben.state().session.is_playing());
    assert_eq!(ben.state().session.position(), 0.0);

    // The loop is each window's own too.
    ann.get_by_label("Loop").click();
    ann.run_steps(2);
    ben.run_steps(2);
    assert!(ann.state().loop_on && !ben.state().loop_on);
    assert_eq!(ann.state().session.loop_range(), Some(ann.state().loop_range));
    assert_eq!(ben.state().session.loop_range(), None);

    // Ben plays and stops on his own; ann plays on.
    ben.get_by_label("Play").click();
    until(&mut ann, &mut ben, "it plays in ben's window", |_, b| {
        b.session.position() > 100.0
    });
    ben.get_by_label("Pause").click();
    ben.run_steps(2);
    assert!(!ben.state().session.is_playing());
    assert!(ann.state().session.is_playing());
    assert_eq!(ann.state().session.seq(), 0, "nothing of it is in the log");
}

/// Writes PNGs of what collaboration looks like. Run with
/// `MDAW_SHOTS=/some/dir cargo test -p mdaw-client --test ui_net -- --ignored screenshots`.
#[test]
#[ignore = "needs a GPU adapter; for looking at the UI"]
fn screenshots() {
    let Ok(shots) = std::env::var("MDAW_SHOTS") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben, track, clip) = with_a_clip(&server);
    let the_clip = EntityRef::Clip(track, clip);

    // A lost edit and a refused one, for the changelog and the notice strip.
    let id = ann.state_mut().session.ids().note();
    let note = Note {
        start: Tick::ZERO,
        dur: Tick::beats(1),
        pitch: 60,
        vel: 100,
    };
    ann.state_mut().session.edit(vec![Op::NoteAdd {
        track,
        clip,
        id,
        note,
    }]);
    settle(&mut ann, &mut ben);
    for (h, pitch) in [(&mut ann, 62), (&mut ben, 65)] {
        let op = Op::NoteSet {
            track,
            clip,
            id,
            prop: NoteProp::Pitch(pitch),
        };
        h.state_mut().session.edit(vec![op]);
    }
    let id = ben.state_mut().session.ids().device();
    let device = Device::new(
        id,
        DeviceKind::Plugin {
            format: mdaw_model::PluginFormat::Clap,
            id: "org.example.synth".into(),
            state: None,
        },
    );
    ben.state_mut().session.edit(vec![Op::DeviceAdd {
        track,
        device,
        after: None,
    }]);
    settle(&mut ann, &mut ben);

    // Ann holds the clip while ben looks at the changelog.
    let (from, to) = (
        lane(&ann, 0, Tick::beats(2)),
        lane(&ann, 0, Tick::bars(2) + Tick::beats(2)),
    );
    press(&mut ann, from);
    pull(&mut ann, from, to);
    until(&mut ann, &mut ben, "ben sees ann holding the clip", |_, b| {
        b.session.locked_by(&the_clip).is_some()
    });
    ben.get_by_label("Changelog").click();
    ben.run_steps(2);
    ben.get_by_label("Whole project").click();
    ben.run_steps(3);
    ben.render()
        .expect("render")
        .save(format!("{shots}/collab-ben.png"))
        .expect("save");
    ann.render()
        .expect("render")
        .save(format!("{shots}/collab-ann.png"))
        .expect("save");

    ann.drop_at(to);
    ann.run_steps(2);
    settle(&mut ann, &mut ben);
    ann.get_by_label("Changelog").click();
    ann.run_steps(3);
    ann.render()
        .expect("render")
        .save(format!("{shots}/changelog-ann.png"))
        .expect("save");
}

#[test]
fn a_plugin_added_in_the_device_tab_reaches_the_other_window() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben, track, _) = with_a_clip(&server);
    ann.state_mut()
        .session
        .enable_plugins(mdaw_plugin::Host::test_synth_only());
    ann.state_mut().sel.track = Some(track);
    ann.get_by_label("Device").click();
    ann.run_steps(3);
    let has_plugin = |app: &DawApp| {
        let content = &app.session.project().track(track).unwrap().content;
        content
            .devices
            .iter()
            .any(|d| matches!(d.kind, DeviceKind::Plugin { .. }))
    };

    let add = ann.get_by_label("Instrument…").rect().center();
    press(&mut ann, add);
    ann.drop_at(add);
    ann.run_steps(2);
    let synth = ann.get_by_label_contains("mdaw Test Synth").rect().center();
    press(&mut ann, synth);
    ann.drop_at(synth);
    ann.run_steps(4);
    assert!(has_plugin(ann.state()), "the plugin is on ann's track");
    settle(&mut ann, &mut ben);
    assert!(has_plugin(ben.state()), "and on ben's, an ordinary edit");

    // Ben has no plugins: the track says so where he sees it.
    ben.state_mut().sel.track = Some(track);
    ben.get_by_label("Device").click();
    ben.run_steps(3);
    assert_eq!(
        ben.state().session.track_sound(track),
        mdaw_client::plugins::TrackSound::NotInstalled,
        "the track header says \"plugin not installed\""
    );
    assert!(shows(&ben, "is not installed here"));
}
