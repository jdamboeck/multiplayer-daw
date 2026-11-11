//! The M6 demo as a test: a plugin goes onto a track through a checkout and is committed with
//! a stem. A client with the plugin plays it live from the committed state; one without plays
//! the stem. The plugin is the built-in test synth, hosted through CLAP like any other.

use mdaw_client::plugins::TrackSound;
use mdaw_client::{CommitState, Session, Status, Who};
use mdaw_dsp::pocket;
use mdaw_engine::Engine;
use mdaw_model::*;
use mdaw_plugin::{Host, TEST_SYNTH};
use mdaw_server::{Config, Server};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn serve() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        listen: "127.0.0.1:0".parse().unwrap(),
        host: None,
    };
    (dir, Server::start(config).unwrap())
}

/// A client; `plugins` says whether it hosts plugins (the test synth only).
fn join(server: &Server, name: &str, plugins: bool) -> Session {
    let mut user_key = [0u8; 16];
    user_key[..name.len()].copy_from_slice(name.as_bytes());
    let who = Who {
        user_key,
        name: name.into(),
    };
    let engine = Engine::manual(48_000, false);
    let mut session = Session::join(server.join_info().clone(), who, engine, Arc::new(|| {})).unwrap();
    if plugins {
        session.enable_plugins(Host::test_synth_only());
    }
    session
}

