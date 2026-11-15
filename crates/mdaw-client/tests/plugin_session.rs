//! A plugin in a session on Tracktion, through the session's own API (what was
//! `tests/plugins.rs` on the Rust engine): a plugin that failed to load is loaded again on
//! request, a plugin takes the place of the track's instrument, its parameters are offered, and
//! a setting changed here reaches the document by itself and is heard. One engine per process,
//! so one test, in steps. The test synth is copied to `target/phase5-scratch/plugin-session/`
//! and that folder is the process's `CLAP_PATH`. (How the plugin and its settings travel between
//! clients is `tests/slice.rs`.)

use mdaw_client::audio_tracktion::{TracktionAudio, TracktionOptions};
use mdaw_client::{DeviceStatus, Session, TrackSound};
use mdaw_model::{Clip, DeviceKind, Note, Op, PluginFormat, Tick, TrackId};
use std::path::{Path, PathBuf};
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

#[test]
fn a_plugin_in_a_session_on_tracktion() {
    let clap = scratch().join("clap");
    let _ = std::fs::remove_dir_all(&clap);
    mdaw_test_plugin::clap_folder(&clap);
    let synth_file = clap.join("mdaw-test-synth.clap");
    let settings = scratch().join("settings");
    let _ = std::fs::remove_dir_all(&settings);
    // SAFETY: before the engine and any other thread of this binary (it has one test).
    unsafe { std::env::set_var("CLAP_PATH", &clap) };
    let mut options = TracktionOptions::new(&settings);
    options.scan = vec![PluginFormat::Clap];
    let audio = TracktionAudio::new(options).expect("the engine starts");
    let mut session = Session::starter(audio);
    until(&mut session, "the scan ends", |s| {
        !s.scanning_plugins() && s.plugin_catalog().iter().any(|p| p.id == mdaw_test_plugin::ID)
    });
    let synth = session
        .plugin_catalog()
        .into_iter()
        .find(|p| p.id == mdaw_test_plugin::ID)
        .unwrap();
    let track = session.project().ordered_tracks().next().unwrap().0;
    let builtin = session.project().track(track).unwrap().content.devices[0].id;

    // A bar of four notes.
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

    // ---- a. the plugin's file is gone when it is to be loaded: it failed; back, it loads on
    //         request
    std::fs::rename(&synth_file, clap.join("away")).unwrap();
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
    std::fs::rename(clap.join("away"), &synth_file).unwrap();
    session.restart_plugin(track, device);
    until(&mut session, "the plugin runs after the restart", |s| {
        s.track_sound(track) == TrackSound::Live
    });

    // ---- b. a track has one instrument: the synth took the built-in one's place
    let devices = &session.project().track(track).unwrap().content.devices;
    assert_eq!(devices.len(), 1);
    assert!(devices.iter().all(|d| d.id != builtin));

    // ---- c. its parameters are offered
    let deadline = Instant::now() + Duration::from_secs(30);
    let params = loop {
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
    };
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
    let in_document = |s: &Session| match &s
        .project()
        .track(track)
        .unwrap()
        .content
        .device(device)
        .unwrap()
        .kind
    {
        DeviceKind::Plugin { state, .. } => state.is_some(),
        DeviceKind::Builtin { .. } => false,
    };
    until(&mut session, "the setting is in the document", in_document);
    println!("RESULT d: in the document after {:?}", set.elapsed());
    let [left, right] = listen(&mut session, "balance-right");
    println!("RESULT d: left={left:.5} right={right:.5}");
    assert!(
        right > 0.02 && left < 1e-4,
        "the plugin is heard on the right: {left} {right}"
    );
}
