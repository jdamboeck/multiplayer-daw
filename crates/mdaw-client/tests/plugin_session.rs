//! A plugin in a session on Tracktion, through the session's own API (what was
//! `tests/plugins.rs` on the Rust engine): a plugin that failed to load is loaded again on
//! request, a plugin takes the place of the track's instrument, its parameters are offered, a
//! setting changed here reaches the document by itself and is heard, and settings the plugin
//! will not take are reported while it plays on as it was. One engine per process at a time:
//! the tests take turns (`support::tracktion`). The test synth is copied to
//! `target/phase5-scratch/plugin-session/clap`, and that folder is the process's `CLAP_PATH`.
//! (How the plugin and its settings travel between clients is `tests/slice.rs`.)

mod support;

use mdaw_client::audio_tracktion::{TracktionAudio, TracktionOptions};
use mdaw_client::{DeviceStatus, PluginEntry, Session, TrackSound, Who};
use mdaw_model::{Clip, DeviceKind, Note, Op, PluginFormat, Tick, TrackId};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

fn scratch() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/phase5-scratch/plugin-session");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn until(session: &mut Session, what: &str, done: impl Fn(&Session) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done(session) {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        session.frame();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn plugin_device(session: &Session, track: TrackId) -> Option<mdaw_model::DeviceId> {
    let content = &session.project().track(track)?.content;
    content
        .devices
        .iter()
        .find(|d| matches!(d.kind, DeviceKind::Plugin { .. }))
        .map(|d| d.id)
}

/// Renders the first bar; the peak of each channel.
fn listen(session: &mut Session, name: &str) -> [f64; 2] {
    let path = scratch().join(format!("{name}.wav"));
    let _ = std::fs::remove_file(&path);
    session.frame();
    let te = session
        .audio_mut()
        .as_any_mut()
        .downcast_mut::<TracktionAudio>()
        .unwrap();
    te.wait_idle();
    session.render(&path, Tick::ZERO, Tick::bars(1)).expect("render");
    let data = std::fs::read(&path).unwrap();
    let pcm = &data[data.windows(4).position(|w| w == b"data").unwrap() + 8..];
    let mut peak = [0.0f64; 2];
    for frame in pcm.chunks_exact(6) {
        for (channel, s) in frame.chunks_exact(3).enumerate() {
            let v = (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f64 / 8_388_608.0;
            peak[channel] = peak[channel].max(v.abs());
        }
    }
    peak
}

/// The folder with the test synth, made once and put on `CLAP_PATH`.
fn clap() -> PathBuf {
    let clap = scratch().join("clap");
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = std::fs::remove_dir_all(&clap);
        mdaw_test_plugin::clap_folder(&clap);
        // SAFETY: the other test of this binary reads the environment only through `std`, which
        // takes the same lock as `set_var`; no engine (whose C++ code reads it) exists while
        // the caller holds `support::tracktion()`.
        unsafe { std::env::set_var("CLAP_PATH", &clap) };
    });
    clap
}

/// An engine without a device that looks for CLAP plugins, with settings of its own under
/// `name`. Hold `support::tracktion()` while it lives.
fn engine(name: &str) -> TracktionAudio {
    clap();
    let settings = scratch().join(name);
    let _ = std::fs::remove_dir_all(&settings);
    let mut options = TracktionOptions::new(&settings);
    options.scan = vec![PluginFormat::Clap];
    TracktionAudio::new(options).expect("the engine starts")
}

/// The test synth, once the scan has found it.
fn test_synth(session: &mut Session) -> PluginEntry {
    until(session, "the scan ends", |s| {
        !s.scanning_plugins() && s.plugin_catalog().iter().any(|p| p.id == mdaw_test_plugin::ID)
    });
    session
        .plugin_catalog()
        .into_iter()
        .find(|p| p.id == mdaw_test_plugin::ID)
        .unwrap()
}

/// A clip of four notes, one bar long, on `track`.
fn four_notes(session: &mut Session, track: TrackId) {
    let clip = session.ids().clip();
    let mut ops = vec![Op::ClipAdd {
        track,
        id: clip,
        clip: Clip {
            start: Tick::ZERO,
            length: Tick::bars(1),
            name: "c".into(),
            notes: Default::default(),
        },
    }];
    for beat in 0..4 {
        let id = session.ids().note();
        ops.push(Op::NoteAdd {
            track,
            clip,
            id,
            note: Note {
                start: Tick::beats(beat),
                dur: Tick::beats(1) - Tick(60),
                pitch: 60 + beat as u8,
                vel: 100,
            },
        });
    }
    assert!(session.edit(ops));
}

/// The parameters of a plugin device, once they are offered.
fn params_of(
    session: &mut Session,
    track: TrackId,
    device: mdaw_model::DeviceId,
) -> Vec<mdaw_client::ParamEntry> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let params = session
            .plugin_params(track, device, usize::MAX)
            .map(|(list, _)| list)
            .unwrap_or_default();
        if params.iter().any(|p| p.name == "Balance") {
            break params;
        }
        assert!(
            Instant::now() < deadline,
            "timed out reading the parameters: {params:?}"
        );
        session.frame();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Whether the document holds settings for `device`, and which.
fn state_in_document(
    session: &Session,
    track: TrackId,
    device: mdaw_model::DeviceId,
) -> Option<mdaw_model::BlobHash> {
    match &session.project().track(track)?.content.device(device)?.kind {
        DeviceKind::Plugin { state, .. } => *state,
        DeviceKind::Builtin { .. } => None,
    }
}