fn until(sessions: &mut [&mut Session], what: &str, done: impl Fn(&mut [&mut Session]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for session in sessions.iter_mut() {
            session.frame();
        }
        if done(sessions) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn settle(sessions: &mut [&mut Session]) {
    until(sessions, "the clients agree", |sessions| {
        let head = sessions.iter().map(|s| s.seq()).max().unwrap();
        sessions
            .iter()
            .all(|s| s.unconfirmed() == 0 && s.seq() == head && s.status() == Status::Online)
    });
}

/// A track with Pocket and a one-bar clip of four notes.
fn add_track(session: &mut Session) -> (TrackId, DeviceId) {
    let ids = session.ids();
    let (track, clip, pocket_id) = (ids.track(), ids.clip(), ids.device());
    let mut content = TrackContent::default();
    let kind = DeviceKind::Builtin {
        kind: BuiltinKind::Pocket,
        dsp_version: pocket::DSP_VERSION,
    };
    content.devices.push_back(Device::new(pocket_id, kind));
    let notes = (0..4)
        .map(|beat| {
            let note = Note {
                start: Tick::beats(beat),
                dur: Tick::beats(1),
                pitch: 60 + beat as u8 * 2,
                vel: 100,
            };
            (session.ids().note(), note)
        })
        .collect();
    assert!(session.edit(vec![
        Op::TrackAdd {
            id: track,
            shell: TrackShell {
                name: "lead".into(),
                color: 0,
            },
            content,
            after: None,
        },
        Op::ClipAdd {
            track,
            id: clip,
            clip: Clip {
                start: Tick::ZERO,
                length: Tick::bars(1),
                name: "riff".into(),
                notes,
            },
        },
    ]));
    (track, pocket_id)
}

/// Plays the first bar on this client alone and returns the peak of each channel.
fn listen(session: &mut Session) -> [f32; 2] {
    session.set_linked(false);
    session.seek(Tick::ZERO);
    session.play();
    session.frame();
    let mut peak = [0.0f32; 2];
    // One bar at 120 bpm is two seconds.
    for _ in 0..(2 * 48_000 / 1024) {
        let audio = session.engine_mut().pump(1024).expect("a manual engine");
        for frame in audio.as_chunks::<2>().0 {
            peak[0] = peak[0].max(frame[0].abs());
            peak[1] = peak[1].max(frame[1].abs());
        }
    }
    session.pause();
    session.frame();
    peak
}

fn plugin_device(session: &Session, track: TrackId) -> DeviceId {
    let content = &session.project().track(track).unwrap().content;
    content
        .devices
        .iter()
        .find(|d| matches!(d.kind, DeviceKind::Plugin { .. }))
        .expect("a plugin device")
        .id
}

#[test]
fn a_plugin_is_heard_live_where_it_is_installed_and_from_its_stem_elsewhere() {
    let (_dir, server) = serve();
    let mut ann = join(&server, "ann", true);
    let mut ben = join(&server, "ben", true);
    let mut cat = join(&server, "cat", false);
    let (track, pocket_id) = add_track(&mut ann);
    settle(&mut [&mut ann, &mut ben, &mut cat]);
    let synth = ann
        .plugin_catalog()
        .into_iter()
        .find(|p| p.id == TEST_SYNTH)
        .expect("the test synth is installed");

    // Plugins go onto a track only through a checkout.
    assert!(!ann.add_plugin(track, &synth));
    assert!(ann.notices().iter().any(|n| n.contains("Check out")));

    ann.check_out(track);
    until(&mut [&mut ann, &mut ben, &mut cat], "the checkout lands", |s| {
        s.iter().all(|s| s.checkout_owner(track).is_some()) && s[0].workspace(track).is_some()
    });
    assert!(ann.add_plugin(track, &synth));
    // A track has one instrument: the synth takes Pocket's place.
    let devices = &ann.project().track(track).unwrap().content.devices;
    assert_eq!(devices.len(), 1);
    assert!(devices.iter().all(|d| d.id != pocket_id));
    until(&mut [&mut ann], "the plugin runs", |s| {
        s[0].track_sound(track) == TrackSound::Live
    });
    // Plugins run on this thread: it does not go to sleep while one runs, so that its timers and
    // the callbacks it asks for are served even when nothing changes on screen.
    assert!(
        ann.next_frame_in()
            .is_some_and(|wait| wait <= Duration::from_millis(50)),
        "{:?}",
        ann.next_frame_in()
    );
    assert!(
        cat.next_frame_in()
            .is_none_or(|wait| wait > Duration::from_millis(50))
    );
    let device = plugin_device(&ann, track);
    let (params, _) = ann.plugin_params(track, device, usize::MAX).unwrap();
    assert_eq!(
        params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["Level", "Balance", "Misbehave on lowest note"]
    );
    // All to the right: a setting that only the plugin's state carries.
    ann.set_plugin_param(track, device, mdaw_test_plugin::BALANCE, 1.0);
    // A fader and a master away from unity: they apply where the track plays, so the stem is
    // rendered without them, or they would count twice.
    assert!(ann.edit(vec![Op::TrackSetMix {
        track,
        prop: MixProp::Gain(0.5),
    }]));
    assert!(ann.edit(vec![Op::MasterSetGain { gain: 1.5 }]));
    let [left, right] = listen(&mut ann);
    assert!(
        right > 0.02 && left < 1e-4,
        "ann hears the plugin: {left} {right}"
    );
    // The others hear the track as it was, from Pocket.
    assert_eq!(ben.track_sound(track), TrackSound::Builtin);

    assert!(ann.start_commit(track));
    until(&mut [&mut ann, &mut ben, &mut cat], "the stem is rendered", |s| {
        matches!(s[0].commit_state(), Some(CommitState::Ready { .. }))
    });
    ann.finish_commit("the test synth, all right", false);
    until(&mut [&mut ann, &mut ben, &mut cat], "the commit lands", |s| {
        s.iter().all(|s| s.checkout_owner(track).is_none())
    });
    settle(&mut [&mut ann, &mut ben, &mut cat]);
    let committed = ben.project().track(track).unwrap().clone();
    let stem = committed.stem.expect("the commit carries a stem");
    assert_eq!(stem.tempo_hash, ben.project().tempo_map().hash());
    assert!(matches!(
        committed.content.device(device).unwrap().kind,
        DeviceKind::Plugin { state: Some(_), .. }
    ));
    // Ann's own instance carries on with what she set.
    assert_eq!(ann.track_sound(track), TrackSound::Live);

    // Ben has the plugin: it runs on his machine, from the committed state.
    until(&mut [&mut ann, &mut ben, &mut cat], "ben runs the plugin", |s| {
        s[1].track_sound(track) == TrackSound::Live
    });
    let balance = ben.plugin_params(track, device, usize::MAX).unwrap().0[1].value;
    assert_eq!(balance, 1.0, "the state came with the commit");
    let [left, right] = listen(&mut ben);
    assert!(
        right > 0.02 && left < 1e-4,
        "ben hears the plugin: {left} {right}"
    );
    let ben_heard = right;

    // Cat has no plugins: she hears the stem, which sounds the same.
    until(&mut [&mut ann, &mut ben, &mut cat], "cat has the stem", |s| {
        s[2].track_sound(track) == TrackSound::Stem { stale: false }
    });
    assert!(!cat.plugin_running(track, device));
    let [left, right] = listen(&mut cat);
    assert!(right > 0.02 && left < 1e-3, "cat hears the stem: {left} {right}");
    // As loud as the plugin itself, through the same fader and master.
    let db = 20.0 * (right / ben_heard).log10();
    assert!(db.abs() < 0.5, "the stem is {db:+.1} dB against the plugin");

    // A track with plugins is changed only through a checkout.
    let clip = *ben
        .project()
        .track(track)
        .unwrap()
        .content
        .clips
        .keys()
        .next()
        .unwrap();
    let note = Op::NoteAdd {
        track,
        clip,
        id: ben.ids().note(),
        note: Note {
            start: Tick::beats(2),
            dur: Tick::beats(1),
            pitch: 72,
            vel: 90,
        },
    };
    assert!(!ben.edit(vec![note]));
    assert!(ben.notices().iter().any(|n| n.contains("checkout")));

    // Another tempo: the stem no longer fits, and says so; it still plays.
    let id = *ben.project().tempo.keys().next().unwrap();
    let point = TempoPoint {
        tick: Tick::ZERO,
        milli_bpm: 90_000,
    };
    assert!(ben.edit(vec![Op::TempoPut { id, point }]));
    settle(&mut [&mut ann, &mut ben, &mut cat]);
    assert_eq!(cat.track_sound(track), TrackSound::Stem { stale: true });
    assert_eq!(ben.track_sound(track), TrackSound::Live);
}

/// The helper that runs plugins in processes of their own, built if it is not there yet.
#[cfg(target_os = "linux")]
fn helper() -> std::path::PathBuf {
    if let Some(helper) = mdaw_plugin::isolated::helper() {
        return helper;
    }
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "-p", "mdaw-plugin", "--bin", "mdaw-plugin-host"])
        .status()
        .unwrap();
    assert!(status.success());
    mdaw_plugin::isolated::helper().expect("the helper was just built")
}

