//! How the client's engine worker schedules its jobs, through the public API of
//! `TracktionAudio`: the plugin scan does not hold up the project, slow plugin state reads do not
//! pile up, a plugin window that cannot open ends closed with a word to the user, and shutting
//! down does not wait for a scan. One engine per process, so one test, in steps.
//!
//! The test plugins (`test-gui.clap`, `test-stateless.clap`,
//! `bad-hang.clap`, native/tracktion-bridge/test/bad_clap.c) are built next to the plugin
//! scanner and copied to `target/phase5-scratch/engine-scheduling/clap`. No window is opened:
//! `DISPLAY` is removed first.

use mdaw_client::Audio;
use mdaw_client::audio::{DeviceStatus, EditorState};
use mdaw_client::audio_tracktion::{TracktionAudio, TracktionOptions};
use mdaw_model::{
    ActorId, BuiltinKind, Device, DeviceId, DeviceKind, IdGen, Op, PluginFormat, Project, ShellProp,
    TempoPoint, Tick, TrackContent, TrackId, TrackShell,
};
use mdaw_tracktion::EntityKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const GUI_PLUGIN: &str = "org.mdaw.test-gui";
const STATELESS_PLUGIN: &str = "org.mdaw.test-stateless";
/// Generous: what is checked is the order of things, not their speed.
const TIMEOUT: Duration = Duration::from_secs(60);

fn scratch() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/phase5-scratch/engine-scheduling")
}

fn fresh_dir(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
    std::fs::create_dir_all(path).unwrap();
}

fn options(settings: &Path) -> TracktionOptions {
    let mut options = TracktionOptions::new(settings);
    options.scan = vec![PluginFormat::Clap];
    options
}

fn apply(project: &mut Project, op: Op) {
    assert!(mdaw_model::apply(project, &op).is_some(), "{op:?}");
}

fn plugin(device: DeviceId, id: &str) -> Device {
    Device::new(
        device,
        DeviceKind::Plugin {
            format: PluginFormat::Clap,
            id: id.into(),
            name: id.into(),
            state: None,
        },
    )
}

fn add_track(project: &mut Project, track: TrackId, name: &str, device: Device) {
    let mut content = TrackContent::default();
    content.devices.push_back(device);
    apply(
        project,
        Op::TrackAdd {
            id: track,
            shell: TrackShell {
                name: name.into(),
                color: 0,
            },
            content,
            after: None,
        },
    );
}

/// Makes the scanner examine a plugin file again (it goes by the file's time).
fn touch(path: &Path) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(SystemTime::now()))
        .expect("touch");
}