#[test]
fn a_plugin_in_a_session_on_tracktion() {
    let _engine = support::tracktion();
    let synth_file = clap().join("mdaw-test-synth.clap");
    let mut session = Session::starter(engine("settings"));
    let synth = test_synth(&mut session);
    let track = session.project().ordered_tracks().next().unwrap().0;
    let builtin = session.project().track(track).unwrap().content.devices[0].id;
    four_notes(&mut session, track);

    // ---- a. the plugin's file is gone when it is to be loaded: it failed; back, it loads on
    //         request
    let away = clap().join("away");
    std::fs::rename(&synth_file, &away).unwrap();
    assert!(session.add_plugin(track, &synth));
    let device = plugin_device(&session, track).expect("a plugin device");
    until(&mut session, "the device failed", |s| {
        matches!(s.device_status(track, device), DeviceStatus::Failed(_))
    });
    println!("RESULT a: {:?}", session.device_status(track, device));
    assert_eq!(session.track_sound(track), TrackSound::Failed);
    // Everything else goes on: editing, for one.
    assert!(session.edit(vec![Op::TrackSetShell {
        id: track,
        prop: mdaw_model::ShellProp::Name("after the failure".into()),
    }]));
    std::fs::rename(&away, &synth_file).unwrap();
    session.restart_plugin(track, device);
    until(&mut session, "the plugin runs after the restart", |s| {
        s.track_sound(track) == TrackSound::Live
    });

    // ---- b. a track has one instrument: the synth took the built-in one's place
    let devices = &session.project().track(track).unwrap().content.devices;
    assert_eq!(devices.len(), 1);
    assert!(devices.iter().all(|d| d.id != builtin));

    // ---- c. its parameters are offered
    let params = params_of(&mut session, track, device);
    let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
    println!("RESULT c: {names:?}");
    assert!(
        names.contains(&"Balance") && names.contains(&"Level"),
        "{names:?}"
    );

    // ---- d. all to the right: the setting reaches the document by itself (the state of the
    //         plugin, uploaded), and is heard
    let balance = params.iter().find(|p| p.name == "Balance").unwrap();
    session.set_plugin_param(track, device, &balance.id, 1.0);
    let set = Instant::now();
    until(&mut session, "the setting is in the document", |s| {
        state_in_document(s, track, device).is_some()
    });
    println!("RESULT d: in the document after {:?}", set.elapsed());
    let [left, right] = listen(&mut session, "balance-right");
    println!("RESULT d: left={left:.5} right={right:.5}");
    assert!(
        right > 0.02 && left < 1e-4,
        "the plugin is heard on the right: {left} {right}"
    );
}

/// Settings the plugin will not take (what the test synth reads as not its own) are reported in
/// a notice, and the plugin plays on with the settings it had: not with others, and not with
/// none. The settings come from the server, as another client's would.
#[test]
fn a_plugin_that_will_not_take_its_settings_is_reported_not_played_wrong() {
    let _engine = support::tracktion();
    let garbage: &[u8] = b"not a state of the test synth";
    let hash = mdaw_proto::blob_hash(garbage);
    let data = scratch().join("server");
    let _ = std::fs::remove_dir_all(&data);
    // On the server before it starts, so that it is there when the engine asks for it.
    mdaw_server::Blobs::open(data.join("blobs"))
        .and_then(|blobs| blobs.put(&hash, garbage))
        .expect("the server's blob store takes the settings");
    let server = mdaw_server::Server::start(mdaw_server::Config {
        data_dir: data,
        listen: "127.0.0.1:0".parse().unwrap(),
        host: None,
    })
    .unwrap();
    let who = Who {
        user_key: *b"refused-settings",
        name: "ann".into(),
    };
    let audio = engine("refused-settings");
    let mut session = Session::join(server.join_info().clone(), who, audio, Arc::new(|| {})).unwrap();
    let synth = test_synth(&mut session);
    let track = session.ids().track();
    assert!(session.edit(vec![Op::TrackAdd {
        id: track,
        shell: mdaw_model::TrackShell {
            name: "Synth".into(),
            color: 0,
        },
        content: Default::default(),
        after: None,
    }]));
    four_notes(&mut session, track);
    assert!(session.add_plugin(track, &synth));
    let device = plugin_device(&session, track).expect("a plugin device");
    until(&mut session, "the plugin runs", |s| {
        s.track_sound(track) == TrackSound::Live
    });

    // Settings of its own first, which the document gets: all to the right.
    let balance = params_of(&mut session, track, device)
        .into_iter()
        .find(|p| p.name == "Balance")
        .unwrap();
    session.set_plugin_param(track, device, &balance.id, 1.0);
    until(
        &mut session,
        "its settings are in the document and confirmed",
        |s| state_in_document(s, track, device).is_some() && s.unconfirmed() == 0,
    );
    let [left, right] = listen(&mut session, "own-settings");
    assert!(right > 0.02 && left < 1e-4, "heard on the right: {left} {right}");

    // Settings it will not take.
    assert!(session.edit(vec![Op::DeviceSetState {
        track,
        id: device,
        state: Some(hash),
    }]));
    until(&mut session, "the user is told", |s| {
        s.notices()
            .iter()
            .any(|n| n.contains("did not take its settings"))
    });
    println!("RESULT: {:?}", session.notices());
    assert_eq!(session.device_status(track, device), DeviceStatus::Running);
    let [left, right] = listen(&mut session, "refused-settings");
    println!("RESULT: left={left:.5} right={right:.5}");
    assert!(
        right > 0.02 && left < 1e-4,
        "it plays on with its own settings, on the right: {left} {right}"
    );
}
