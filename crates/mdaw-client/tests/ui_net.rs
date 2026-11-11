//! Two whole client windows against a real server in one process: pointer events in, the other
//! window's document and labels out.

use egui::{Pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use mdaw_client::{DawApp, Session, Status, Who};
use mdaw_engine::Engine;
use mdaw_model::{ClipId, Device, DeviceKind, EntityRef, Note, NoteProp, Op, Tick, TrackId};
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
    let (mut ann, mut ben, track, _) = with_a_clip(&server);

    // Plugins enter a project only through a commit, which this is not.
    let id = ann.state_mut().session.ids().device();
    let device = Device::new(
        id,
        DeviceKind::Clap {
            plugin_id: "org.example.synth".into(),
            state: None,
        },
    );
    assert!(ann.state_mut().session.edit(vec![Op::DeviceAdd {
        track,
        device,
        after: None
    }]));
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
fn a_track_is_checked_out_committed_and_heard_through_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben, track, clip) = with_a_clip(&server);

    ann.get_by_label("Check out").click();
    until(&mut ann, &mut ben, "ben sees the lock", |a, b| {
        a.session.workspace(track).is_some() && b.session.checkout_owner(track).is_some()
    });
    assert!(shows(&ben, "Locked"));
    assert!(shows(&ann, "Commit…"));

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
    ann.get_by_label("Commit…").click();
    until(&mut ann, &mut ben, "the samples are rendered", |a, _| {
        matches!(
            a.session.commit_state(),
            Some(mdaw_client::CommitState::Ready { .. })
        )
    });
    assert!(shows(&ann, "What did you change?"));
    assert!(shows(&ann, "Track alone"));
    ann.get_by_label("Commit").click();
    until(&mut ann, &mut ben, "ben has the note", |_, b| {
        b.session.project().track(track).unwrap().content.clips[&clip]
            .notes
            .len()
            == 1
    });
    settle(&mut ann, &mut ben);
    assert!(!shows(&ann, "What did you change?"), "the dialog is gone");
    assert!(shows(&ben, "Check out"), "the track is free again");

    ben.get_by_label("Changelog").click();
    ben.run_steps(2);
    ben.get_by_label("Whole project").click();
    ben.run_steps(3);
    assert!(shows(&ben, "ann committed \"Track 1\""));
    ben.get_by_label_contains("Track alone").click();
    until(&mut ann, &mut ben, "the sample plays", |_, b| {
        b.session.project().tracks.len() == 1
            && b.session.log().iter().any(|e| {
                b.session
                    .samples(e.seq)
                    .iter()
                    .any(|s| b.session.sample_position(s.blob).is_some())
            })
    });
}

#[test]
fn ending_someone_elses_checkout_asks_first_and_leaves_them_a_fork() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben, track, clip) = with_a_clip(&server);
    ann.get_by_label("Check out").click();
    until(&mut ann, &mut ben, "ben sees the lock", |a, b| {
        a.session.workspace(track).is_some() && b.session.checkout_owner(track).is_some()
    });
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

    ben.get_by_label("Locked").click();
    ben.run_steps(3);
    ben.get_by_label("Cancel").click();
    ben.run_steps(3);
    assert!(
        ben.state().session.checkout_owner(track).is_some(),
        "nothing happened"
    );
    ben.get_by_label("Locked").click();
    ben.run_steps(3);
    ben.get_by_label("End the checkout").click();
    until(&mut ann, &mut ben, "the checkout is over", |a, b| {
        a.session.workspace(track).is_none() && b.session.checkout_owner(track).is_none()
    });
    assert!(shows(&ann, "ben ended your checkout"));
    ann.get_by_label("Changelog").click();
    ann.run_steps(3);
    ann.get_by_label("Restore as new track").click();
    ann.run_steps(3);
    settle(&mut ann, &mut ben);
    assert_eq!(ben.state().session.project().tracks.len(), 2);
}

#[test]
fn play_in_one_window_plays_in_the_other_until_it_unlinks() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path(), "127.0.0.1:0");
    let (mut ann, mut ben) = (live_window(&server, "ann"), live_window(&server, "ben"));
    until(&mut ann, &mut ben, "both know the server's clock", |a, b| {
        a.session.timing().is_some() && b.session.timing().is_some()
    });
    assert!(shows(&ben, "clock ±"));

    ann.get_by_label("Play").click();
    until(&mut ann, &mut ben, "it plays in both", |a, b| {
        a.session.position() > 0.0 && b.session.position() > 0.0
    });
    assert!(shows(&ben, "Pause"), "ben's button is the one that stops it");
    assert!(shows(&ben, "in sync"));

    // The loop is part of what is shared.
    ann.get_by_label("Loop").click();
    until(&mut ann, &mut ben, "ben loops too", |_, b| b.loop_on);
    assert_eq!(ben.state().loop_range, ann.state().loop_range);

    // Ben unlinks: what ann does no longer reaches ben's transport.
    ben.get_by_label("Link").click();
    ben.run_steps(2);
    assert_eq!(ben.state().session.linked(), Some(false));
    ann.get_by_label("Pause").click();
    until(&mut ann, &mut ben, "ann has stopped and ben knows", |a, b| {
        !a.session.is_playing() && b.session.shared_transport().is_some_and(|t| !t.playing)
    });
    assert!(ben.state().session.is_playing());
    assert!(shows(&ben, "Pause") && !shows(&ben, "in sync"));

    // Linked again, ben is where everyone is: stopped, at ann's playhead.
    ben.get_by_label("Link").click();
    until(&mut ann, &mut ben, "ben is back with ann", |a, b| {
        !b.session.is_playing() && a.session.position() == b.session.position()
    });
    assert!(shows(&ben, "Play"));
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
        DeviceKind::Clap {
            plugin_id: "org.example.synth".into(),
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

    // Ann checks the track out and opens the commit dialog; ben sees the lock.
    ann.drop_at(to);
    ann.run_steps(2);
    ann.get_by_label("Check out").click();
    until(&mut ann, &mut ben, "ben sees the lock", |a, b| {
        a.session.workspace(track).is_some() && b.session.checkout_owner(track).is_some()
    });
    ann.get_by_label("Commit…").click();
    until(&mut ann, &mut ben, "the samples are rendered", |a, _| {
        matches!(
            a.session.commit_state(),
            Some(mdaw_client::CommitState::Ready { .. })
        )
    });
    for (h, name) in [(&mut ann, "checkout-ann"), (&mut ben, "checkout-ben")] {
        h.render()
            .expect("render")
            .save(format!("{shots}/{name}.png"))
            .expect("save");
    }

    // Ann commits and reads the changelog of the track she has selected.
    ann.get_by_label("Commit").click();
    until(&mut ann, &mut ben, "the commit lands", |_, b| {
        b.session.checkout_owner(track).is_none()
    });
    settle(&mut ann, &mut ben);
    ann.get_by_label("Changelog").click();
    ann.run_steps(3);
    ann.render()
        .expect("render")
        .save(format!("{shots}/changelog-ann.png"))
        .expect("save");
}