#[cfg(target_os = "linux")]
#[test]
fn a_plugin_that_crashes_leaves_the_session_running_and_can_be_restarted() {
    let mut session = Session::starter(Engine::manual(48_000, false));
    session.enable_plugins(Host::isolated_test_synth(helper()));
    let track = session.project().ordered_tracks().next().unwrap().0;
    let synth = session
        .plugin_catalog()
        .into_iter()
        .find(|p| p.id == TEST_SYNTH)
        .unwrap();
    assert!(session.add_plugin(track, &synth));
    until(&mut [&mut session], "the plugin runs", |s| {
        s[0].track_sound(track) == TrackSound::Live
    });
    let device = plugin_device(&session, track);
    // A setting the document does not have: it is kept aside, for after a crash.
    session.set_plugin_param(track, device, mdaw_test_plugin::BALANCE, 1.0);
    session.engine_mut().pump(1024);
    let kept = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < kept {
        session.frame();
        std::thread::sleep(Duration::from_millis(10));
    }

    // Told to, the plugin crashes on this note.
    session.set_plugin_param(
        track,
        device,
        mdaw_test_plugin::MISBEHAVE,
        mdaw_test_plugin::CRASH,
    );
    session.engine_mut().pump(1024);
    session.live().set_target(Some(track));
    session.live().press(mdaw_test_plugin::CRASH_PITCH, 100);
    let started = Instant::now();
    for _ in 0..20 {
        session.engine_mut().pump(1024).unwrap();
    }
    assert!(started.elapsed() < Duration::from_millis(500), "audio goes on");
    until(&mut [&mut session], "the crash is noticed", |s| {
        s[0].plugin_crashed(track, device)
    });
    assert_eq!(session.track_sound(track), TrackSound::Crashed { stem: false });
    assert!(
        session
            .notices()
            .iter()
            .any(|n| n.contains("mdaw Test Synth crashed"))
    );
    // Everything else goes on: editing, for one.
    assert!(session.edit(vec![Op::TrackSetShell {
        id: track,
        prop: ShellProp::Name("after the crash".into()),
    }]));

    session.restart_plugin(track, device);
    until(&mut [&mut session], "the plugin runs again", |s| {
        s[0].track_sound(track) == TrackSound::Live
    });
    let (params, _) = session.plugin_params(track, device, usize::MAX).unwrap();
    assert_eq!(params[1].value, 1.0, "with the setting it had");
}