/// Runs frames until `done`, or fails with `what`.
fn until(
    audio: &mut TracktionAudio,
    project: &Project,
    what: &str,
    mut done: impl FnMut(&mut TracktionAudio) -> bool,
) {
    let started = Instant::now();
    while !done(audio) {
        assert!(started.elapsed() < TIMEOUT, "{what}: not after {TIMEOUT:?}");
        audio.frame(project);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn set_delay(ms: u64) {
    // SAFETY: the variable exists from the start of the test (set before the first engine), so
    // only its value changes; the plugin reads it with getenv at each call.
    unsafe { std::env::set_var("MDAW_TEST_STATE_DELAY_MS", ms.to_string()) };
}

#[test]
fn engine_jobs_are_scheduled() {
    let scanner = mdaw_tracktion::built_scanner_path().expect("the build made the scanner");
    let built = scanner.parent().expect("the scanner's folder");
    let clap = scratch().join("clap");
    let settings = scratch().join("settings");
    fresh_dir(&clap);
    fresh_dir(&settings);
    for file in ["test-gui.clap", "test-stateless.clap"] {
        std::fs::copy(built.join(file), clap.join(file))
            .unwrap_or_else(|e| panic!("{file} is built next to the scanner: {e}"));
    }
    // SAFETY: before the first engine and any other thread of this binary (it has one test).
    unsafe {
        std::env::remove_var("DISPLAY");
        std::env::set_var("CLAP_PATH", &clap);
        std::env::set_var("MDAW_SCAN_TIMEOUT_MS", "6000");
        std::env::set_var("MDAW_TEST_STATE_DELAY_MS", "0");
    }

    // ---- a. a first engine scans, so the next one knows the plugins from the start
    {
        let mut audio = TracktionAudio::new(options(&settings)).expect("the engine starts");
        let empty = Project::default();
        until(&mut audio, &empty, "the first scan ends", |a| {
            !a.scanning_plugins()
        });
        assert!(
            audio.plugin_catalog().iter().any(|p| p.id == GUI_PLUGIN),
            "the scan found test-gui"
        );
    }

    // ---- b. while a plugin hangs the scanner, the project gets to the engine
    let hang = clap.join("bad-hang.clap");
    std::fs::copy(built.join("bad-hang.clap"), &hang).expect("bad-hang.clap is built next to the scanner");
    touch(&hang);
    let mut ids = IdGen::new(ActorId(7));
    let (tempo, track, device) = (ids.tempo(), ids.track(), ids.device());
    let mut project = Project::default();
    apply(
        &mut project,
        Op::TempoPut {
            id: tempo,
            point: TempoPoint {
                tick: Tick(0),
                milli_bpm: 120_000,
            },
        },
    );
    add_track(&mut project, track, "GUI", plugin(device, GUI_PLUGIN));

    let created = Instant::now();
    let mut audio = TracktionAudio::new(options(&settings)).expect("the engine starts again");
    until(&mut audio, &project, "the catalog from earlier scans", |a| {
        a.plugin_catalog().iter().any(|p| p.id == GUI_PLUGIN)
    });
    assert!(
        audio.scanning_plugins(),
        "the known plugins are there before the scan ends ({:?})",
        created.elapsed()
    );
    audio.set_project(&project);
    until(&mut audio, &project, "the test-gui device runs", |a| {
        a.device_status(&project, track, device) == DeviceStatus::Running
    });
    assert!(
        audio.scanning_plugins(),
        "the device runs while the scan goes on ({:?})",
        created.elapsed()
    );
    // More edits reach the engine during the scan.
    let (synth_track, synth) = (ids.track(), ids.device());
    apply(
        &mut project,
        Op::TrackSetShell {
            id: track,
            prop: ShellProp::Name("Renamed".into()),
        },
    );
    add_track(
        &mut project,
        synth_track,
        "4OSC",
        Device::new(
            synth,
            DeviceKind::Builtin {
                kind: BuiltinKind::TwoOsc,
                dsp_version: mdaw_model::two_osc::DSP_VERSION,
            },
        ),
    );
    audio.set_project(&project);
    let t = Instant::now();
    audio.wait_idle();
    let waited = t.elapsed();
    println!("RESULT wait_idle during the scan: {waited:?}");
    assert!(
        waited < Duration::from_secs(3),
        "wait_idle waited for the scan: {waited:?}"
    );
    assert!(
        audio.engine().has(EntityKind::Track, synth_track.0.0),
        "the new track is in the engine"
    );
    assert!(
        audio.engine().has(EntityKind::Device, synth.0.0),
        "with its synth"
    );
    assert!(audio.scanning_plugins(), "all that while the scan went on");
    until(
        &mut audio,
        &project,
        "the scan with the hanging plugin ends",
        |a| !a.scanning_plugins(),
    );
    println!("RESULT scan with a hanging plugin: {:?}", created.elapsed());
    assert_eq!(
        audio.device_status(&project, track, device),
        DeviceStatus::Running,
        "still running after the scan"
    );

    // ---- c. slow state reads of a watched plugin do not pile up
    until(&mut audio, &project, "the parameters are read", |a| {
        a.plugin_params(track, device, usize::MAX).is_some()
    });
    audio.watch(device);
    set_delay(800);
    let t = Instant::now();
    let mut deepest = 0;
    while t.elapsed() < Duration::from_secs(4) {
        audio.frame(&project);
        deepest = deepest.max(audio.queue_stats().depths.background);
        std::thread::sleep(Duration::from_millis(16));
    }
    let stats = audio.queue_stats();
    println!(
        "RESULT background depth at most {deepest}; looks: {:?}",
        stats.labels.get("look")
    );
    assert!(
        deepest <= 1,
        "one watched device: at most one look waits ({deepest})"
    );
    audio.set_plugin_param(track, device, "0", 0.25);
    let t = Instant::now();
    audio.wait_idle();
    let waited = t.elapsed();
    println!("RESULT wait_idle behind 800 ms reads: {waited:?}");
    assert!(
        waited < Duration::from_millis(2500),
        "one read running and one to follow at most: {waited:?}"
    );
    set_delay(0);
    // Without an audio device a parameter reaches the plugin with the next block rendered;
    // reading its settings for the document renders one first.
    let captured = audio.changed_states(&project, true);
    assert_eq!(captured.len(), 1, "the set changed the plugin's settings");
    audio.plugin_params(track, device, usize::MAX);
    audio.wait_idle();
    let (params, _) = audio
        .plugin_params(track, device, usize::MAX)
        .expect("the parameters are read");
    let level = params.iter().find(|p| p.id == "0").expect("the Level parameter");
    assert!(
        (level.value - 0.25).abs() < 1e-3,
        "the set was carried out: {level:?}"
    );

    // ---- d. the device tab asking for parameters again and again keeps one reading queued
    set_delay(300);
    let before = audio
        .queue_stats()
        .labels
        .get("params")
        .copied()
        .unwrap_or_default();
    let mut deepest = 0;
    for _ in 0..1000 {
        audio.plugin_params(track, device, usize::MAX);
        deepest = deepest.max(audio.queue_stats().depths.background);
        std::thread::sleep(Duration::from_millis(1));
    }
    let after = audio
        .queue_stats()
        .labels
        .get("params")
        .copied()
        .unwrap_or_default();
    println!(
        "RESULT params asked {} times, {} coalesced, {} dropped, deepest background {deepest}",
        after.submitted - before.submitted,
        after.coalesced - before.coalesced,
        after.dropped - before.dropped
    );
    assert!(deepest <= 2, "one params reading and one look at most: {deepest}");
    set_delay(0);
    audio.wait_idle();

    // ---- e. a plugin without a window: Opening, then Closed with a notice
    let (plain_track, plain) = (ids.track(), ids.device());
    add_track(
        &mut project,
        plain_track,
        "Plain",
        plugin(plain, STATELESS_PLUGIN),
    );
    audio.set_project(&project);
    until(&mut audio, &project, "the stateless device runs", |a| {
        a.device_status(&project, plain_track, plain) == DeviceStatus::Running
    });
    audio.frame(&project);
    audio
        .toggle_editor(plain_track, plain, "Plain")
        .expect("asking is fine");
    assert_eq!(audio.editor_state(plain_track, plain), EditorState::Opening);
    let mut notices = Vec::new();
    let started = Instant::now();
    while audio.editor_state(plain_track, plain) != EditorState::Closed {
        assert!(started.elapsed() < TIMEOUT, "the window request was not answered");
        assert!(!audio.editor_open(plain_track, plain), "never open");
        notices.extend(audio.frame(&project));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!audio.editor_open(plain_track, plain));
    println!("RESULT notices: {notices:?}");
    assert!(
        notices.iter().any(|n| n.contains("window did not open")),
        "the user is told: {notices:?}"
    );
    drop(audio);

    // ---- f. shutting down does not wait for a scan stuck on a hanging plugin
    touch(&hang);
    let audio = TracktionAudio::new(options(&settings)).expect("the engine starts a third time");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(audio.scanning_plugins(), "the scan waits for the hanging plugin");
    let t = Instant::now();
    drop(audio);
    let took = t.elapsed();
    println!("RESULT drop during a scan: {took:?}");
    assert!(
        took < Duration::from_secs(3),
        "dropping cancelled the scan: {took:?}"
    );
    let _ = std::fs::remove_file(&hang);
}