/// Ann's track with the test synth in place of Pocket, checked out by her.
fn synth_track(ann: &mut Session, others: &mut [&mut Session]) -> (TrackId, DeviceId) {
    let (track, _) = add_track(ann);
    let mut all: Vec<&mut Session> = vec![&mut *ann];
    all.extend(others.iter_mut().map(|s| &mut **s));
    settle(&mut all);
    ann.check_out(track);
    until(&mut [&mut *ann], "the checkout lands", |s| {
        s[0].workspace(track).is_some()
    });
    let synth = ann
        .plugin_catalog()
        .into_iter()
        .find(|p| p.id == TEST_SYNTH)
        .unwrap();
    assert!(ann.add_plugin(track, &synth));
    until(&mut [&mut *ann], "the plugin runs", |s| {
        s[0].track_sound(track) == TrackSound::Live
    });
    (track, plugin_device(ann, track))
}

/// Starts the commit and waits until it is either ready or given up. True if ready.
fn try_commit(ann: &mut Session, track: TrackId) -> bool {
    if !ann.start_commit(track) {
        return false;
    }
    until(&mut [&mut *ann], "the render ends", |s| {
        !matches!(s[0].commit_state(), Some(CommitState::Rendering { .. }))
    });
    matches!(ann.commit_state(), Some(CommitState::Ready { .. }))
}

#[test]
fn a_commit_whose_render_goes_wrong_is_not_sent() {
    let (_dir, server) = serve();
    let mut ann = join(&server, "ann", true);
    let mut ben = join(&server, "ben", false);
    let (track, device) = synth_track(&mut ann, &mut [&mut ben]);
    let clip = *ann
        .project()
        .track(track)
        .unwrap()
        .content
        .clips
        .keys()
        .next()
        .unwrap();
    let low = Op::NoteAdd {
        track,
        clip,
        id: ann.ids().note(),
        note: Note {
            start: Tick::beats(3),
            dur: Tick::beats(1),
            pitch: mdaw_test_plugin::CRASH_PITCH,
            vel: 100,
        },
    };
    assert!(ann.edit(vec![low]));

    // The plugin fails while the stem is rendered: nothing is committed, and Ann is told.
    ann.set_plugin_param(track, device, mdaw_test_plugin::MISBEHAVE, mdaw_test_plugin::FAIL);
    // Settings reach a plugin with its next block.
    ann.engine_mut().pump(256);
    assert!(!try_commit(&mut ann, track));
    assert!(ann.commit_state().is_none());
    assert!(
        ann.notices()
            .iter()
            .any(|n| n.contains("crashed while the commit was rendered")),
        "{:?}",
        ann.notices()
    );
    assert_eq!(ann.checkout_owner(track), Some(ann.user()), "still checked out");

    // A plugin that plays nothing: a silent stem is not committed either.
    ann.set_plugin_param(track, device, mdaw_test_plugin::MISBEHAVE, 0.0);
    ann.set_plugin_param(track, device, mdaw_test_plugin::LEVEL, 0.0);
    ann.engine_mut().pump(256);
    assert!(!try_commit(&mut ann, track));
    assert!(ann.notices().iter().any(|n| n.contains("rendered as silence")));

    // A track that runs too long for a stem is turned down before anything is rendered.
    ann.set_plugin_param(track, device, mdaw_test_plugin::LEVEL, 0.5);
    ann.engine_mut().pump(256);
    let late = Op::ClipSet {
        track,
        id: clip,
        prop: ClipProp::Start(Tick::bars(500)),
    };
    assert!(ann.edit(vec![late]));
    assert!(!ann.start_commit(track));
    assert!(
        ann.notices()
            .iter()
            .any(|n| n.contains("can be committed up to 15 minutes"))
    );

    // Put right, it commits.
    let back = Op::ClipSet {
        track,
        id: clip,
        prop: ClipProp::Start(Tick::ZERO),
    };
    assert!(ann.edit(vec![back]));
    assert!(try_commit(&mut ann, track));
    ann.finish_commit("", false);
    until(&mut [&mut ann, &mut ben], "the commit lands", |s| {
        s[1].checkout_owner(track).is_none()
    });
    assert!(ben.project().track(track).unwrap().stem.is_some());
}

/// Renders a track with an installed plugin three ways and prints their levels: live, as the
/// audio graph runs it; offline, as a commit renders its stem; and a stem file. Run by hand:
/// `MDAW_PROBE_PLUGIN=<id> MDAW_PROBE_STATE=<state blob> MDAW_PROBE_STEM=<flac> cargo test -p
/// mdaw-client --test plugins -- --ignored probe --nocapture`.
#[test]
#[ignore = "depends on the plugins installed here"]
fn probe_levels_live_offline_and_stem() {
    use mdaw_client::plugins::{Plugins, Shared};
    use mdaw_dsp::{Compiler, OfflineRender};
    let (Ok(id), Ok(state)) = (
        std::env::var("MDAW_PROBE_PLUGIN"),
        std::env::var("MDAW_PROBE_STATE"),
    ) else {
        return;
    };
    let state: Arc<[u8]> = if state == "none" {
        Arc::from(&[][..])
    } else {
        std::fs::read(state).unwrap().into()
    };
    let hash = mdaw_proto::blob_hash(&state);
    let mut ids = IdGen::new(ActorId(9));
    let (track, clip, device) = (ids.track(), ids.clip(), ids.device());
    let mut content = TrackContent::default();
    content.devices.push_back(Device::new(
        device,
        DeviceKind::Plugin {
            format: PluginFormat::Clap,
            id: id.clone(),
            state: (!state.is_empty()).then_some(hash),
        },
    ));
    let notes = (0..8)
        .map(|n| {
            let note = Note {
                start: Tick(n * 960),
                dur: Tick(240),
                pitch: 60,
                vel: 100,
            };
            (ids.note(), note)
        })
        .collect();
    let mut project = Project::default();
    apply_all(
        &mut project,
        &[
            Op::TrackAdd {
                id: track,
                shell: TrackShell {
                    name: "t".into(),
                    color: 0,
                },
                content,
                after: None,
            },
            Op::ClipAdd {
                track,
                id: clip,
                clip: Clip {
                    start: Tick::bars(1),
                    length: Tick::bars(2),
                    name: "c".into(),
                    notes,
                },
            },
        ],
    );
    let rms = |s: &[f32]| (s.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / s.len().max(1) as f64).sqrt();
    let report = |what: &str, audio: &[f32]| {
        let peak = audio.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        eprintln!("{what:28} rms {:.4}  peak {:.4}", rms(audio), peak);
    };
    let range = TickRange::new(Tick::ZERO, Tick::bars(3));

    let plugins = Shared(Rc::new(RefCell::new(Plugins::new(48_000))));
    plugins.0.borrow_mut().set_host(Some(Host::standard(false)));
    plugins.0.borrow_mut().add_state(hash, state.clone());

    // Offline, as a commit renders it.
    let audio = {
        let mut p = plugins.0.borrow_mut();
        let mut source = p.offline(48_000);
        let mut render = OfflineRender::new(&project, Some(&[track]), range, 48_000, 256, &mut source);
        let (_instances, failures) = source.finish();
        assert!(failures.is_empty(), "{failures:?}");
        while !render.step(usize::MAX) {}
        render.finish()
    };
    report("offline (the stem's render)", &audio);

    // Live: the graph with a running instance, block by block, as the audio thread does.
    let mut source = plugins.clone();
    let mut graph = Compiler::new(48_000).build_with(&project, None, &mut source);
    let mut live = Vec::new();
    let mut pos = 0i64;
    graph.chase(0);
    while live.len() < audio.len() {
        let mut out = vec![0.0f32; 256 * 2];
        graph.process(Some(pos), &mut out);
        live.extend(out);
        pos += 256;
        plugins.0.borrow_mut().idle();
    }
    report("live (the graph)", &live);

    if let Ok(stem) = std::env::var("MDAW_PROBE_STEM") {
        let decoded = mdaw_engine::flac::decode(&std::fs::read(stem).unwrap(), 900).unwrap();
        report("stem file", &decoded.samples);
    }
}
